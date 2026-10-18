#![allow(warnings)]

use axum::response::Html;
use reqwest;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use axum::{
    Json, Router, ServiceExt,
    body::Body,
    extract::{FromRequestParts, Request},
    http::{StatusCode, request::Parts},
    middleware,
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_anyhow::{ApiError, ApiResult, on_error, set_expose_errors};
pub mod result_ext;
use chrono::{Duration, Utc, prelude::*};
use config;
use futures::future::BoxFuture;
use futures_util::StreamExt;
use http::Uri;
use itertools::Itertools;
use remux_utils::Store;
use reqwest::header::LOCATION;
pub use result_ext::{IntoApiError, OptionExt, ResultExt};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::json;
use std::{self, collections::HashMap, env, fs, path::Path, sync::Arc};
use timed;
use tower::{Layer, util::MapRequestLayer};
use tower_http::{
    cors::{Any, CorsLayer},
    services::{ServeDir, ServeFile},
};
use tracing::{self, debug, error, info, instrument, warn};
use tracing_subscriber::{
    EnvFilter, Layer as TracingLayer, fmt, layer::SubscriberExt,
    util::SubscriberInitExt,
};
use url::Url;
use uuid::Uuid;

mod conversions;
pub mod device_profile;
mod errors;
mod keyed_lock;
pub mod sdks {
    pub use remux_sdks::*;
}
mod addons;
pub mod api;
mod common;
pub mod jellyfin_client;
pub use common::stable_media_uuid;
pub mod db;
#[cfg(feature = "desktop")]
pub mod embedded_static;
pub mod games;
pub mod intro;
mod iptv;
pub mod localization;
pub mod logging;
pub mod playback;
pub mod playback_session;
pub mod services;
pub mod signals;
pub mod stream;
mod subtitle_selection;
pub mod tasks;
mod torrent;
mod web_client;
mod web_patches;
mod web_transform;
mod ws;

/// Paths to web assets served from the filesystem (non-desktop builds).
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FilesystemPaths {
    #[serde(default = "default_web_path")]
    pub web_path: String,
    #[serde(default = "default_dashboard_path")]
    pub dashboard_path: String,
    /// The Remux web client, served at `/remux/`. Optional: without it the
    /// server runs Jellyfin Web only.
    #[serde(default = "default_remux_web_path")]
    pub remux_web_path: String,
}

impl Default for FilesystemPaths {
    fn default() -> Self {
        Self {
            web_path: default_web_path(),
            dashboard_path: default_dashboard_path(),
            remux_web_path: default_remux_web_path(),
        }
    }
}

impl FilesystemPaths {
    pub fn load_from_env() -> Self {
        let mut paths = Self::default();
        if let Ok(v) = std::env::var("WEB_PATH") {
            paths.web_path = v;
        }
        if let Ok(v) = std::env::var("DASHBOARD_PATH") {
            paths.dashboard_path = v;
        }
        if let Ok(v) = std::env::var("REMUX_WEB_PATH") {
            paths.remux_web_path = v;
        }
        paths
    }
}

/// Opaque service type for the `/admin` static file handler.
pub type AdminService = tower::util::BoxCloneSyncService<
    axum::extract::Request,
    axum::response::Response,
    std::convert::Infallible,
>;

/// Build an `AdminService` that serves dashboard files from the filesystem.
/// Does not inject user custom JS (admin panel is remux's own UI).
pub fn admin_from_filesystem(dashboard_path: &str) -> AdminService {
    let index = format!("{dashboard_path}/index.html");
    tower::util::BoxCloneSyncService::new(
        web_transform::TransformLayer::new(None)
            .layer(ServeDir::new(dashboard_path).fallback(ServeFile::new(index))),
    )
}

pub use web_client::{RemuxWeb, WebClientService};

/// Route auto-registration via `#[get("/path")]`, `#[post("/path")]`, etc.
pub struct RouteRegistration(pub fn(axum::Router<AppState>) -> axum::Router<AppState>);
inventory::collect!(RouteRegistration);

pub fn collect_routes() -> axum::Router<AppState> {
    let mut router = axum::Router::new();
    for entry in inventory::iter::<RouteRegistration> {
        router = (entry.0)(router);
    }
    router
}

pub async fn init_app_with_config(config: Config) -> Result<Router> {
    let paths = FilesystemPaths::default();
    let admin = admin_from_filesystem(
        &paths
            .dashboard_path
            .clone(),
    );
    let web_path = paths
        .web_path
        .clone();
    let remux_web = RemuxWeb::from_filesystem(&paths.remux_web_path);
    let (router, _ctx) =
        init_app(config, Some(paths), admin, Some(remux_web), move |pool| {
            WebClientService::from_filesystem(&web_path, pool)
        })
        .await?;
    Ok(router)
}

pub async fn init_app_with_ctx(config: Config) -> Result<(Router, AppContext)> {
    let paths = FilesystemPaths::default();
    let admin = admin_from_filesystem(
        &paths
            .dashboard_path
            .clone(),
    );
    let web_path = paths
        .web_path
        .clone();
    let remux_web = RemuxWeb::from_filesystem(&paths.remux_web_path);
    init_app(config, Some(paths), admin, Some(remux_web), move |pool| {
        WebClientService::from_filesystem(&web_path, pool)
    })
    .await
}

/// Start the HTTP server with web assets served from the filesystem.
/// Binds to `0.0.0.0:{port}` (default 3000, or `PORT` env var).
pub async fn serve(config: Config, paths: FilesystemPaths) -> Result<()> {
    let admin = admin_from_filesystem(
        &paths
            .dashboard_path
            .clone(),
    );
    let web_path = paths
        .web_path
        .clone();
    let addr = std::net::SocketAddr::new(config.host, config.port);
    let remux_web = RemuxWeb::from_filesystem(&paths.remux_web_path);
    let (router, ctx) =
        init_app(config, Some(paths), admin, Some(remux_web), move |pool| {
            WebClientService::from_filesystem(&web_path, pool)
        })
        .await?;
    spawn_games_warmup(&ctx);
    bind_and_serve(router, addr).await
}

/// Scans the games library and loads its metadata databases in the
/// background, so the first game opened after a restart already has its
/// genre, description and the rest. Never blocks a request; does nothing when
/// games are not active. Part of starting a server (not of building the app),
/// so tests that arrange their library after boot are unaffected.
pub fn spawn_games_warmup(ctx: &AppContext) {
    // Clear leftovers of an install interrupted by a restart (cores are
    // independent of the games feature flag).
    let cleanup_ctx = ctx.clone();
    tokio::spawn(async move {
        cleanup_ctx
            .cores
            .cleanup_stale(&cleanup_ctx.config)
            .await;
    });
    if !games::is_active(&ctx.config) {
        return;
    }
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let snap = ctx
            .games
            .snapshot(&ctx.config)
            .await;
        ctx.game_meta
            .warm(&ctx.config, &snap)
            .await;
    });
}

pub async fn bind_and_serve(router: Router, addr: std::net::SocketAddr) -> Result<()> {
    let app = MapRequestLayer::new(rewrite_request_uri).layer(router);
    info!("starting webserver at {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

/// Loads `Config` from `env`, layered over the compiled-in defaults. Shared by
/// every binary embedding this crate (the headless server binary, the desktop
/// tray app) so they all resolve settings — `HOST`/`PORT`/etc. — the same way,
/// rather than each reimplementing (or skipping) the config/env merge.
pub fn load_config(
    env: config::Environment,
) -> std::result::Result<Config, config::ConfigError> {
    config::Config::builder()
        .add_source(
            env.try_parsing(true)
                .list_separator(",")
                .with_list_parse_key("games_roots"),
        )
        .build()?
        .try_deserialize()
}

/// [`load_config`] against the process environment — the common case for a
/// binary with no need to inject a custom source (tests aside).
pub fn load_config_from_env() -> std::result::Result<Config, config::ConfigError> {
    load_config(config::Environment::default())
}

#[cfg(unix)]
const TARGET_OPEN_FILE_LIMIT: libc::rlim_t = 8192;

#[cfg(unix)]
fn desired_open_file_limit(
    current_soft: libc::rlim_t,
    current_hard: libc::rlim_t,
) -> Option<libc::rlim_t> {
    let desired = TARGET_OPEN_FILE_LIMIT.min(current_hard);
    (current_soft < desired).then_some(desired)
}

#[cfg(unix)]
fn raise_open_file_limit() {
    // Torrent peers, tracker sockets, HTTP clients, and transcoders all share
    // the process descriptor table. Some desktop launch environments provide a
    // soft limit too small for concurrent playback, so raise only the soft
    // limit while preserving the administrator-controlled hard limit.
    unsafe {
        let mut limits = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        if libc::getrlimit(libc::RLIMIT_NOFILE, limits.as_mut_ptr()) != 0 {
            warn!(
                error = %std::io::Error::last_os_error(),
                "failed to read open-file limit"
            );
            return;
        }

        let mut limits = limits.assume_init();
        let Some(desired) = desired_open_file_limit(limits.rlim_cur, limits.rlim_max)
        else {
            return;
        };
        let previous = limits.rlim_cur;
        limits.rlim_cur = desired;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &limits) != 0 {
            warn!(
                previous,
                desired,
                hard = limits.rlim_max,
                error = %std::io::Error::last_os_error(),
                "failed to raise open-file limit"
            );
        }
    }
}

#[cfg(not(unix))]
fn raise_open_file_limit() {}

pub async fn init_app(
    config: Config,
    web_paths: Option<FilesystemPaths>,
    admin: AdminService,
    remux_web: Option<RemuxWeb>,
    make_web_client: impl FnOnce(sqlx::SqlitePool) -> WebClientService,
) -> Result<(Router, AppContext)> {
    raise_open_file_limit();
    info!("starting remux {}", env!("CARGO_PKG_VERSION"));
    info!("config: {}", serde_json::to_string_pretty(&config).unwrap());

    let conn = db::connect(
        config
            .database_url
            .as_deref()
            .expect("Config::resolve() must be called before init_app"),
        config.slow_query_threshold_ms,
    )
    .await?;

    info!("Running database migrations. Do not interrupt!");
    db::migrate(&conn).await?;
    info!("migrations complete");

    // Checkpoint the WAL before accepting any requests. At this point no
    // other readers exist, so TRUNCATE is guaranteed to succeed and the WAL
    // is cleared to zero — preventing large WALs left over from previous
    // write-heavy tasks (metadata refresh, library scan) from slowing down
    // the first queries after a restart.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&conn)
        .await
        .ok();
    crate::db::Settings::init_server_id(&conn).await?;

    // Probe hardware and persist results at startup.
    // vaapi_driver is always re-detected (regardless of auto_detect) because
    // it is a runtime property of the host, not a user preference.
    {
        let mut enc_opts = db::Settings::get_encoding_config(&conn).await?;
        if enc_opts
            .auto_detect_hardware_acceleration
            .unwrap_or(true)
        {
            let detected =
                crate::playback::engine::detect_hardware_acceleration().await;
            enc_opts.hardware_acceleration_type = Some(detected);
        }
        let device = enc_opts
            .vaapi_device
            .as_deref()
            .unwrap_or("/dev/dri/renderD128");
        let driver = crate::playback::engine::detect_vaapi_driver(device).await;
        let opencl =
            crate::playback::engine::detect_opencl_tonemap(device, &driver).await;
        tracing::info!(opencl_tonemap = opencl, "OpenCL tone mapping probed");
        crate::playback::hw_accel::set_opencl_tonemap_available(opencl);
        enc_opts.vaapi_driver = Some(driver);
        db::Settings::set_encoding_config(&conn, &enc_opts).await?;
    }

    let saved_config = db::Settings::get_config(&conn).await?;

    let p2p_enabled = saved_config
        .p2p_enabled
        .unwrap_or(true);
    let torrent_mgr = if p2p_enabled {
        let mgr = torrent::TorrentManager::from_config(&config).await?;
        mgr.update_limits(
            saved_config
                .p2p_upload_speed_kbps
                .unwrap_or(0),
            saved_config
                .p2p_download_speed_kbps
                .unwrap_or(0),
        );
        Some(mgr)
    } else {
        None
    };

    let web_client = make_web_client(conn.clone());
    // The Remux client has its own UI, so no branding scripts are injected.
    let remux_web_installed = remux_web
        .as_ref()
        .is_some_and(|r| r.installed);
    let remux_web = remux_web.map(|r| r.service);

    let addons = addons::AddonService::from_db(&conn, &config).await?;
    let transcode_sessions_dir = resolve_transcode_dir(&config.data_dir);
    let (ws_tx, _) = tokio::sync::broadcast::channel::<ws::WsEvent>(128);
    let mut signals = signals::Signals::default();
    signals.register(ws::WebSocketSubscriber {
        ws_tx: ws_tx.clone(),
    });

    let mut ctx = AppContext {
        config,
        db: conn.clone(),
        store: Store::new_weighted(128 * 1024 * 1024),
        sessions: playback_session::PlaybackSessionManager::new(transcode_sessions_dir),
        torrent: Arc::new(tokio::sync::RwLock::new(torrent_mgr.map(Arc::new))),
        ws_tx,
        default_web_client: Arc::new(tokio::sync::RwLock::new(
            web_client::normalize_web_client(saved_config.default_web_client)
                .as_str()
                .to_string(),
        )),
        web_paths,
        remux_web_installed,
        addons,
        signals,
        started_at: Utc::now(),
        games: games::GamesIndex::default(),
        game_thumbs: games::thumbs::ThumbService::default(),
        game_meta: games::meta::MetaService::default(),
        cores: games::cores::CoresService::default(),
        moonfin_settings: api::moonfin_settings::SettingsHub::default(),
        mdblist: api::moonfin_mdblist::MdbListService::default(),
    };
    ctx.signals
        .register(services::media_tracker::MediaTrackerSubscriber { ctx: ctx.clone() });
    ctx.signals
        .register(api::webhooks::WebhookSubscriber { ctx: ctx.clone() });
    ctx.signals
        .register(services::four_k_capability::FourKCapabilitySubscriber::new(
            ctx.clone(),
        ));

    api::moonfin_settings::import_defaults_file(&ctx).await;

    // Sync intro items at startup (best-effort; errors are logged not fatal).
    if let Err(e) = intro::sync_intros(&ctx).await {
        warn!(err = ?e, "intro sync failed at startup");
    }

    // Kill idle sessions after 15 minutes of no activity.
    // Pings keep active sessions alive indefinitely.
    ctx.sessions
        .clone()
        .spawn_cleanup_task(
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60 * 15),
        );

    db::StreamGroup::migrate_from_settings(&conn).await;

    let task_service = tasks::TaskService::new(ctx.clone()).await?;

    task_service
        .start()
        .await?;
    if !ctx
        .config
        .disable_startup_tasks
    {
        task_service
            .run_startup_tasks()
            .await?;
    }

    let state = AppState {
        ctx: ctx.clone(),
        tasks: task_service,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    let base = Router::new()
        .route("/websocket", get(ws::ws_handler))
        .route("/socket", get(ws::ws_handler))
        .route("/", get(web_client::root_redirect))
        .route("/serviceworker.js", get(web_client::root_serviceworker))
        .merge(collect_routes());

    let mut router = base
        .nest_service("/admin", admin)
        .nest_service("/web", web_client.clone())
        .nest_service("/jellyfin", web_client);
    if let Some(remux_web) = remux_web {
        router = router.nest_service("/remux", remux_web);
    }
    let router = router.with_state(state);

    let router = router
        .layer(on_error(log_api_error))
        .layer(
            tower_http::trace::TraceLayer::new_for_http()
                .make_span_with(|_request: &axum::http::Request<axum::body::Body>| {
                    // let uri = _request.uri();
                    // let path = uri.path();
                    // let uri_str = match uri.query() {
                    //     Some(q) => format!("{path}?{q}"),
                    //     None => path.to_string(),
                    // };
                    tracing::info_span!("request", user = tracing::field::Empty)
                })
                .on_request(|request: &axum::http::Request<axum::body::Body>, _span: &tracing::Span| {
                    let full = logging::loggable_uri(request.uri());
                    debug!(target: "remux_server::request", method = %request.method(), uri = %full, "→");
                })
                .on_response(|response: &axum::http::Response<axum::body::Body>, latency: std::time::Duration, _span: &tracing::Span| {
                    debug!(target: "remux_server::request", status = %response.status().as_u16(), latency_ms = %latency.as_millis(), "←");
                })
                .on_failure(()),
        )
        .layer(cors);

    Ok((router, ctx))
}

#[derive(Clone)]
pub struct AppContext {
    pub config: Config,
    pub db: sqlx::SqlitePool,
    pub store: Store,
    pub sessions: playback_session::PlaybackSessionManager,
    pub torrent: Arc<tokio::sync::RwLock<Option<Arc<torrent::TorrentManager>>>>,
    pub ws_tx: tokio::sync::broadcast::Sender<ws::WsEvent>,
    pub default_web_client: Arc<tokio::sync::RwLock<String>>,
    /// Present in filesystem builds; `None` in desktop (assets are embedded).
    pub web_paths: Option<FilesystemPaths>,
    /// Whether Remux Web is served at `/remux` with an `index.html` to open.
    pub remux_web_installed: bool,
    pub addons: addons::AddonService,
    pub signals: signals::Signals,
    /// When this server process started.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Lazily-scanned index of retro games (see [`games`]).
    pub games: games::GamesIndex,
    /// Libretro box-art fetcher/cache for the games endpoints.
    pub game_thumbs: games::thumbs::ThumbService,
    /// libretro-database metadata lookups for game details.
    pub game_meta: games::meta::MetaService,
    /// Install state of the server-hosted EmulatorJS bundle.
    pub cores: games::cores::CoresService,
    /// Moonfin settings-sync state (SSE fan-out, admin defaults cache).
    pub moonfin_settings: api::moonfin_settings::SettingsHub,
    /// MDBList ratings proxy state (HTTP client, in-flight and failure memory).
    pub mdblist: api::moonfin_mdblist::MdbListService,
}

impl AppContext {
    /// Gracefully shut down background services (torrent DHT, etc.).
    /// Call this when the server is stopping to release sockets immediately.
    pub async fn shutdown(&self) {
        let old = self
            .torrent
            .write()
            .await
            .take();
        if let Some(mgr) = old {
            mgr.shutdown()
                .await;
        }
    }

    pub async fn set_p2p_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        let current = self
            .torrent
            .read()
            .await
            .clone();
        match (enabled, current.is_some()) {
            (true, false) => {
                let mgr =
                    Arc::new(torrent::TorrentManager::from_config(&self.config).await?);
                *self
                    .torrent
                    .write()
                    .await = Some(mgr);
            }
            (false, true) => {
                let old = self
                    .torrent
                    .write()
                    .await
                    .take();
                if let Some(mgr) = old {
                    mgr.shutdown()
                        .await;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct AppState {
    pub ctx: AppContext,
    pub tasks: tasks::TaskService,
}

fn default_data_dir() -> std::path::PathBuf {
    dirs::data_dir()
        .map(|d| d.join("remux"))
        .unwrap_or_else(|| std::path::PathBuf::from("/data"))
}

fn default_web_path() -> String {
    default_data_dir()
        .join("jellyfin-web")
        .to_str()
        .map(str::to_owned)
        .unwrap_or_else(|| "/data/jellyfin-web".to_string())
}

fn default_remux_web_path() -> String {
    default_data_dir()
        .join("remux-web")
        .to_str()
        .map(str::to_owned)
        .unwrap_or_else(|| "/data/remux-web".to_string())
}

fn default_dashboard_path() -> String {
    default_data_dir()
        .join("dashboard")
        .to_str()
        .map(str::to_owned)
        .unwrap_or_else(|| "/data/dashboard".to_string())
}

fn default_host() -> std::net::IpAddr {
    std::net::Ipv4Addr::UNSPECIFIED.into()
}

fn default_port() -> u16 {
    3000
}

fn default_torrent_http_port() -> u16 {
    9876
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Config {
    #[serde(default = "default_data_dir")]
    pub data_dir: std::path::PathBuf,
    /// `None` means derive from `data_dir` — call `resolve()` after loading.
    pub database_url: Option<String>,
    /// `None` means derive from `data_dir` — call `resolve()` after loading.
    pub torrent_data_dir: Option<String>,
    /// Address the HTTP server listens on. Defaults to every IPv4 interface;
    /// `127.0.0.1` keeps it behind a reverse proxy on the same host, `::`
    /// listens on IPv6 as well.
    #[serde(default = "default_host")]
    pub host: std::net::IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Explicit port for the internal torrent HTTP server.
    /// When absent the OS picks a free ephemeral port.
    #[serde(default = "default_torrent_http_port_opt")]
    pub torrent_http_port: Option<u16>,
    /// Log queries that exceed this threshold in milliseconds. Defaults to 10 000 ms.
    #[serde(default = "default_slow_query_threshold_ms")]
    pub slow_query_threshold_ms: u64,
    /// Disable the DHT gossip socket. Useful when no Torznab sources are
    /// configured or when running in a restricted network environment.
    #[serde(default)]
    pub disable_dht: bool,
    /// Skip running `StartupTrigger`-tagged tasks (e.g. the default library
    /// refresh) during `init_app`. Only meant for test/benchmark harnesses
    /// that bulk-write their own fixture data immediately after startup —
    /// letting the real startup tasks run concurrently races the same
    /// tables and can corrupt fixture writes.
    #[serde(default)]
    pub disable_startup_tasks: bool,
    /// TCP port range for librqbit peer connections.  Announced to trackers so
    /// they return us in peer lists.  Defaults to 6881.  Does not need to be
    /// forwarded/open for outbound-only operation, but must be a real port
    /// (not 0) or many trackers will reject the announce.
    #[serde(default = "default_torrent_peer_port")]
    pub torrent_peer_port: Option<u16>,
    /// Path to the bgutil-pot binary used by yt-dlp for YouTube POT token generation.
    #[serde(default = "default_bgutil_script_path")]
    pub bgutil_script_path: std::path::PathBuf,
    /// Base URL for the TMDB API. Overridable for testing.
    #[serde(default = "default_tmdb_base_url")]
    pub tmdb_base_url: String,
    /// Base URL for remuxdb. When set, probe results are submitted after each live probe.
    #[serde(default = "default_remuxdb_url")]
    pub remuxdb_url: Option<String>,
    #[serde(default = "default_activity_log_retention_days")]
    pub activity_log_retention_days: u32,
    #[serde(default = "default_jellyfin_version")]
    pub jellyfin_version: String,
    /// Log file output is best-effort; stdout logging remains enabled when false.
    #[serde(default = "default_log_file_enabled")]
    pub log_file_enabled: bool,
    /// Optional log directory; defaults to `<data_dir>/logs` after `resolve()`.
    #[serde(default)]
    pub log_dir: Option<std::path::PathBuf>,
    /// Maximum number of rolling log files retained. Zero disables the count cap.
    #[serde(default = "default_log_file_max_files")]
    pub log_file_max_files: usize,
    /// Age-based log retention in days; zero disables age-based cleanup.
    #[serde(default = "default_log_file_retention_days")]
    pub log_file_retention_days: u32,
    /// OTLP gRPC endpoint (e.g. `http://jaeger-collector:4317`) to export
    /// tracing spans to. `None` (the default) disables tracing export
    /// entirely — normal log output is unaffected either way.
    pub otlp_endpoint: Option<String>,
    /// Enables the retro-games feature (synthetic Games view and, later, the
    /// `/moonfin/games` endpoints). Off by default.
    #[serde(default)]
    pub games_enabled: bool,
    /// Root directories scanned for games, laid out as
    /// `<root>/<System>/[<Game>/]<rom>`. As an environment variable this is a
    /// comma-separated list (`GAMES_ROOTS=/roms,/more-roms`).
    #[serde(default)]
    pub games_roots: Vec<String>,
    /// Largest ROM, in bytes, the server will unpack from a `.zip`/`.7z`
    /// archive (non-arcade cores). Bigger ROMs answer 413.
    #[serde(default = "default_games_archive_max_extract_bytes")]
    pub games_archive_max_extract_bytes: u64,
    /// Longest a library scan may run, in seconds, before the server gives up
    /// waiting for it and keeps serving the previous game list.
    #[serde(default = "default_games_scan_timeout_secs")]
    pub games_scan_timeout_secs: u64,
    /// Display name of the synthetic Games view.
    #[serde(default = "default_games_library_name")]
    pub games_library_name: String,
    /// Per-user cap on total bytes of stored game saves.
    #[serde(default = "default_games_saves_max_bytes_per_user")]
    pub games_saves_max_bytes_per_user: u64,
    /// Per-user cap on the number of stored game save files.
    #[serde(default = "default_games_saves_max_files_per_user")]
    pub games_saves_max_files_per_user: u32,
    /// Serve game box art/snaps/titles fetched (and cached) from the libretro
    /// thumbnail server. Set to false to never make outbound requests.
    #[serde(default = "default_games_thumbnails_enabled")]
    pub games_thumbnails_enabled: bool,
    /// Origin of the libretro-style thumbnail server
    /// (`<base>/<Platform>/Named_Boxarts/<Name>.png`).
    #[serde(default = "default_games_thumbnails_base_url")]
    pub games_thumbnails_base_url: String,
    /// Fill game detail (genre, developer, publisher, franchise, region,
    /// year, players) from the libretro database, downloaded per system and
    /// cached. Set to false to never make these outbound requests.
    #[serde(default = "default_games_metadata_enabled")]
    pub games_metadata_enabled: bool,
    /// Origin of the libretro database `.rdb` files
    /// (`<base>/<Platform>.rdb`), or a local directory holding them. The
    /// `raw.githubusercontent.com/libretro/libretro-database/master/rdb`
    /// origin is a fresher alternative to the default CDN.
    #[serde(default = "default_games_metadata_rdb_base_url")]
    pub games_metadata_rdb_base_url: String,
    /// Days before a cached `.rdb` is revalidated upstream (ETag /
    /// Last-Modified, so an unchanged file is not downloaded again).
    #[serde(default = "default_games_metadata_refresh_days")]
    pub games_metadata_refresh_days: u32,
    /// Seconds to wait after a failed database download before trying again.
    #[serde(default = "default_games_metadata_retry_secs")]
    pub games_metadata_retry_secs: u64,
    /// Largest ROM (bytes) hashed for CRC matching; bigger files (disc
    /// images) are matched by name only.
    #[serde(default = "default_games_metadata_max_hash_bytes")]
    pub games_metadata_max_hash_bytes: u64,
    /// Opt-in: fill game descriptions and ratings from the LaunchBox Games
    /// Database (`Metadata.zip`, ~108 MB, downloaded in the background and
    /// reduced to a small local index). Off by default: LaunchBox publishes no
    /// terms for the download, so enabling it makes you responsible for them.
    #[serde(default)]
    pub games_launchbox_enabled: bool,
    /// Where to fetch the LaunchBox `Metadata.zip` (an http(s) URL), or the
    /// path of a local copy of it.
    #[serde(default = "default_games_launchbox_url")]
    pub games_launchbox_url: String,
    /// Days before the LaunchBox download is revalidated (ETag, so an
    /// unchanged archive is not fetched again).
    #[serde(default = "default_games_launchbox_refresh_days")]
    pub games_launchbox_refresh_days: u32,
    /// Override for the EmulatorJS bundle archive (7z or zip) the server
    /// installs from `POST /Moonfin/Games/Cores/Install`. Defaults to the
    /// pinned release. Requires `games_cores_sha256`; unset means the pinned
    /// release is used.
    #[serde(default)]
    pub games_cores_url: Option<String>,
    /// Expected SHA-256 (64 hex digits) of the `games_cores_url` archive.
    #[serde(default)]
    pub games_cores_sha256: Option<String>,
    /// Expected byte length of the `games_cores_url` archive (optional).
    #[serde(default)]
    pub games_cores_size: Option<u64>,
    /// Largest cores archive accepted, download or upload, in bytes.
    #[serde(default = "default_games_cores_max_download_bytes")]
    pub games_cores_max_download_bytes: u64,
    /// Most bytes extracted from a cores archive (zip-bomb guard).
    #[serde(default = "default_games_cores_max_extract_bytes")]
    pub games_cores_max_extract_bytes: u64,
    /// Optional JSON file with admin default Moonfin settings (a flat profile
    /// object). Only a one-time seed: at startup, if the database holds no
    /// defaults yet, the file is imported into it; after that the database
    /// (edited through `/Moonfin/Admin/Defaults`) is the only source and the
    /// file is ignored. A missing or invalid file is skipped with a warning.
    #[serde(default)]
    pub moonfin_default_settings_path: Option<String>,
    /// Directory holding the extracted Moonfin web bundle served at
    /// `/Moonfin/Web`. Defaults to `<data_dir>/moonfin-web` when unset. A
    /// missing directory means the hosted web app answers 404.
    #[serde(default)]
    pub moonfin_web_path: Option<String>,
    /// `defaultServerUrl` advertised by `/Moonfin/Web/config.json` (pre-fills
    /// the Add Server box). Defaults to the request origin.
    #[serde(default)]
    pub moonfin_web_default_server_url: Option<String>,
    /// `forcedServerUrl` advertised by `/Moonfin/Web/config.json`; locks the
    /// hosted app to one server. A trailing `/web` or `/web/index.html` is
    /// stripped.
    #[serde(default)]
    pub moonfin_web_forced_server_url: Option<String>,
    /// `enableWebRtcScan` advertised by `/Moonfin/Web/config.json`.
    #[serde(default)]
    pub moonfin_web_enable_webrtc_scan: bool,
    /// Server-wide MDBList API key (`MDBLIST_API_KEY`). Powers the Moonfin
    /// ratings proxy for every user; users may also supply their own key in
    /// their synced settings. Never logged or serialized.
    #[serde(default)]
    pub mdblist_api_key: Option<Secret>,
    /// Origin of the MDBList API (overridable for tests).
    #[serde(default = "default_mdblist_base_url")]
    pub mdblist_base_url: String,
    /// How long fetched MDBList ratings stay fresh, in hours (Moonbase: 7 days).
    #[serde(default = "default_mdblist_cache_ttl_hours")]
    pub mdblist_cache_ttl_hours: u64,
}

/// A credential that never reveals itself through `Debug` or `Serialize`
/// (the startup banner logs the whole [`Config`]).
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value; only hand this to the service it authenticates with.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("<redacted>")
    }
}

fn default_mdblist_base_url() -> String {
    "https://api.mdblist.com".to_string()
}

fn default_mdblist_cache_ttl_hours() -> u64 {
    24 * 7
}

fn default_games_archive_max_extract_bytes() -> u64 {
    games::archive::DEFAULT_MAX_EXTRACT_BYTES
}

fn default_games_saves_max_bytes_per_user() -> u64 {
    512 * 1024 * 1024
}

fn default_games_saves_max_files_per_user() -> u32 {
    2000
}

fn default_games_thumbnails_enabled() -> bool {
    true
}

fn default_games_thumbnails_base_url() -> String {
    "https://thumbnails.libretro.com".to_string()
}

fn default_games_metadata_enabled() -> bool {
    true
}

fn default_games_metadata_rdb_base_url() -> String {
    "https://cdn.jsdelivr.net/gh/libretro/libretro-database@master/rdb".to_string()
}

fn default_games_metadata_refresh_days() -> u32 {
    30
}

fn default_games_metadata_retry_secs() -> u64 {
    300
}

fn default_games_metadata_max_hash_bytes() -> u64 {
    64 * 1024 * 1024
}

fn default_games_launchbox_url() -> String {
    "https://gamesdb.launchbox-app.com/Metadata.zip".to_string()
}

fn default_games_launchbox_refresh_days() -> u32 {
    30
}

fn default_games_cores_max_download_bytes() -> u64 {
    400 * 1024 * 1024
}

fn default_games_cores_max_extract_bytes() -> u64 {
    1024 * 1024 * 1024
}

fn default_games_scan_timeout_secs() -> u64 {
    120
}

fn default_games_library_name() -> String {
    "Games".to_string()
}

fn default_jellyfin_version() -> String {
    "10.11.8".to_string()
}

fn default_remuxdb_url() -> Option<String> {
    Some("https://remuxdb.1632022.xyz".to_string())
}

fn default_activity_log_retention_days() -> u32 {
    90
}

fn default_log_file_enabled() -> bool {
    true
}

fn default_log_file_max_files() -> usize {
    5
}

fn default_log_file_retention_days() -> u32 {
    5
}

fn default_tmdb_base_url() -> String {
    "https://api.themoviedb.org/3/".to_string()
}

fn default_bgutil_script_path() -> std::path::PathBuf {
    std::path::PathBuf::from("/usr/local/bin/bgutil-pot")
}

fn default_slow_query_threshold_ms() -> u64 {
    10_000
}

fn ensure_writable_directory(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    let _probe = tempfile::NamedTempFile::new_in(path)?;
    Ok(())
}

fn select_transcode_dir(
    primary: std::path::PathBuf,
    fallback: std::path::PathBuf,
) -> std::path::PathBuf {
    let primary_error = match ensure_writable_directory(&primary) {
        Ok(()) => return primary,
        Err(error) => error,
    };

    match ensure_writable_directory(&fallback) {
        Ok(()) => {
            warn!(
                primary = %primary.display(),
                fallback = %fallback.display(),
                error = %primary_error,
                "transcode directory is not writable; using temporary storage"
            );
            fallback
        }
        Err(fallback_error) => {
            warn!(
                primary = %primary.display(),
                fallback = %fallback.display(),
                primary_error = %primary_error,
                fallback_error = %fallback_error,
                "transcode and fallback directories are not writable"
            );
            primary
        }
    }
}

fn resolve_transcode_dir(data_dir: &std::path::Path) -> std::path::PathBuf {
    select_transcode_dir(
        data_dir.join("transcode_sessions"),
        std::env::temp_dir().join("remux-transcode"),
    )
}

fn default_torrent_http_port_opt() -> Option<u16> {
    Some(default_torrent_http_port())
}

fn default_torrent_peer_port() -> Option<u16> {
    Some(6881)
}

impl Config {
    /// Fill in `None` fields that derive from `data_dir`. Call once after loading.
    pub fn resolve(mut self) -> Self {
        if self
            .database_url
            .is_none()
        {
            self.database_url = Some(format!(
                "sqlite://{}?mode=rwc",
                self.data_dir
                    .join("db.sqlite")
                    .display()
            ));
        }
        if self
            .torrent_data_dir
            .is_none()
        {
            self.torrent_data_dir = Some(
                self.data_dir
                    .join("torrents")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if self
            .log_dir
            .is_none()
        {
            self.log_dir = Some(
                self.data_dir
                    .join("logs"),
            );
        }
        self
    }
}

impl Config {
    /// The configured server-wide MDBList key, if non-blank.
    pub fn mdblist_key(&self) -> Option<&str> {
        self.mdblist_api_key
            .as_ref()
            .map(Secret::expose)
            .map(str::trim)
            .filter(|k| !k.is_empty())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            database_url: None,
            torrent_data_dir: None,
            host: default_host(),
            port: default_port(),
            torrent_http_port: default_torrent_http_port_opt(),
            slow_query_threshold_ms: default_slow_query_threshold_ms(),
            disable_dht: false,
            disable_startup_tasks: false,
            torrent_peer_port: default_torrent_peer_port(),
            bgutil_script_path: default_bgutil_script_path(),
            tmdb_base_url: default_tmdb_base_url(),
            remuxdb_url: Some("https://remuxdb.1632022.xyz".to_string()),
            activity_log_retention_days: default_activity_log_retention_days(),
            jellyfin_version: default_jellyfin_version(),
            log_file_enabled: default_log_file_enabled(),
            log_dir: None,
            log_file_max_files: default_log_file_max_files(),
            log_file_retention_days: default_log_file_retention_days(),
            otlp_endpoint: None,
            games_enabled: false,
            games_roots: Vec::new(),
            games_scan_timeout_secs: default_games_scan_timeout_secs(),
            games_library_name: default_games_library_name(),
            games_archive_max_extract_bytes: default_games_archive_max_extract_bytes(),
            games_saves_max_bytes_per_user: default_games_saves_max_bytes_per_user(),
            games_saves_max_files_per_user: default_games_saves_max_files_per_user(),
            games_thumbnails_enabled: default_games_thumbnails_enabled(),
            games_thumbnails_base_url: default_games_thumbnails_base_url(),
            games_metadata_enabled: default_games_metadata_enabled(),
            games_metadata_rdb_base_url: default_games_metadata_rdb_base_url(),
            games_metadata_refresh_days: default_games_metadata_refresh_days(),
            games_metadata_retry_secs: default_games_metadata_retry_secs(),
            games_metadata_max_hash_bytes: default_games_metadata_max_hash_bytes(),
            games_launchbox_enabled: false,
            games_launchbox_url: default_games_launchbox_url(),
            games_launchbox_refresh_days: default_games_launchbox_refresh_days(),
            games_cores_url: None,
            games_cores_sha256: None,
            games_cores_size: None,
            games_cores_max_download_bytes: default_games_cores_max_download_bytes(),
            games_cores_max_extract_bytes: default_games_cores_max_extract_bytes(),
            moonfin_default_settings_path: None,
            moonfin_web_path: None,
            moonfin_web_default_server_url: None,
            moonfin_web_forced_server_url: None,
            moonfin_web_enable_webrtc_scan: false,
            mdblist_api_key: None,
            mdblist_base_url: default_mdblist_base_url(),
            mdblist_cache_ttl_hours: default_mdblist_cache_ttl_hours(),
        }
        .resolve()
    }
}

pub fn rewrite_request_uri<B>(mut req: http::Request<B>) -> http::Request<B> {
    let uri = req.uri();
    // Moonfin plugin routes: only a leading `/emby` is stripped (later
    // occurrences, e.g. inside ROM file names, must survive) and every path is
    // normalised per-segment, never treated as a case-preserving static file.
    let raw_path = uri.path();
    let moonfin_rest = raw_path
        .strip_prefix("/emby")
        .filter(|rest| rest.starts_with('/'))
        .unwrap_or(raw_path);
    let is_moonfin = moonfin_rest
        .trim_start_matches('/')
        .split('/')
        .next()
        .is_some_and(|seg| seg.eq_ignore_ascii_case("moonfin"));
    let path = if is_moonfin {
        moonfin_rest.to_string()
    } else {
        raw_path.replace("/emby", "")
    };

    // Trim trailing slashes, but keep "/" for root, and preserve the trailing slash
    // on SPA mount prefixes (/web/, /admin/, /jellyfin/, /remux/) so WebClientService and
    // ServeDir can properly resolve relative asset URLs without infinite redirect loops.
    let trimmed = path.trim_end_matches('/');
    let path = if trimmed.is_empty() {
        "/".to_string()
    } else if trimmed.eq_ignore_ascii_case("/web")
        || trimmed.eq_ignore_ascii_case("/admin")
        || trimmed.eq_ignore_ascii_case("/jellyfin")
        || trimmed.eq_ignore_ascii_case("/remux")
    {
        if path.ends_with('/') {
            format!("{trimmed}/")
        } else {
            trimmed.to_string()
        }
    } else {
        trimmed.to_string()
    };

    // Keep file paths case-sensitive (Linux filesystems are case-sensitive).
    // Only normalize API-style routes that don't look like files, plus known
    // API file endpoints (for example /Videos/.../Stream.vtt).
    let last_segment = path
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let is_file_like = last_segment.contains('.');
    let lower_path = path.to_ascii_lowercase();
    let api_file_like = is_file_like
        && (lower_path.starts_with("/videos/")
            || lower_path.starts_with("/audio/")
            || lower_path.starts_with("/items/")
            || lower_path.starts_with("/mediasegments/")
            || lower_path.starts_with("/sessions/"));

    let smart_lower_path: String = path
        .split('/')
        .map(|seg| {
            if seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric())
            {
                seg.to_ascii_lowercase()
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/");

    // Remux Web has path routes whose segments are case-sensitive (names,
    // ids), unlike the API keywords normalised below. Leave its paths alone.
    let is_remux_web = path.eq_ignore_ascii_case("/remux")
        || path
            .to_ascii_lowercase()
            .starts_with("/remux/");

    let new_path = if path == "/" || is_remux_web {
        path
    } else if is_file_like && !api_file_like && !is_moonfin {
        // Static file — preserve original casing.
        path
    } else if api_file_like {
        // Known API file endpoint (HLS, VTT …) — full lowercase so the route
        // and file-extension both match our lowercase handler definitions.
        lower_path
    } else {
        // Regular API route — lowercase route keywords, preserve param values.
        smart_lower_path
    };

    let query = uri
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();

    let new_uri = http::Uri::builder()
        .path_and_query(format!("{}{}", new_path, query))
        .build()
        .unwrap_or_else(|_| uri.clone());

    *req.uri_mut() = new_uri;
    req
}

/// Builds the OTLP (gRPC) tracing layer and registers its tracer provider
/// globally so it stays alive for the process lifetime — dropping it would
/// silently stop span export. `endpoint` is e.g. `http://jaeger:4317`.
fn build_otel_layer<S>(
    endpoint: &str,
) -> Option<impl tracing_subscriber::Layer<S> + Send + Sync + 'static>
where
    S: tracing::Subscriber
        + Send
        + Sync
        + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
    {
        Ok(exporter) => exporter,
        Err(e) => {
            eprintln!("failed to build OTLP exporter for {endpoint}: {e}");
            return None;
        }
    };

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name("remux-server")
                .build(),
        )
        .build();

    let tracer = provider.tracer("remux-server");
    opentelemetry::global::set_tracer_provider(provider);
    Some(tracing_opentelemetry::layer().with_tracer(tracer))
}

/// `otlp_endpoint`: when set (e.g. `http://jaeger-collector:4317`), tracing spans are
/// also exported via OTLP — see `Config::otlp_endpoint`. Gated by the same
/// `RUST_LOG`/`EnvFilter` as normal log output, so raising the level to see
/// more logs also sends more spans. File logging is best-effort: an unavailable
/// directory or rolling appender leaves stdout logging active.
pub fn setup_logging(
    log_dir: Option<&std::path::Path>,
    max_log_files: usize,
    otlp_endpoint: Option<&str>,
) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,remux=info"));

    let fmt_layer = fmt::layer()
        .with_timer(fmt::time::ChronoLocal::new("%H:%M:%S".to_string()))
        .with_target(true)
        .with_line_number(true)
        .with_file(false)
        .compact();

    let file_layer = log_dir.and_then(|dir| {
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("remux")
            .filename_suffix("log")
            .max_log_files(max_log_files)
            .build(dir);
        match appender {
            Ok(appender) => Some(
                fmt::layer()
                    .with_ansi(false)
                    // Daily rolling filenames are UTC; explicit UTC timestamps keep
                    // entries immediately after midnight on the matching date.
                    .with_timer(fmt::time::ChronoUtc::new("%+".to_string()))
                    .with_target(true)
                    .with_line_number(true)
                    .with_file(false)
                    .compact()
                    .with_writer(appender),
            ),
            Err(error) => {
                eprintln!("Remux file logging unavailable; continuing with stdout only: {error}");
                None
            }
        }
    });

    // FmtSpan::CLOSE makes every span passing through a layer log its own
    // duration on exit. Scoped to the `remux_server::metadata_refresh`
    // target only (every refresh-pipeline span sets this explicitly) via a
    // dedicated layer + per-layer filter, rather than applied to fmt_layer/
    // file_layer above — those cover the whole app, and every other
    // `#[instrument]`'d function (playback, subtitles, etc.) would otherwise
    // start emitting close events too, at whatever level it happens to be
    // instrumented at (often its default of INFO).
    let refresh_span_layer = fmt::layer()
        .with_timer(fmt::time::ChronoLocal::new("%H:%M:%S".to_string()))
        .with_target(true)
        .with_line_number(true)
        .with_file(false)
        .with_span_events(fmt::format::FmtSpan::CLOSE)
        .compact()
        .with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target("remux_server::metadata_refresh", tracing::Level::TRACE),
        );

    let otel_layer = otlp_endpoint.and_then(build_otel_layer);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(file_layer)
        .with(refresh_span_layer)
        .with(otel_layer)
        .try_init()
        .ok(); // try_init + ok() so tests don't panic on repeated calls
}

async fn handle_404(uri: axum::http::Uri) -> impl IntoResponse {
    debug!("404 - Not Found: {}", logging::loggable_uri(&uri));
    (StatusCode::NOT_FOUND, "Not Found")
}

fn log_api_error(err: &axum_anyhow::ApiError) {
    let status = err.status();
    let is_server_error = status.is_server_error();
    if let Some(cause) = err.error() {
        if is_server_error {
            error!(
                status = %status,
                title = %err.title(),
                detail = %err.detail(),
                cause = %format!("{:#}", cause),
                "api error"
            );
        } else {
            debug!(
                status = %status,
                title = %err.title(),
                detail = %err.detail(),
                cause = %format!("{:#}", cause),
                "api error"
            );
        }
    } else if is_server_error {
        error!(
            status = %status,
            title = %err.title(),
            detail = %err.detail(),
            "api error"
        );
    } else {
        debug!(
            status = %status,
            title = %err.title(),
            detail = %err.detail(),
            "api error"
        );
    }
}

async fn handle_static_404(req: Request<Body>) -> ApiResult<impl IntoResponse> {
    debug!(
        "Static 404 Not Found: {} {}",
        req.method(),
        req.uri()
            .path()
    );
    Ok((StatusCode::NOT_FOUND, "404 - File not found"))
}

#[cfg(test)]
pub mod integration_test;

#[cfg(test)]
mod rewrite_uri_tests {
    use super::rewrite_request_uri;

    fn rewrite(path: &str) -> String {
        let req = http::Request::builder()
            .method("GET")
            .uri(path)
            .body(())
            .unwrap();
        rewrite_request_uri(req)
            .uri()
            .path()
            .to_string()
    }

    /// `/remux/` lost its trailing slash here, so the web client service
    /// redirected it back to `/remux/` and the browser looped.
    #[test]
    fn keeps_the_remux_web_mount_and_its_paths_untouched() {
        assert_eq!(rewrite("/remux/"), "/remux/");
        assert_eq!(rewrite("/remux"), "/remux");
        assert_eq!(rewrite("/remux/assets/App.js"), "/remux/assets/App.js");
        assert_eq!(
            rewrite("/remux/library/Some-Name"),
            "/remux/library/Some-Name"
        );
        assert_eq!(
            rewrite("/remux/library/Some-Name/"),
            "/remux/library/Some-Name"
        );
    }

    fn rewrite_pq(path: &str) -> String {
        let req = http::Request::builder()
            .method("GET")
            .uri(path)
            .body(())
            .unwrap();
        rewrite_request_uri(req)
            .uri()
            .path_and_query()
            .map(|pq| pq.to_string())
            .unwrap_or_default()
    }

    #[test]
    fn moonfin_routes_are_lowercased_per_segment() {
        assert_eq!(
            rewrite_pq("/Moonfin/Games/Libraries"),
            "/moonfin/games/libraries"
        );
        assert_eq!(
            rewrite_pq("/emby/Moonfin/Games/Libraries"),
            "/moonfin/games/libraries"
        );
        assert_eq!(
            rewrite_pq("/Moonfin/Games/Saves/lr-fceumm-6a2f9c"),
            "/moonfin/games/saves/lr-fceumm-6a2f9c"
        );
    }

    #[test]
    fn moonfin_file_like_paths_keep_filename_case() {
        assert_eq!(
            rewrite_pq("/Moonfin/EmulatorJS/player.html?core=fceumm&rom=x"),
            "/moonfin/emulatorjs/player.html?core=fceumm&rom=x"
        );
        assert_eq!(
            rewrite_pq("/Moonfin/EmulatorJS/data/cores/fceumm-wasm.data"),
            "/moonfin/emulatorjs/data/cores/fceumm-wasm.data"
        );
        assert_eq!(
            rewrite_pq("/Moonfin/Games/lib-id/Rom/Tetris.zip"),
            "/moonfin/games/lib-id/rom/Tetris.zip"
        );
    }

    #[test]
    fn moonfin_rom_path_preserves_encoded_name_and_query() {
        assert_eq!(
            rewrite_pq(
                "/Moonfin/Games/0b7c1e2a-9f3d-5a6b-8c7d-112233445566/Rom/6a2f9c/Super%20Mario%20Bros%20%28U%29.zip?ApiKey=AbC123"
            ),
            "/moonfin/games/0b7c1e2a-9f3d-5a6b-8c7d-112233445566/rom/6a2f9c/Super%20Mario%20Bros%20%28U%29.zip?ApiKey=AbC123"
        );
    }

    #[test]
    fn moonfin_only_strips_leading_emby() {
        assert_eq!(
            rewrite_pq("/Moonfin/Games/lib-id/Rom/6a2f9c/emby_quest.zip"),
            "/moonfin/games/lib-id/rom/6a2f9c/emby_quest.zip"
        );
    }

    #[test]
    fn non_moonfin_file_like_paths_unchanged() {
        assert_eq!(rewrite_pq("/web/Main.Foo.js"), "/web/Main.Foo.js");
        assert_eq!(
            rewrite_pq("/Web/Assets/App.js?V=1"),
            "/Web/Assets/App.js?V=1"
        );
    }

    #[test]
    fn lowercases_alpha_keyword_segments() {
        assert_eq!(rewrite("/Items/Sessions"), "/items/sessions");
        assert_eq!(rewrite("/Genres"), "/genres");
    }

    #[test]
    fn lowercases_keyword_with_digits() {
        assert_eq!(rewrite("/Items/Filters2"), "/items/filters2");
        assert_eq!(rewrite("/Hls1"), "/hls1");
    }

    #[test]
    fn preserves_uuid_param_values() {
        assert_eq!(
            rewrite("/Items/f27caa37-e514-2225-cced-ed48f6553502"),
            "/items/f27caa37-e514-2225-cced-ed48f6553502"
        );
        assert_eq!(
            rewrite("/Items/F27CAA37-E514-2225-CCED-ED48F6553502"),
            "/items/F27CAA37-E514-2225-CCED-ED48F6553502"
        );
    }

    #[test]
    fn leaves_special_char_device_ids_alone() {
        let path = "/Sessions/Play/YWJjMTIz%7Cabc";
        let rewritten = rewrite(path);
        assert!(rewritten.starts_with("/sessions/play/"));
        assert!(rewritten.contains("YWJjMTIz%7Cabc"));
    }

    #[test]
    fn strips_trailing_slashes_from_api_routes() {
        assert_eq!(rewrite("/Items/"), "/items");
        assert_eq!(rewrite("/items/"), "/items");
        assert_eq!(rewrite("/Search/Hints/"), "/search/hints");
        assert_eq!(rewrite("/Genres/"), "/genres");
        assert_eq!(rewrite("/emby/Items/"), "/items");
        assert_eq!(
            rewrite("/Items/f27caa37-e514-2225-cced-ed48f6553502/"),
            "/items/f27caa37-e514-2225-cced-ed48f6553502"
        );
        assert_eq!(
            rewrite("/Users/f27caa37-e514-2225-cced-ed48f6553502/Items/"),
            "/users/f27caa37-e514-2225-cced-ed48f6553502/items"
        );
    }

    #[test]
    fn strips_multiple_trailing_slashes() {
        assert_eq!(rewrite("/Items///"), "/items");
        assert_eq!(rewrite("/Genres//"), "/genres");
    }

    #[test]
    fn preserves_root_and_emby_root() {
        assert_eq!(rewrite("/"), "/");
        assert_eq!(rewrite("///"), "/");
        assert_eq!(rewrite("/emby"), "/");
        assert_eq!(rewrite("/emby/"), "/");
    }

    #[test]
    fn preserves_spa_mount_trailing_slashes() {
        assert_eq!(rewrite("/web"), "/web");
        assert_eq!(rewrite("/web/"), "/web/");
        assert_eq!(rewrite("/admin"), "/admin");
        assert_eq!(rewrite("/admin/"), "/admin/");
        assert_eq!(rewrite("/jellyfin"), "/jellyfin");
        assert_eq!(rewrite("/jellyfin/"), "/jellyfin/");
    }

    #[test]
    fn preserves_query_string_with_trailing_slash() {
        let req = http::Request::builder()
            .method("GET")
            .uri("/Items/?Limit=50&ParentId=123")
            .body(())
            .unwrap();
        let rewritten = rewrite_request_uri(req);
        assert_eq!(
            rewritten
                .uri()
                .path(),
            "/items"
        );
        assert_eq!(
            rewritten
                .uri()
                .query(),
            Some("Limit=50&ParentId=123")
        );
    }

    #[test]
    fn normalizes_file_endpoints_with_trailing_slash() {
        assert_eq!(rewrite("/Videos/123/Stream.vtt/"), "/videos/123/stream.vtt");
    }
}

#[cfg(test)]
mod transcode_dir_tests {
    use super::select_transcode_dir;

    #[test]
    fn uses_writable_primary_directory() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp
            .path()
            .join("primary");
        let fallback = temp
            .path()
            .join("fallback");

        assert_eq!(select_transcode_dir(primary.clone(), fallback), primary);
    }

    #[test]
    fn falls_back_when_primary_directory_cannot_be_created() {
        let temp = tempfile::tempdir().unwrap();
        let blocking_file = temp
            .path()
            .join("not-a-directory");
        std::fs::write(&blocking_file, b"blocked").unwrap();
        let primary = blocking_file.join("transcode_sessions");
        let fallback = temp
            .path()
            .join("fallback");

        assert_eq!(select_transcode_dir(primary, fallback.clone()), fallback);
    }
}

#[cfg(all(test, unix))]
mod open_file_limit_tests {
    use super::{TARGET_OPEN_FILE_LIMIT, desired_open_file_limit};

    #[test]
    fn raises_soft_limit_to_target_without_changing_hard_limit() {
        assert_eq!(
            desired_open_file_limit(256, libc::RLIM_INFINITY),
            Some(TARGET_OPEN_FILE_LIMIT)
        );
        assert_eq!(
            desired_open_file_limit(TARGET_OPEN_FILE_LIMIT, libc::RLIM_INFINITY),
            None
        );
    }

    #[test]
    fn clamps_soft_limit_to_the_existing_hard_limit() {
        assert_eq!(desired_open_file_limit(256, 4096), Some(4096));
        assert_eq!(desired_open_file_limit(4096, 4096), None);
    }
}
