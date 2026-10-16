//! Moonfin/Moonbase-compatible retro-games endpoints (`/moonfin/games/...`).
//!
//! Wire format follows the Moonfin client parser (`moonbase_games_api.dart`):
//! camelCase fields, optional metadata omitted rather than null. Catalog data
//! comes from the games index snapshot; ROM/BIOS files are only ever resolved
//! through snapshot ids, never from request input. Saves are opaque per-user
//! blobs under `<data_dir>/games/saves/<user id>/`.

use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime},
};

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use dashmap::DashMap;
use futures_util::StreamExt;
use remux_macros::{get, put, query};
use serde::Serialize;
use strum_macros::{Display, EnumString};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    AppState,
    db::auth::AuthSession,
    games::{self, Game, GamesSnapshot},
};

/// Per-blob ceiling for save uploads (matches Moonbase).
const MAX_SAVE_BYTES: usize = 32 * 1024 * 1024;
/// Upper bound on a sanitized save id's length.
const MAX_SAVE_ID_LEN: usize = 128;
/// Temp files older than this are leftovers from a crashed upload.
const STALE_TMP_AGE: Duration = Duration::from_secs(3600);

/// One async mutex per user so concurrent PUTs cannot race past the quota.
/// Entries are tiny and bounded by the user count, so they are never evicted.
static USER_SAVE_LOCKS: LazyLock<DashMap<Uuid, Arc<tokio::sync::Mutex<()>>>> =
    LazyLock::new(DashMap::new);

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryDto {
    id: String,
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SystemDto {
    id: String,
    name: String,
    core: games::resolver::Core,
    game_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GameSummaryDto {
    id: String,
    title: String,
    system: String,
    core: games::resolver::Core,
    file_name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BiosDto {
    id: String,
    file_name: String,
    size_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GameDetailDto {
    #[serde(flatten)]
    summary: GameSummaryDto,
    backend_override_supported: bool,
    size_bytes: u64,
    bios: Vec<BiosDto>,
    // Metadata from the libretro databases (and LaunchBox, when enabled); each field is left out until it
    // is known (the client treats absent and empty alike).
    #[serde(skip_serializing_if = "Option::is_none")]
    genre: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    developer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    publisher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    franchise: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    year: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    players: Option<u8>,
    // LaunchBox only (opt-in).
    #[serde(skip_serializing_if = "Option::is_none")]
    overview: Option<String>,
    /// 0 to 5.
    #[serde(skip_serializing_if = "Option::is_none")]
    rating: Option<f64>,
}

impl From<&Game> for GameSummaryDto {
    fn from(g: &Game) -> Self {
        Self {
            id: g
                .id
                .clone(),
            title: g
                .title
                .clone(),
            system: g
                .system
                .clone(),
            core: g.core,
            file_name: g
                .file_name
                .clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Gating
// ---------------------------------------------------------------------------

/// Whether the games feature is on and `lib` names the Games view.
fn lib_matches(state: &AppState, lib: &str) -> bool {
    games::is_active(
        &state
            .ctx
            .config,
    ) && Uuid::parse_str(lib).ok() == Some(games::games_view_id())
}

/// Snapshot for `lib`, or `None` when games are disabled / `lib` is unknown.
async fn snapshot_for(state: &AppState, lib: &str) -> Option<Arc<GamesSnapshot>> {
    if !lib_matches(state, lib) {
        return None;
    }
    let cfg = &state
        .ctx
        .config;
    let snap = state
        .ctx
        .games
        .snapshot(cfg)
        .await;
    // Get the metadata databases for these systems on their way (once per
    // scan, in the background) so a game opened later already finds them.
    state
        .ctx
        .game_meta
        .prewarm(cfg, &snap);
    Some(snap)
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

#[get("/moonfin/games/libraries")]
pub async fn games_libraries(
    State(state): State<AppState>,
    _session: AuthSession,
) -> Response {
    let cfg = &state
        .ctx
        .config;
    let libs: Vec<LibraryDto> = if games::is_active(cfg) {
        vec![LibraryDto {
            id: games::games_view_id_string(),
            name: cfg
                .games_library_name
                .clone(),
        }]
    } else {
        Vec::new()
    };
    Json(libs).into_response()
}

#[get("/moonfin/games/{lib}/systems")]
pub async fn games_systems(
    State(state): State<AppState>,
    _session: AuthSession,
    Path(lib): Path<String>,
) -> Response {
    let Some(snap) = snapshot_for(&state, &lib).await else {
        return Json(Vec::<SystemDto>::new()).into_response();
    };
    let systems: Vec<SystemDto> = snap
        .systems()
        .into_iter()
        .filter_map(|name| {
            let mut in_system = snap
                .games
                .iter()
                .filter(|g| g.system == name);
            let first = in_system.next()?;
            Some(SystemDto {
                id: name.to_string(),
                name: name.to_string(),
                core: first.core,
                game_count: 1 + in_system.count(),
            })
        })
        .collect();
    Json(systems).into_response()
}

#[query]
pub struct GamesQuery {
    #[serde(default)]
    pub system: Option<String>,
}

#[get("/moonfin/games/{lib}/games")]
pub async fn games_list(
    State(state): State<AppState>,
    _session: AuthSession,
    Path(lib): Path<String>,
    Query(q): Query<GamesQuery>,
) -> Response {
    let Some(snap) = snapshot_for(&state, &lib).await else {
        return Json(Vec::<GameSummaryDto>::new()).into_response();
    };
    let system = q
        .system
        .as_deref()
        .filter(|s| !s.is_empty());
    let list: Vec<GameSummaryDto> = snap
        .games
        .iter()
        .filter(|g| {
            system.is_none_or(|s| {
                g.system
                    .eq_ignore_ascii_case(s)
            })
        })
        .map(GameSummaryDto::from)
        .collect();
    Json(list).into_response()
}

#[get("/moonfin/games/{lib}/games/{game_id}")]
pub async fn games_detail(
    State(state): State<AppState>,
    _session: AuthSession,
    Path((lib, game_id)): Path<(String, String)>,
) -> Response {
    let Some(snap) = snapshot_for(&state, &lib).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(game) = snap.game(&game_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let bios = snap
        .bios
        .get(&game.system)
        .map(|v| {
            v.iter()
                .map(|b| BiosDto {
                    id: b
                        .id
                        .clone(),
                    file_name: b
                        .file_name
                        .clone(),
                    size_bytes: b.size_bytes,
                })
                .collect()
        })
        .unwrap_or_default();
    // The ROM endpoint unpacks single-ROM archives, so the detail names what
    // the client will actually receive (the list keeps the archive's name,
    // like Moonbase).
    let mut summary = GameSummaryDto::from(game);
    summary.file_name = game
        .served_file_name()
        .to_string();
    let meta = state
        .ctx
        .game_meta
        .get(
            &state
                .ctx
                .config,
            game,
        )
        .await;
    Json(GameDetailDto {
        summary,
        backend_override_supported: false,
        size_bytes: game.served_size_bytes(),
        bios,
        genre: meta.genre,
        developer: meta.developer,
        publisher: meta.publisher,
        franchise: meta.franchise,
        region: meta.region,
        year: meta.year,
        players: meta.players,
        overview: meta.overview,
        rating: meta
            .rating
            .map(|r| r.as_f64()),
    })
    .into_response()
}

#[query]
pub struct ThumbQuery {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// Box art / snap / title screen for a game, fetched from the libretro
/// thumbnail server and cached (see [`games::thumbs`]).
///
/// The Moonfin client (legacy artwork adapter) latches a 404 as "no art" for
/// the screen's lifetime and any other failure as transient until the route is
/// re-entered, with no Retry-After handling; `CachedNetworkImage` just shows
/// the placeholder. So a fetch is awaited for a short budget (a normal first
/// load returns the image directly) and only then falls back to
/// `503` + `Retry-After`; the detached fetch keeps filling the cache.
#[get("/moonfin/games/{lib}/thumb/{game_id}")]
pub async fn games_thumb(
    State(state): State<AppState>,
    _session: AuthSession,
    Path((lib, game_id)): Path<(String, String)>,
    Query(q): Query<ThumbQuery>,
    req: Request<Body>,
) -> Response {
    let cfg = &state
        .ctx
        .config;
    if !cfg.games_thumbnails_enabled {
        return not_found(false);
    }
    let Some(snap) = snapshot_for(&state, &lib).await else {
        return not_found(false);
    };
    let Some(game) = snap.game(&game_id) else {
        return not_found(false);
    };
    let kind = games::thumbs::ThumbKind::parse_lenient(
        q.kind
            .as_deref(),
    );
    match state
        .ctx
        .game_thumbs
        .get(cfg, game, kind)
        .await
    {
        games::thumbs::ThumbResult::Found(path) => {
            let mut resp = match ServeFile::new(path)
                .oneshot(req)
                .await
            {
                Ok(r) => r.map(Body::new),
                Err(never) => match never {},
            };
            if resp
                .status()
                .is_success()
            {
                let h = resp.headers_mut();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
                h.insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("private, max-age=86400"),
                );
            }
            resp
        }
        games::thumbs::ThumbResult::Missing => not_found(true),
        games::thumbs::ThumbResult::Retry(after) => {
            let secs = after
                .as_secs()
                .max(1);
            let mut resp = StatusCode::SERVICE_UNAVAILABLE.into_response();
            let h = resp.headers_mut();
            h.insert(header::RETRY_AFTER, HeaderValue::from(secs));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
    }
}

/// 404 for artwork. Confirmed absence may be cached briefly by clients.
fn not_found(confirmed: bool) -> Response {
    let mut resp = StatusCode::NOT_FOUND.into_response();
    resp.headers_mut()
        .insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if confirmed {
                "private, max-age=300"
            } else {
                "no-store"
            }),
        );
    resp
}

// ---------------------------------------------------------------------------
// ROM / BIOS streaming
// ---------------------------------------------------------------------------

/// `attachment; filename="<ascii fallback>"; filename*=UTF-8''<pct-encoded>`.
fn content_disposition(name: &str) -> Option<HeaderValue> {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded =
        percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC);
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    .ok()
}

/// Serves `path` with Range / HEAD / conditional-request support.
async fn serve_file(path: PathBuf, file_name: &str, req: Request<Body>) -> Response {
    let svc = ServeFile::new(path);
    let mut resp = match svc
        .oneshot(req)
        .await
    {
        Ok(r) => r.map(Body::new),
        Err(never) => match never {},
    };
    if resp
        .status()
        .is_success()
    {
        // ServeFile guesses a type from the extension; ROMs are opaque bytes.
        resp.headers_mut()
            .insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        if let Some(v) = content_disposition(file_name) {
            resp.headers_mut()
                .insert(header::CONTENT_DISPOSITION, v);
        }
    }
    resp
}

#[get(
    "/moonfin/games/{lib}/rom/{token}",
    "/moonfin/games/{lib}/rom/{token}/{file_name}"
)]
pub async fn games_rom(
    State(state): State<AppState>,
    _session: AuthSession,
    Path(params): Path<HashMap<String, String>>,
    req: Request<Body>,
) -> Response {
    let (Some(lib), Some(token)) = (params.get("lib"), params.get("token")) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(snap) = snapshot_for(&state, lib).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(game) = snap.game(token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // The URL's file name only helps filename-sensitive emulators; it never
    // influences resolution.
    debug!(
        requested = params
            .get("file_name")
            .map(String::as_str),
        served = %game.served_file_name(),
        "games: rom request"
    );
    if game
        .inner_rom
        .is_some()
    {
        return serve_unpacked(&state, game, req).await;
    }
    serve_file(
        game.path
            .clone(),
        &game.file_name,
        req,
    )
    .await
}

/// Serves the ROM inside a `.zip`/`.7z` (non-arcade cores) from the unpack
/// cache, extracting it on first use. Range/HEAD behave like any plain file.
async fn serve_unpacked(state: &AppState, game: &Game, req: Request<Body>) -> Response {
    let config = &state
        .ctx
        .config;
    let cache_root = config
        .data_dir
        .join("games")
        .join("extracted");
    match games::archive::ensure_unpacked(
        &cache_root,
        &game.id,
        &game.path,
        config.games_archive_max_extract_bytes,
    )
    .await
    {
        Ok(rom) => serve_file(rom.path, &rom.name, req).await,
        Err(games::archive::ArchiveError::TooLarge { max, .. }) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "error": format!(
                    "The archive entry exceeds the {} MB limit for extracted ROMs.",
                    max / (1024 * 1024)
                )
            })),
        )
            .into_response(),
        Err(e) => {
            warn!(game = %game.id, error = %e, "games: cannot unpack archive ROM");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

#[get("/moonfin/games/{lib}/bios/{token}")]
pub async fn games_bios(
    State(state): State<AppState>,
    _session: AuthSession,
    Path((lib, token)): Path<(String, String)>,
    req: Request<Body>,
) -> Response {
    let Some(snap) = snapshot_for(&state, &lib).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(bios) = snap.bios_file(&token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    serve_file(
        bios.path
            .clone(),
        &bios.file_name,
        req,
    )
    .await
}

// ---------------------------------------------------------------------------
// Cores
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CoresStatusDto {
    installed: bool,
    downloading: bool,
    state: &'static str,
    error: Option<String>,
    files_installed: u32,
}

/// Remux does not host EmulatorJS cores, so this is a constant "idle" status
/// (the same state Moonbase reports before anything is installed).
#[get("/moonfin/games/cores/status")]
pub async fn games_cores_status(_session: AuthSession) -> Json<CoresStatusDto> {
    Json(CoresStatusDto {
        installed: false,
        downloading: false,
        state: "idle",
        error: None,
        files_installed: 0,
    })
}

// ---------------------------------------------------------------------------
// Saves
// ---------------------------------------------------------------------------

/// A save id proven safe to use as a file name: `[a-z0-9-]`, non-empty, bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SaveId(String);

impl SaveId {
    /// Lowercases first: Remux's URI rewrite keeps the case of segments that
    /// contain `-`, so `lr-fceumm-ABC` and `lr-fceumm-abc` must be one save.
    fn parse(s: &str) -> Option<Self> {
        let s = s.to_ascii_lowercase();
        let ok = !s.is_empty()
            && s.len() <= MAX_SAVE_ID_LEN
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        ok.then_some(Self(s))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, EnumString)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
enum SaveKind {
    State,
    Sram,
    Settings,
}

impl SaveKind {
    /// Moonbase semantics: missing or unrecognised kinds mean `state`.
    fn from_query(raw: Option<&str>) -> Self {
        raw.and_then(|s| {
            s.trim()
                .parse()
                .ok()
        })
        .unwrap_or(SaveKind::State)
    }
}

#[query]
pub struct SaveQuery {
    #[serde(default)]
    pub kind: Option<String>,
}

fn save_path(state: &AppState, user: Uuid, id: &SaveId, kind: SaveKind) -> PathBuf {
    state
        .ctx
        .config
        .data_dir
        .join("games")
        .join("saves")
        .join(
            user.simple()
                .to_string(),
        )
        .join(format!("{}.{kind}", id.0))
}

#[get("/moonfin/games/saves/{game_id}")]
pub async fn games_get_save(
    State(state): State<AppState>,
    session: AuthSession,
    Path(game_id): Path<String>,
    Query(q): Query<SaveQuery>,
) -> Response {
    if !games::is_active(
        &state
            .ctx
            .config,
    ) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(id) = SaveId::parse(&game_id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let path = save_path(
        &state,
        session
            .user
            .id,
        &id,
        SaveKind::from_query(
            q.kind
                .as_deref(),
        ),
    );
    match tokio::fs::read(&path).await {
        Ok(data) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            warn!(error = %e, "games: reading save failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[put("/moonfin/games/saves/{game_id}")]
pub async fn games_put_save(
    State(state): State<AppState>,
    session: AuthSession,
    Path(game_id): Path<String>,
    Query(q): Query<SaveQuery>,
    req: Request<Body>,
) -> Response {
    if !games::is_active(
        &state
            .ctx
            .config,
    ) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(id) = SaveId::parse(&game_id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let cfg = &state
        .ctx
        .config;
    let user = session
        .user
        .id;
    let path = save_path(
        &state,
        user,
        &id,
        SaveKind::from_query(
            q.kind
                .as_deref(),
        ),
    );
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| {
            v.to_str()
                .ok()
        })
        .and_then(|v| {
            v.parse::<u64>()
                .ok()
        });
    if declared.is_some_and(|n| n > MAX_SAVE_BYTES as u64) {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }

    // Serialise quota check + write per user.
    let lock = USER_SAVE_LOCKS
        .entry(user)
        .or_default()
        .clone();
    let _guard = lock
        .lock()
        .await;

    let Some(dir) = path.parent() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        warn!(error = %e, "games: creating save dir failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let usage = scan_usage(dir, &path).await;
    let files_after = usage.files
        + u64::from(
            !usage
                .replaced_size
                .is_some(),
        );
    if files_after > u64::from(cfg.games_saves_max_files_per_user) {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let budget = cfg
        .games_saves_max_bytes_per_user
        .saturating_sub(
            usage.bytes
                - usage
                    .replaced_size
                    .unwrap_or(0),
        );
    let cap = budget.min(MAX_SAVE_BYTES as u64);
    if declared.is_some_and(|n| n > cap) {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }

    match write_stream(&path, req.into_body(), cap).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(SaveWriteError::TooLarge) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(SaveWriteError::Empty | SaveWriteError::Body) => {
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(SaveWriteError::Io(e)) => {
            warn!(error = %e, "games: writing save failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Debug)]
enum SaveWriteError {
    TooLarge,
    Empty,
    Body,
    Io(std::io::Error),
}

impl From<std::io::Error> for SaveWriteError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

struct Usage {
    bytes: u64,
    files: u64,
    /// Size of the file the PUT will replace, if it exists.
    replaced_size: Option<u64>,
}

/// Sums a user's saves (temp files excluded) and opportunistically removes
/// stale temp files.
async fn scan_usage(dir: &FsPath, target: &FsPath) -> Usage {
    let mut usage = Usage {
        bytes: 0,
        files: 0,
        replaced_size: None,
    };
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return usage;
    };
    while let Ok(Some(entry)) = rd
        .next_entry()
        .await
    {
        let Ok(meta) = entry
            .metadata()
            .await
        else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name();
        if name
            .to_string_lossy()
            .ends_with(".tmp")
        {
            let stale = meta
                .modified()
                .ok()
                .and_then(|m| {
                    SystemTime::now()
                        .duration_since(m)
                        .ok()
                })
                .is_some_and(|age| age > STALE_TMP_AGE);
            if stale {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
            continue;
        }
        usage.bytes += meta.len();
        usage.files += 1;
        if entry.path() == target {
            usage.replaced_size = Some(meta.len());
        }
    }
    usage
}

/// Streams `body` to a sibling temp file (at most `cap` bytes), fsyncs, then
/// renames over `path`. The temp file is removed on any failure.
async fn write_stream(
    path: &FsPath,
    body: Body,
    cap: u64,
) -> Result<(), SaveWriteError> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("save path has no parent"))?;
    let tmp = dir.join(format!(".{}.tmp", Uuid::new_v4().simple()));
    let result = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut stream = body.into_data_stream();
        let mut written: u64 = 0;
        while let Some(chunk) = stream
            .next()
            .await
        {
            let chunk = chunk.map_err(|_| SaveWriteError::Body)?;
            written += chunk.len() as u64;
            if written > cap {
                return Err(SaveWriteError::TooLarge);
            }
            file.write_all(&chunk)
                .await?;
        }
        if written == 0 {
            return Err(SaveWriteError::Empty);
        }
        file.flush()
            .await?;
        file.sync_all()
            .await?;
        drop(file);
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Config, init_app_with_ctx,
        integration_test::auth_header_with_token,
        integration_test::{AUTH_HEADER, TestGuard},
        rewrite_request_uri,
    };
    use axum::Router;
    use axum_test::TestServer;
    use serde_json::{Value, json};
    use tower::Layer;
    use tower::util::MapRequestLayer;

    const ROM_BYTES: &[u8] = b"0123456789abcdefghij";

    struct Harness {
        server: TestServer,
        _guard: TestGuard,
        token: String,
        _roms: tempfile::TempDir,
        data: tempfile::TempDir,
        lib: String,
    }

    impl Harness {
        fn auth(&self) -> http::HeaderValue {
            http::HeaderValue::from_str(&auth_header_with_token(&self.token)).unwrap()
        }

        async fn login(&self, name: &str, pw: &str) -> String {
            let resp = self
                .server
                .post("/Users/AuthenticateByName")
                .add_header(
                    header::AUTHORIZATION,
                    http::HeaderValue::from_static(AUTH_HEADER),
                )
                .json(&json!({ "Username": name, "Pw": pw }))
                .await;
            resp.json::<Value>()["AccessToken"]
                .as_str()
                .unwrap()
                .to_string()
        }
    }

    /// Server behind the production URI rewrite layer (as in `bind_and_serve`),
    /// so mixed-case Moonfin URLs reach the lowercase routes.
    async fn harness(enabled: bool) -> Harness {
        harness_with(enabled, |_| {}).await
    }

    async fn harness_with(enabled: bool, tweak: impl FnOnce(&mut Config)) -> Harness {
        let roms = tempfile::tempdir().unwrap();
        let r = roms.path();
        std::fs::create_dir_all(r.join("NES")).unwrap();
        std::fs::write(r.join("NES/Super Mario (U).nes"), ROM_BYTES).unwrap();
        std::fs::write(r.join("NES/Zelda.nes"), b"zelda").unwrap();
        std::fs::create_dir_all(r.join("PlayStation/Crash")).unwrap();
        std::fs::write(r.join("PlayStation/Crash/crash.7z"), b"archive").unwrap();
        std::fs::write(r.join("PlayStation/scph1001.bin"), b"bios!").unwrap();
        let data = tempfile::tempdir().unwrap();

        let mut config = Config {
            database_url: Some("sqlite::memory:".into()),
            torrent_http_port: None,
            torrent_peer_port: None,
            disable_dht: true,
            data_dir: data
                .path()
                .to_path_buf(),
            games_enabled: enabled,
            games_roots: vec![
                r.to_string_lossy()
                    .into_owned(),
            ],
            // Never reach the real libretro CDN from tests; metadata tests
            // opt in with their own mock base URL.
            games_metadata_enabled: false,
            ..Default::default()
        };
        tweak(&mut config);
        let (router, ctx) = init_app_with_ctx(config)
            .await
            .unwrap();
        let app = Router::new()
            .fallback_service(MapRequestLayer::new(rewrite_request_uri).layer(router));
        let server = TestServer::builder()
            .mock_transport()
            .build(app)
            .unwrap();
        server
            .post("/Startup/User")
            .json(&json!({ "Name": "test", "Password": "test" }))
            .await;
        server
            .post("/Startup/Complete")
            .await;

        let mut h = Harness {
            server,
            _guard: TestGuard(ctx),
            token: String::new(),
            _roms: roms,
            data,
            lib: games::games_view_id_string(),
        };
        h.token = h
            .login("test", "test")
            .await;
        h
    }

    async fn get_json(h: &Harness, path: &str) -> Value {
        h.server
            .get(path)
            .add_header(header::AUTHORIZATION, h.auth())
            .await
            .json::<Value>()
    }

    fn id_of(list: &Value, title: &str) -> String {
        list.as_array()
            .unwrap()
            .iter()
            .find(|g| g["title"] == title)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn libraries_listed_when_active_empty_when_disabled() {
        let h = harness(true).await;
        let body = get_json(&h, "/Moonfin/Games/Libraries").await;
        assert_eq!(body, json!([{ "id": h.lib, "name": "Games" }]));
        assert_eq!(
            h.lib,
            games::games_view_id()
                .simple()
                .to_string()
        );

        let off = harness(false).await;
        assert_eq!(get_json(&off, "/Moonfin/Games/Libraries").await, json!([]));
        let lib = off
            .lib
            .clone();
        assert_eq!(
            get_json(&off, &format!("/Moonfin/Games/{lib}/Systems")).await,
            json!([])
        );
        assert_eq!(
            get_json(&off, &format!("/Moonfin/Games/{lib}/Games")).await,
            json!([])
        );
        off.server
            .get(&format!("/Moonfin/Games/{lib}/Games/abc"))
            .add_header(header::AUTHORIZATION, off.auth())
            .expect_failure()
            .await
            .assert_status_not_found();
    }

    #[tokio::test]
    async fn library_id_string_equals_userviews_id() {
        // Moonfin matches the two with an exact string compare.
        let h = harness(true).await;
        let libs = get_json(&h, "/Moonfin/Games/Libraries").await;
        let lib_id = libs[0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let views = get_json(&h, "/UserViews").await;
        let view_id = views["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["Name"] == "Games")
            .expect("games view in /UserViews")["Id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(lib_id, view_id);
        // The hyphenated form still routes.
        let hyphenated = games::games_view_id().to_string();
        assert_eq!(
            get_json(&h, &format!("/Moonfin/Games/{hyphenated}/Systems"))
                .await
                .as_array()
                .map(|a| a.is_empty()),
            get_json(&h, &format!("/Moonfin/Games/{}/Systems", h.lib))
                .await
                .as_array()
                .map(|a| a.is_empty()),
        );
    }

    #[tokio::test]
    async fn requires_authentication() {
        let h = harness(true).await;
        for path in [
            "/Moonfin/Games/Libraries".to_string(),
            format!("/Moonfin/Games/{}/Systems", h.lib),
            format!("/Moonfin/Games/{}/Rom/abc", h.lib),
            "/Moonfin/Games/Saves/lr-fceumm-abc".to_string(),
            "/Moonfin/Games/Cores/Status".to_string(),
        ] {
            h.server
                .get(&path)
                .expect_failure()
                .await
                .assert_status(http::StatusCode::UNAUTHORIZED);
        }
        h.server
            .put("/Moonfin/Games/Saves/lr-fceumm-abc")
            .bytes(
                b"x".as_slice()
                    .into(),
            )
            .expect_failure()
            .await
            .assert_status(http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn systems_games_and_detail_shapes() {
        let h = harness(true).await;
        let lib = h
            .lib
            .clone();

        let systems = get_json(&h, &format!("/Moonfin/Games/{lib}/Systems")).await;
        assert_eq!(
            systems,
            json!([
                { "id": "NES", "name": "NES", "core": "nes", "gameCount": 2 },
                { "id": "PlayStation", "name": "PlayStation", "core": "psx", "gameCount": 1 },
            ])
        );

        let all = get_json(&h, &format!("/Moonfin/Games/{lib}/Games")).await;
        assert_eq!(
            all.as_array()
                .unwrap()
                .len(),
            3
        );
        let nes = get_json(&h, &format!("/Moonfin/Games/{lib}/Games?system=nes")).await;
        let titles: Vec<&str> = nes
            .as_array()
            .unwrap()
            .iter()
            .map(|g| {
                g["title"]
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(titles, vec!["Super Mario (U)", "Zelda"]);
        let mario_id = id_of(&nes, "Super Mario (U)");
        assert_eq!(
            nes[0],
            json!({
                "id": mario_id, "title": "Super Mario (U)", "system": "NES",
                "core": "nes", "fileName": "Super Mario (U).nes",
            })
        );

        let detail =
            get_json(&h, &format!("/Moonfin/Games/{lib}/Games/{mario_id}")).await;
        assert_eq!(
            detail,
            json!({
                "id": mario_id, "title": "Super Mario (U)", "system": "NES",
                "core": "nes", "fileName": "Super Mario (U).nes",
                "backendOverrideSupported": false,
                "sizeBytes": ROM_BYTES.len(),
                "bios": [],
            })
        );

        let psx = get_json(
            &h,
            &format!("/Moonfin/Games/{lib}/Games?system=PlayStation"),
        )
        .await;
        let crash_id = id_of(&psx, "Crash");
        let detail =
            get_json(&h, &format!("/Moonfin/Games/{lib}/Games/{crash_id}")).await;
        let bios = detail["bios"][0].clone();
        assert_eq!(bios["fileName"], "scph1001.bin");
        assert_eq!(bios["sizeBytes"], 5);
        assert!(
            bios["id"]
                .as_str()
                .is_some()
        );

        // Unknown game / wrong library.
        h.server
            .get(&format!("/Moonfin/Games/{lib}/Games/{}", "0".repeat(32)))
            .add_header(header::AUTHORIZATION, h.auth())
            .expect_failure()
            .await
            .assert_status_not_found();
        h.server
            .get(&format!(
                "/Moonfin/Games/{}/Games/{mario_id}",
                Uuid::new_v4()
            ))
            .add_header(header::AUTHORIZATION, h.auth())
            .expect_failure()
            .await
            .assert_status_not_found();
    }

    #[tokio::test]
    async fn rom_full_range_head_and_misses() {
        let h = harness(true).await;
        let lib = h
            .lib
            .clone();
        let games = get_json(&h, &format!("/Moonfin/Games/{lib}/Games")).await;
        let id = id_of(&games, "Super Mario (U)");
        // The URL the player builds: mixed case, encoded file name, ApiKey query.
        let url = format!(
            "/Moonfin/Games/{lib}/Rom/{id}/Super%20Mario%20%28U%29.nes?ApiKey={}",
            h.token
        );

        let full = h
            .server
            .get(&url)
            .await;
        assert_eq!(
            full.as_bytes()
                .as_ref(),
            ROM_BYTES
        );
        assert_eq!(
            full.header(header::CONTENT_TYPE),
            "application/octet-stream"
        );
        assert_eq!(full.header(header::ACCEPT_RANGES), "bytes");
        let disp = full
            .header(header::CONTENT_DISPOSITION)
            .to_str()
            .unwrap()
            .to_string();
        assert!(disp.contains("filename=\"Super Mario (U).nes\""), "{disp}");

        // Token-only route works too.
        let bare = h
            .server
            .get(&format!("/Moonfin/Games/{lib}/Rom/{id}?ApiKey={}", h.token))
            .await;
        assert_eq!(
            bare.as_bytes()
                .as_ref(),
            ROM_BYTES
        );

        let part = h
            .server
            .get(&url)
            .add_header(header::RANGE, http::HeaderValue::from_static("bytes=2-5"))
            .await;
        assert_eq!(part.status_code(), http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            part.as_bytes()
                .as_ref(),
            &ROM_BYTES[2..=5]
        );
        assert_eq!(part.header(header::CONTENT_RANGE), "bytes 2-5/20");

        let head = h
            .server
            .method(http::Method::HEAD, &url)
            .await;
        assert_eq!(head.status_code(), http::StatusCode::OK);
        assert_eq!(head.header(header::CONTENT_LENGTH), "20");
        assert_eq!(head.header(header::ACCEPT_RANGES), "bytes");
        assert!(
            head.as_bytes()
                .is_empty()
        );

        h.server
            .get(&format!(
                "/Moonfin/Games/{lib}/Rom/{}?ApiKey={}",
                "f".repeat(32),
                h.token
            ))
            .expect_failure()
            .await
            .assert_status_not_found();
        h.server
            .get(&format!(
                "/Moonfin/Games/{}/Rom/{id}?ApiKey={}",
                Uuid::new_v4(),
                h.token
            ))
            .expect_failure()
            .await
            .assert_status_not_found();
        // Path-like tokens never resolve.
        h.server
            .get(&format!(
                "/Moonfin/Games/{lib}/Rom/..%2F..%2Fetc%2Fpasswd?ApiKey={}",
                h.token
            ))
            .expect_failure()
            .await
            .assert_status_not_found();
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            w.start_file(*name, opts)
                .unwrap();
            w.write_all(data)
                .unwrap();
        }
        w.finish()
            .unwrap()
            .into_inner()
    }

    /// Harness with an extra games root holding archives.
    async fn archive_harness(
        build: impl FnOnce(&std::path::Path),
        tweak: impl FnOnce(&mut Config),
    ) -> (Harness, tempfile::TempDir) {
        let extra = tempfile::tempdir().unwrap();
        build(extra.path());
        let root = extra
            .path()
            .to_string_lossy()
            .into_owned();
        let h = harness_with(true, move |c| {
            c.games_roots
                .push(root);
            tweak(c);
        })
        .await;
        (h, extra)
    }

    fn rom_url(h: &Harness, id: &str, name: &str) -> String {
        format!(
            "/Moonfin/Games/{}/Rom/{id}/{name}?ApiKey={}",
            h.lib, h.token
        )
    }

    fn extracted_dir(h: &Harness, id: &str) -> std::path::PathBuf {
        h.data
            .path()
            .join("games/extracted")
            .join(id)
    }

    #[tokio::test]
    async fn single_rom_zip_is_unpacked_and_cached() {
        let inner: Vec<u8> = (0..200u8).collect();
        let (h, _extra) = archive_harness(
            |r| {
                std::fs::create_dir_all(r.join("SNES/Hack Pack")).unwrap();
                std::fs::write(
                    r.join("SNES/Hack Pack/pack.zip"),
                    zip_bytes(&[
                        ("readme.txt", &[b'r'; 900]),
                        ("sub/Hack Game.sfc", &inner),
                    ]),
                )
                .unwrap();
            },
            |_| {},
        )
        .await;
        let lib = h
            .lib
            .clone();
        let list =
            get_json(&h, &format!("/Moonfin/Games/{lib}/Games?system=snes")).await;
        let id = id_of(&list, "Hack Pack");
        // Like Moonbase, the list keeps the archive's name; the detail names
        // what the ROM endpoint actually sends.
        assert_eq!(
            list.as_array()
                .unwrap()
                .iter()
                .find(|g| g["id"] == id.as_str())
                .unwrap()["fileName"],
            "pack.zip"
        );
        let detail = get_json(&h, &format!("/Moonfin/Games/{lib}/Games/{id}")).await;
        assert_eq!(detail["fileName"], "Hack Game.sfc");
        assert_eq!(detail["sizeBytes"], 200);
        assert_eq!(detail["core"], "snes");
        assert!(
            !extracted_dir(&h, &id).exists(),
            "nothing is unpacked before the first ROM request"
        );

        let url = rom_url(&h, &id, "Hack%20Game.sfc");
        let full = h
            .server
            .get(&url)
            .await;
        assert_eq!(
            full.as_bytes()
                .as_ref(),
            inner.as_slice()
        );
        assert_eq!(
            full.header(header::CONTENT_TYPE),
            "application/octet-stream"
        );
        let disp = full
            .header(header::CONTENT_DISPOSITION)
            .to_str()
            .unwrap()
            .to_string();
        assert!(disp.contains("filename=\"Hack Game.sfc\""), "{disp}");
        let cached = extracted_dir(&h, &id).join("Hack Game.sfc");
        assert_eq!(std::fs::read(&cached).unwrap(), inner);
        let stamp = std::fs::metadata(&cached)
            .unwrap()
            .modified()
            .unwrap();

        let part = h
            .server
            .get(&url)
            .add_header(header::RANGE, http::HeaderValue::from_static("bytes=10-19"))
            .await;
        assert_eq!(part.status_code(), http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            part.as_bytes()
                .as_ref(),
            &inner[10..=19]
        );
        assert_eq!(part.header(header::CONTENT_RANGE), "bytes 10-19/200");
        let head = h
            .server
            .method(http::Method::HEAD, &url)
            .await;
        assert_eq!(head.status_code(), http::StatusCode::OK);
        assert_eq!(head.header(header::CONTENT_LENGTH), "200");

        // Later requests are served from the cache, not re-extracted.
        assert_eq!(
            std::fs::metadata(&cached)
                .unwrap()
                .modified()
                .unwrap(),
            stamp
        );
    }

    #[tokio::test]
    async fn multi_rom_zip_serves_first_known_rom() {
        let (h, _extra) = archive_harness(
            |r| {
                std::fs::create_dir_all(r.join("GBA")).unwrap();
                std::fs::write(
                    r.join("GBA/Duo.zip"),
                    zip_bytes(&[
                        ("notes.nfo", b"info"),
                        ("First.gba", b"first!"),
                        ("Second.gba", b"second-and-longer"),
                    ]),
                )
                .unwrap();
            },
            |_| {},
        )
        .await;
        let lib = h
            .lib
            .clone();
        let list =
            get_json(&h, &format!("/Moonfin/Games/{lib}/Games?system=gba")).await;
        let id = id_of(&list, "Duo");
        let detail = get_json(&h, &format!("/Moonfin/Games/{lib}/Games/{id}")).await;
        assert_eq!(detail["fileName"], "First.gba");
        assert_eq!(detail["sizeBytes"], 6);
        let rom = h
            .server
            .get(&rom_url(&h, &id, "First.gba"))
            .await;
        assert_eq!(
            rom.as_bytes()
                .as_ref(),
            b"first!"
        );
    }

    #[tokio::test]
    async fn sevenz_rom_is_unpacked() {
        let (h, _extra) = archive_harness(
            |r| {
                std::fs::create_dir_all(r.join("GBA")).unwrap();
                let mut w = sevenz_rust2::ArchiveWriter::create(r.join("GBA/Kirby.7z"))
                    .unwrap();
                w.push_archive_entry(
                    sevenz_rust2::ArchiveEntry::new_file("Kirby.gba"),
                    Some(b"kirby-rom-data".as_slice()),
                )
                .unwrap();
                w.finish()
                    .unwrap();
            },
            |_| {},
        )
        .await;
        let lib = h
            .lib
            .clone();
        let list =
            get_json(&h, &format!("/Moonfin/Games/{lib}/Games?system=gba")).await;
        let id = id_of(&list, "Kirby");
        let detail = get_json(&h, &format!("/Moonfin/Games/{lib}/Games/{id}")).await;
        assert_eq!(detail["fileName"], "Kirby.gba");
        assert_eq!(detail["sizeBytes"], 14);
        let rom = h
            .server
            .get(&rom_url(&h, &id, "Kirby.gba"))
            .await;
        assert_eq!(
            rom.as_bytes()
                .as_ref(),
            b"kirby-rom-data"
        );
    }

    #[tokio::test]
    async fn arcade_zip_is_served_raw() {
        let zip = zip_bytes(&[("pacman.6e", &[1u8; 64]), ("pacman.6f", &[2u8; 64])]);
        let expected = zip.clone();
        let (h, _extra) = archive_harness(
            move |r| {
                std::fs::create_dir_all(r.join("Arcade")).unwrap();
                std::fs::write(r.join("Arcade/pacman.zip"), &zip).unwrap();
            },
            |_| {},
        )
        .await;
        let lib = h
            .lib
            .clone();
        let list =
            get_json(&h, &format!("/Moonfin/Games/{lib}/Games?system=arcade")).await;
        let id = id_of(&list, "pacman");
        let detail = get_json(&h, &format!("/Moonfin/Games/{lib}/Games/{id}")).await;
        assert_eq!(detail["fileName"], "pacman.zip");
        assert_eq!(detail["sizeBytes"], expected.len());
        let rom = h
            .server
            .get(&rom_url(&h, &id, "pacman.zip"))
            .await;
        assert_eq!(
            rom.as_bytes()
                .as_ref(),
            expected.as_slice()
        );
        assert!(!extracted_dir(&h, &id).exists());
    }

    #[tokio::test]
    async fn corrupt_archive_is_listed_and_served_as_is() {
        let (h, _extra) = archive_harness(
            |r| {
                std::fs::create_dir_all(r.join("SNES")).unwrap();
                std::fs::write(r.join("SNES/Broken.zip"), b"definitely not a zip")
                    .unwrap();
            },
            |_| {},
        )
        .await;
        let lib = h
            .lib
            .clone();
        let list =
            get_json(&h, &format!("/Moonfin/Games/{lib}/Games?system=snes")).await;
        let id = id_of(&list, "Broken");
        let detail = get_json(&h, &format!("/Moonfin/Games/{lib}/Games/{id}")).await;
        assert_eq!(detail["fileName"], "Broken.zip");
        assert_eq!(detail["sizeBytes"], 20);
        let rom = h
            .server
            .get(&rom_url(&h, &id, "Broken.zip"))
            .await;
        assert_eq!(
            rom.as_bytes()
                .as_ref(),
            b"definitely not a zip"
        );
    }

    #[tokio::test]
    async fn archive_that_goes_bad_after_scan_is_404_and_oversize_is_413() {
        let (h, extra) = archive_harness(
            |r| {
                std::fs::create_dir_all(r.join("SNES")).unwrap();
                std::fs::write(
                    r.join("SNES/Fine.zip"),
                    zip_bytes(&[("fine.sfc", &[9u8; 100])]),
                )
                .unwrap();
                std::fs::write(
                    r.join("SNES/Big.zip"),
                    zip_bytes(&[("big.sfc", &[9u8; 5000])]),
                )
                .unwrap();
            },
            |c| c.games_archive_max_extract_bytes = 1000,
        )
        .await;
        let lib = h
            .lib
            .clone();
        let list =
            get_json(&h, &format!("/Moonfin/Games/{lib}/Games?system=snes")).await;
        let big = id_of(&list, "Big");
        h.server
            .get(&rom_url(&h, &big, "big.sfc"))
            .expect_failure()
            .await
            .assert_status(http::StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!extracted_dir(&h, &big).exists());

        let fine = id_of(&list, "Fine");
        std::fs::write(
            extra
                .path()
                .join("SNES/Fine.zip"),
            b"now corrupt and a different size",
        )
        .unwrap();
        h.server
            .get(&rom_url(&h, &fine, "fine.sfc"))
            .expect_failure()
            .await
            .assert_status_not_found();
    }

    #[tokio::test]
    async fn bios_is_served_and_roms_are_not_bios() {
        let h = harness(true).await;
        let lib = h
            .lib
            .clone();
        let games = get_json(&h, &format!("/Moonfin/Games/{lib}/Games")).await;
        let crash = id_of(&games, "Crash");
        let detail = get_json(&h, &format!("/Moonfin/Games/{lib}/Games/{crash}")).await;
        let bios_id = detail["bios"][0]["id"]
            .as_str()
            .unwrap();
        let resp = h
            .server
            .get(&format!("/Moonfin/Games/{lib}/Bios/{bios_id}"))
            .add_header(header::AUTHORIZATION, h.auth())
            .await;
        assert_eq!(
            resp.as_bytes()
                .as_ref(),
            b"bios!"
        );
        h.server
            .method(
                http::Method::HEAD,
                &format!("/Moonfin/Games/{lib}/Bios/{bios_id}"),
            )
            .add_header(header::AUTHORIZATION, h.auth())
            .await
            .assert_status_ok();
        // A game id is not a BIOS token.
        h.server
            .get(&format!("/Moonfin/Games/{lib}/Bios/{crash}"))
            .add_header(header::AUTHORIZATION, h.auth())
            .expect_failure()
            .await
            .assert_status_not_found();
    }

    #[tokio::test]
    async fn thumb_and_cores_status() {
        let h = harness(true).await;
        let lib = h
            .lib
            .clone();
        h.server
            .get(&format!(
                "/Moonfin/Games/{lib}/Thumb/abc?type=boxart&artVersion=2"
            ))
            .add_header(header::AUTHORIZATION, h.auth())
            .expect_failure()
            .await
            .assert_status_not_found();
        assert_eq!(
            get_json(&h, "/Moonfin/Games/Cores/Status").await,
            json!({
                "installed": false, "downloading": false, "state": "idle",
                "error": null, "filesInstalled": 0,
            })
        );
    }

    const MARIO_PATH: &str = "/Nintendo%20-%20Nintendo%20Entertainment%20System/Named_Boxarts/Super%20Mario%20%28U%29.png";

    fn png_bytes() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(b"fake image data");
        v
    }

    async fn thumb_harness(mock: &httpmock::MockServer) -> Harness {
        let base = mock.base_url();
        harness_with(true, move |c| c.games_thumbnails_base_url = base).await
    }

    async fn thumb_get(h: &Harness, id: &str, query: &str) -> axum_test::TestResponse {
        h.server
            .get(&format!("/Moonfin/Games/{}/Thumb/{id}?{query}", h.lib))
            .add_header(header::AUTHORIZATION, h.auth())
            .await
    }

    async fn mario_id(h: &Harness) -> String {
        let games = get_json(h, &format!("/Moonfin/Games/{}/Games", h.lib)).await;
        id_of(&games, "Super Mario (U)")
    }

    #[tokio::test]
    async fn thumb_hit_is_cached_and_not_refetched() {
        let mock = httpmock::MockServer::start_async().await;
        let hit = mock
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path(MARIO_PATH);
                then.status(200)
                    .header("content-type", "image/png")
                    .body(png_bytes());
            })
            .await;
        let h = thumb_harness(&mock).await;
        let id = mario_id(&h).await;

        for _ in 0..2 {
            let resp = thumb_get(&h, &id, "type=boxart&artVersion=2").await;
            resp.assert_status_ok();
            assert_eq!(resp.header(header::CONTENT_TYPE), "image/png");
            assert_eq!(resp.header(header::CACHE_CONTROL), "private, max-age=86400");
            assert_eq!(
                resp.as_bytes()
                    .as_ref(),
                png_bytes().as_slice()
            );
        }
        hit.assert_hits_async(1)
            .await;
        assert!(
            h.data
                .path()
                .join(format!("games/thumbs/nes/boxart/{id}.png"))
                .is_file()
        );
    }

    #[tokio::test]
    async fn thumb_falls_back_to_short_name_and_kind_selects_folder() {
        let mock = httpmock::MockServer::start_async().await;
        let snap = mock
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/Nintendo%20-%20Nintendo%20Entertainment%20System/Named_Snaps/Super%20Mario.png");
                then.status(200)
                    .body(png_bytes());
            })
            .await;
        let h = thumb_harness(&mock).await;
        let id = mario_id(&h).await;
        // "Super Mario (U)" 404s (no mock), "Super Mario" matches.
        thumb_get(&h, &id, "type=snap")
            .await
            .assert_status_ok();
        snap.assert_hits_async(1)
            .await;
        assert!(
            h.data
                .path()
                .join(format!("games/thumbs/nes/snap/{id}.png"))
                .is_file()
        );
    }

    #[tokio::test]
    async fn thumb_miss_is_404_and_negative_cached() {
        let mock = httpmock::MockServer::start_async().await;
        let any = mock
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET);
                then.status(404);
            })
            .await;
        let h = thumb_harness(&mock).await;
        let id = mario_id(&h).await;

        thumb_get(&h, &id, "type=boxart")
            .await
            .assert_status_not_found();
        // stem "Super Mario (U)" (== title) and short "Super Mario".
        let first = any
            .hits_async()
            .await;
        assert_eq!(first, 2);
        assert!(
            h.data
                .path()
                .join(format!("games/thumbs/nes/boxart/{id}.miss"))
                .is_file()
        );

        thumb_get(&h, &id, "type=boxart")
            .await
            .assert_status_not_found();
        assert_eq!(
            any.hits_async()
                .await,
            first,
            "miss marker prevents refetch"
        );
    }

    #[tokio::test]
    async fn thumb_upstream_error_is_503_and_not_negative_cached() {
        let mock = httpmock::MockServer::start_async().await;
        let any = mock
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET);
                then.status(500);
            })
            .await;
        let h = thumb_harness(&mock).await;
        let id = mario_id(&h).await;

        let resp = thumb_get(&h, &id, "type=boxart").await;
        assert_eq!(resp.status_code(), http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            resp.header(header::RETRY_AFTER)
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                >= 1
        );
        let hits = any
            .hits_async()
            .await;
        assert!(hits >= 1);
        assert!(
            !h.data
                .path()
                .join(format!("games/thumbs/nes/boxart/{id}.miss"))
                .exists()
        );
        // Backoff: an immediate retry does not hit the failing upstream again.
        let again = thumb_get(&h, &id, "type=boxart").await;
        assert_eq!(again.status_code(), http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            any.hits_async()
                .await,
            hits
        );
    }

    #[tokio::test]
    async fn thumb_non_png_body_is_not_cached() {
        let mock = httpmock::MockServer::start_async().await;
        mock.mock_async(|when, then| {
            when.method(httpmock::Method::GET);
            then.status(200)
                .body("<html>captive portal</html>");
        })
        .await;
        let h = thumb_harness(&mock).await;
        let id = mario_id(&h).await;
        let resp = thumb_get(&h, &id, "type=boxart").await;
        assert_eq!(resp.status_code(), http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            !h.data
                .path()
                .join(format!("games/thumbs/nes/boxart/{id}.png"))
                .exists()
        );
    }

    #[tokio::test]
    async fn thumb_concurrent_requests_share_one_fetch() {
        let mock = httpmock::MockServer::start_async().await;
        let hit = mock
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path(MARIO_PATH);
                then.status(200)
                    .delay(Duration::from_millis(300))
                    .body(png_bytes());
            })
            .await;
        let h = thumb_harness(&mock).await;
        let id = mario_id(&h).await;
        let (a, b, c) = tokio::join!(
            thumb_get(&h, &id, "type=boxart"),
            thumb_get(&h, &id, "type=boxart"),
            thumb_get(&h, &id, "type=boxart"),
        );
        a.assert_status_ok();
        b.assert_status_ok();
        c.assert_status_ok();
        hit.assert_hits_async(1)
            .await;
    }

    #[tokio::test]
    async fn thumb_unknown_game_auth_and_disabled() {
        let mock = httpmock::MockServer::start_async().await;
        let any = mock
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET);
                then.status(200)
                    .body(png_bytes());
            })
            .await;
        let h = thumb_harness(&mock).await;
        let id = mario_id(&h).await;

        thumb_get(&h, "nope", "type=boxart")
            .await
            .assert_status_not_found();
        let wrong_lib = h
            .server
            .get(&format!("/Moonfin/Games/{}/Thumb/{id}", Uuid::new_v4()))
            .add_header(header::AUTHORIZATION, h.auth())
            .expect_failure()
            .await;
        wrong_lib.assert_status_not_found();
        let anon = h
            .server
            .get(&format!("/Moonfin/Games/{}/Thumb/{id}", h.lib))
            .expect_failure()
            .await;
        assert_eq!(anon.status_code(), http::StatusCode::UNAUTHORIZED);
        assert_eq!(
            any.hits_async()
                .await,
            0
        );

        let base = mock.base_url();
        let off = harness_with(true, move |c| {
            c.games_thumbnails_base_url = base;
            c.games_thumbnails_enabled = false;
        })
        .await;
        let id = mario_id(&off).await;
        thumb_get(&off, &id, "type=boxart")
            .await
            .assert_status_not_found();
        assert_eq!(
            any.hits_async()
                .await,
            0,
            "disabled never goes upstream"
        );
    }

    #[tokio::test]
    async fn saves_roundtrip_isolation_and_validation() {
        let h = harness(true).await;
        let id = "lr-fceumm-6a2f9c";
        let url = format!("/Moonfin/Games/Saves/{id}?kind=state");

        h.server
            .get(&url)
            .add_header(header::AUTHORIZATION, h.auth())
            .expect_failure()
            .await
            .assert_status_not_found();

        let put = h
            .server
            .put(&url)
            .add_header(header::AUTHORIZATION, h.auth())
            .content_type("application/octet-stream")
            .bytes(
                b"state-v1"
                    .as_slice()
                    .into(),
            )
            .await;
        assert_eq!(put.status_code(), http::StatusCode::NO_CONTENT);
        h.server
            .put(&url)
            .add_header(header::AUTHORIZATION, h.auth())
            .bytes(
                b"state-v2"
                    .as_slice()
                    .into(),
            )
            .await;
        let got = h
            .server
            .get(&url)
            .add_header(header::AUTHORIZATION, h.auth())
            .await;
        assert_eq!(
            got.as_bytes()
                .as_ref(),
            b"state-v2"
        );
        assert_eq!(got.header(header::CONTENT_TYPE), "application/octet-stream");

        // Default kind is state; other kinds are separate slots.
        let default_kind = h
            .server
            .get(&format!("/Moonfin/Games/Saves/{id}"))
            .add_header(header::AUTHORIZATION, h.auth())
            .await;
        assert_eq!(
            default_kind
                .as_bytes()
                .as_ref(),
            b"state-v2"
        );
        h.server
            .get(&format!("/Moonfin/Games/Saves/{id}?kind=sram"))
            .add_header(header::AUTHORIZATION, h.auth())
            .expect_failure()
            .await
            .assert_status_not_found();
        h.server
            .put(&format!("/Moonfin/Games/Saves/{id}?kind=sram"))
            .add_header(header::AUTHORIZATION, h.auth())
            .bytes(
                b"sram"
                    .as_slice()
                    .into(),
            )
            .await;
        assert!(
            h.data
                .path()
                .join("games/saves")
                .read_dir()
                .unwrap()
                .next()
                .is_some()
        );

        // Another user does not see it.
        h.server
            .post("/Users/New")
            .add_header(header::AUTHORIZATION, h.auth())
            .json(&json!({ "Name": "second", "Password": "pw" }))
            .await;
        let other = h
            .login("second", "pw")
            .await;
        let other_auth =
            http::HeaderValue::from_str(&auth_header_with_token(&other)).unwrap();
        h.server
            .get(&url)
            .add_header(header::AUTHORIZATION, other_auth.clone())
            .expect_failure()
            .await
            .assert_status_not_found();
        h.server
            .put(&url)
            .add_header(header::AUTHORIZATION, other_auth.clone())
            .bytes(
                b"theirs"
                    .as_slice()
                    .into(),
            )
            .await;
        let mine = h
            .server
            .get(&url)
            .add_header(header::AUTHORIZATION, h.auth())
            .await;
        assert_eq!(
            mine.as_bytes()
                .as_ref(),
            b"state-v2"
        );
        let theirs = h
            .server
            .get(&url)
            .add_header(header::AUTHORIZATION, other_auth)
            .await;
        assert_eq!(
            theirs
                .as_bytes()
                .as_ref(),
            b"theirs"
        );

        // No stray temp files.
        for entry in walk(
            h.data
                .path(),
        ) {
            assert!(!entry.ends_with(".tmp"), "{entry}");
        }

        // Bad ids.
        for bad in ["a.b", "a%2Fb", "a_b"] {
            h.server
                .get(&format!("/Moonfin/Games/Saves/{bad}"))
                .add_header(header::AUTHORIZATION, h.auth())
                .expect_failure()
                .await
                .assert_status_bad_request();
        }
        h.server
            .put("/Moonfin/Games/Saves/bad_id")
            .add_header(header::AUTHORIZATION, h.auth())
            .bytes(
                b"x".as_slice()
                    .into(),
            )
            .expect_failure()
            .await
            .assert_status_bad_request();
        // Empty payload.
        h.server
            .put(&url)
            .add_header(header::AUTHORIZATION, h.auth())
            .bytes(Vec::new().into())
            .expect_failure()
            .await
            .assert_status_bad_request();
    }

    fn walk(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.extend(walk(&p));
                } else {
                    out.push(
                        p.to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn oversize_save_is_rejected_with_413() {
        let h = harness(true).await;
        let url = "/Moonfin/Games/Saves/lr-fceumm-big?kind=state";
        let at_limit = vec![7u8; MAX_SAVE_BYTES];
        let resp = h
            .server
            .put(url)
            .add_header(header::AUTHORIZATION, h.auth())
            .bytes(at_limit.into())
            .await;
        assert_eq!(resp.status_code(), http::StatusCode::NO_CONTENT);
        let too_big = vec![7u8; MAX_SAVE_BYTES + 1];
        h.server
            .put(url)
            .add_header(header::AUTHORIZATION, h.auth())
            .bytes(too_big.into())
            .expect_failure()
            .await
            .assert_status(http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    fn user_dir_files(h: &Harness) -> Vec<String> {
        walk(
            &h.data
                .path()
                .join("games/saves"),
        )
    }

    #[tokio::test]
    async fn mixed_case_id_hits_same_file() {
        let h = harness(true).await;
        h.server
            .put("/Moonfin/Games/Saves/lr-fceumm-ABC123?kind=state")
            .add_header(header::AUTHORIZATION, h.auth())
            .bytes(
                b"case-bytes"
                    .as_slice()
                    .into(),
            )
            .await
            .assert_status(http::StatusCode::NO_CONTENT);
        for url in [
            "/Moonfin/Games/Saves/lr-fceumm-abc123?kind=state",
            "/Moonfin/Games/Saves/lr-fceumm-ABC123?kind=state",
            "/Moonfin/Games/Saves/LR-FCEUMM-abc123?kind=state",
        ] {
            let got = h
                .server
                .get(url)
                .add_header(header::AUTHORIZATION, h.auth())
                .await;
            assert_eq!(
                got.as_bytes()
                    .as_ref(),
                b"case-bytes",
                "{url}"
            );
        }
        let files = user_dir_files(&h);
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].ends_with("lr-fceumm-abc123.state"));
    }

    #[tokio::test]
    async fn chunked_oversize_body_is_rejected_and_cleaned_up() {
        let h = harness(true).await;
        // No Content-Length: a streamed body bigger than the cap.
        let chunk = bytes::Bytes::from(vec![1u8; 1024 * 1024]);
        let n = MAX_SAVE_BYTES / chunk.len() + 2;
        let stream = futures_util::stream::iter(
            (0..n).map(move |_| Ok::<_, std::io::Error>(chunk.clone())),
        );
        let req = Request::builder()
            .method("PUT")
            .uri("/Moonfin/Games/Saves/lr-fceumm-chunk?kind=state")
            .header(header::AUTHORIZATION, h.auth())
            .body(Body::from_stream(stream))
            .unwrap();
        // Drive the handler's writer directly (axum-test cannot send a
        // chunked body); this is the code path the PUT handler uses.
        let dir = h
            .data
            .path()
            .join("games/saves/u");
        std::fs::create_dir_all(&dir).unwrap();
        let r =
            write_stream(&dir.join("x.state"), req.into_body(), MAX_SAVE_BYTES as u64)
                .await;
        assert!(matches!(r, Err(SaveWriteError::TooLarge)), "{r:?}");
        let left = walk(&dir);
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    #[tokio::test]
    async fn byte_quota_rejects_and_overwrite_within_quota_is_accepted() {
        let h = harness_with(true, |c| c.games_saves_max_bytes_per_user = 100).await;
        let send = |id: &str, n: usize| {
            let url = format!("/Moonfin/Games/Saves/{id}?kind=state");
            let body = vec![9u8; n];
            let h = &h;
            async move {
                h.server
                    .put(&url)
                    .add_header(header::AUTHORIZATION, h.auth())
                    .bytes(body.into())
                    .await
                    .status_code()
            }
        };
        assert_eq!(send("a-1", 60).await, http::StatusCode::NO_CONTENT);
        // 60 + 50 > 100.
        assert_eq!(send("b-1", 50).await, http::StatusCode::PAYLOAD_TOO_LARGE);
        // Rejected upload leaves nothing behind.
        assert_eq!(user_dir_files(&h).len(), 1, "{:?}", user_dir_files(&h));
        // Overwriting a-1 with 100 bytes fits (old 60 counts as freed).
        assert_eq!(send("a-1", 100).await, http::StatusCode::NO_CONTENT);
        assert_eq!(send("a-1", 101).await, http::StatusCode::PAYLOAD_TOO_LARGE);
        let got = h
            .server
            .get("/Moonfin/Games/Saves/a-1?kind=state")
            .add_header(header::AUTHORIZATION, h.auth())
            .await;
        assert_eq!(
            got.as_bytes()
                .len(),
            100
        );
    }

    #[tokio::test]
    async fn file_count_quota_rejects_new_but_allows_replace_and_isolates_users() {
        let h = harness_with(true, |c| c.games_saves_max_files_per_user = 2).await;
        let send = |auth: http::HeaderValue, id: &str, kind: &str| {
            let url = format!("/Moonfin/Games/Saves/{id}?kind={kind}");
            let h = &h;
            async move {
                h.server
                    .put(&url)
                    .add_header(header::AUTHORIZATION, auth)
                    .bytes(
                        b"x".as_slice()
                            .into(),
                    )
                    .await
                    .status_code()
            }
        };
        assert_eq!(
            send(h.auth(), "g-1", "state").await,
            http::StatusCode::NO_CONTENT
        );
        assert_eq!(
            send(h.auth(), "g-1", "sram").await,
            http::StatusCode::NO_CONTENT
        );
        assert_eq!(
            send(h.auth(), "g-2", "state").await,
            http::StatusCode::PAYLOAD_TOO_LARGE
        );
        // Replacing an existing file is not a new file.
        assert_eq!(
            send(h.auth(), "g-1", "state").await,
            http::StatusCode::NO_CONTENT
        );

        // Another user has their own budget.
        h.server
            .post("/Users/New")
            .add_header(header::AUTHORIZATION, h.auth())
            .json(&json!({ "Name": "second", "Password": "pw" }))
            .await;
        let other = h
            .login("second", "pw")
            .await;
        let other_auth =
            http::HeaderValue::from_str(&auth_header_with_token(&other)).unwrap();
        assert_eq!(
            send(other_auth.clone(), "g-2", "state").await,
            http::StatusCode::NO_CONTENT
        );
        assert_eq!(
            send(other_auth, "g-3", "state").await,
            http::StatusCode::NO_CONTENT
        );
    }

    #[test]
    fn save_id_and_kind_parsing() {
        assert!(SaveId::parse("lr-fceumm-6a2f9c").is_some());
        assert!(SaveId::parse("ejs-snes-abc123").is_some());
        assert_eq!(
            SaveId::parse("LR-Fceumm-ABC")
                .unwrap()
                .0,
            "lr-fceumm-abc"
        );
        for bad in [
            "",
            "a/b",
            "a.b",
            "a b",
            "a_b",
            &"a".repeat(MAX_SAVE_ID_LEN + 1),
        ] {
            assert!(SaveId::parse(bad).is_none(), "{bad:?}");
        }
        assert_eq!(SaveKind::from_query(None), SaveKind::State);
        assert_eq!(SaveKind::from_query(Some("SRAM")), SaveKind::Sram);
        assert_eq!(SaveKind::from_query(Some("settings")), SaveKind::Settings);
        assert_eq!(SaveKind::from_query(Some("../x")), SaveKind::State);
    }

    // -----------------------------------------------------------------------
    // Game metadata from the libretro databases
    // -----------------------------------------------------------------------

    use games::rdb::test_support::{Rec, build as build_rdb};

    const NES_RDB_PATH: &str = "/Nintendo%20-%20Nintendo%20Entertainment%20System.rdb";
    const NES_RDB_FILE: &str =
        "games/meta/rdb/Nintendo - Nintendo Entertainment System.rdb";

    async fn meta_harness(
        base: String,
        tweak: impl FnOnce(&mut Config) + Send + 'static,
    ) -> Harness {
        harness_with(true, move |c| {
            c.games_metadata_enabled = true;
            c.games_metadata_rdb_base_url = base;
            tweak(c);
        })
        .await
    }

    fn nes_dir(h: &Harness) -> std::path::PathBuf {
        h._roms
            .path()
            .join("NES")
    }

    async fn game_id(h: &Harness, title: &str) -> String {
        let list = get_json(h, &format!("/Moonfin/Games/{}/Games", h.lib)).await;
        id_of(&list, title)
    }

    async fn detail(h: &Harness, id: &str) -> Value {
        let resp = h
            .server
            .get(&format!("/Moonfin/Games/{}/Games/{id}", h.lib))
            .add_header(header::AUTHORIZATION, h.auth())
            .await;
        resp.assert_status_ok();
        resp.json::<Value>()
    }

    /// Detail polled until `key` appears (the lookup finishes in the
    /// background after a first, possibly bare, answer).
    async fn detail_until(h: &Harness, id: &str, key: &str) -> Value {
        let mut last = Value::Null;
        for _ in 0..200 {
            last = detail(h, id).await;
            if last
                .get(key)
                .is_some()
            {
                return last;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("detail never gained {key:?}: {last}");
    }

    const META_KEYS: [&str; 9] = [
        "genre",
        "developer",
        "publisher",
        "franchise",
        "region",
        "year",
        "players",
        "overview",
        "rating",
    ];

    fn assert_no_meta(v: &Value) {
        for k in META_KEYS {
            assert!(
                v.get(k)
                    .is_none(),
                "unexpected {k}: {v}"
            );
        }
    }

    fn rdb_mock<'a>(
        mock: &'a httpmock::MockServer,
        body: Vec<u8>,
    ) -> httpmock::Mock<'a> {
        mock.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path(NES_RDB_PATH);
            then.status(200)
                .header("etag", "\"abc\"")
                .body(body);
        })
    }

    fn mario_record() -> Rec {
        Rec::named("Super Mario (USA)")
            .crc(crc32fast::hash(ROM_BYTES))
            .genre("Platform")
            .developer("Nintendo R&D4")
            .publisher("Nintendo")
            .franchise("Mario")
            .region("USA")
            .year(1985)
            .users(2)
    }

    #[tokio::test]
    async fn metadata_fills_detail_from_crc_and_caches_the_database() {
        let mock = httpmock::MockServer::start_async().await;
        let hit = rdb_mock(&mock, build_rdb(&[mario_record()]));
        let h = meta_harness(mock.base_url(), |_| {}).await;
        let id = game_id(&h, "Super Mario (U)").await;

        let d = detail_until(&h, &id, "genre").await;
        assert_eq!(d["genre"], "Platform");
        assert_eq!(d["developer"], "Nintendo R&D4");
        assert_eq!(d["publisher"], "Nintendo");
        assert_eq!(d["franchise"], "Mario");
        assert_eq!(d["region"], "USA");
        assert_eq!(d["year"], json!(1985));
        assert_eq!(d["players"], json!(2));
        // Existing fields are intact; overview/rating stay out of phase A.
        assert_eq!(d["id"], id);
        assert!(d["sizeBytes"].is_number());
        assert!(
            d.get("overview")
                .is_none()
                && d.get("rating")
                    .is_none()
        );

        // The list never carries metadata.
        let list = get_json(&h, &format!("/Moonfin/Games/{}/Games", h.lib)).await;
        for g in list
            .as_array()
            .unwrap()
        {
            assert_no_meta(g);
        }
        // A game the database does not know has the fields omitted, not null.
        let zelda = game_id(&h, "Zelda").await;
        assert_no_meta(&detail(&h, &zelda).await);

        // Downloaded once, cached with its validator; later requests are free.
        hit.assert_hits(1);
        assert!(
            h.data
                .path()
                .join(NES_RDB_FILE)
                .is_file()
        );
        assert_eq!(
            std::fs::read_to_string(
                h.data
                    .path()
                    .join(format!("{NES_RDB_FILE}.etag"))
            )
            .unwrap(),
            "\"abc\""
        );
        detail(&h, &id).await;
        hit.assert_hits(1);
    }

    #[tokio::test]
    async fn records_sharing_a_crc_merge_regardless_of_order() {
        let crc = crc32fast::hash(ROM_BYTES);
        let rich = Rec::named("Super Mario (USA)")
            .crc(crc)
            .genre("Platform")
            .developer("Nintendo")
            .year(1985);
        let bare = Rec::named("Super Mario (USA)")
            .crc(crc)
            .region("USA");
        for recs in [[rich.clone(), bare.clone()], [bare, rich]] {
            let mock = httpmock::MockServer::start_async().await;
            rdb_mock(&mock, build_rdb(&recs));
            let h = meta_harness(mock.base_url(), |_| {}).await;
            let id = game_id(&h, "Super Mario (U)").await;
            let d = detail_until(&h, &id, "genre").await;
            assert_eq!(d["genre"], "Platform");
            assert_eq!(d["developer"], "Nintendo");
            assert_eq!(d["year"], json!(1985));
            assert_eq!(d["region"], "USA");
        }
    }

    #[tokio::test]
    async fn nes_header_matches_the_headerless_record() {
        let body = b"rom body that is not the whole file";
        let mut file = b"NES\x1A".to_vec();
        file.extend([0u8; 12]);
        file.extend(body);
        let mock = httpmock::MockServer::start_async().await;
        rdb_mock(
            &mock,
            build_rdb(&[
                Rec::named("Headered (Japan)")
                    .crc(crc32fast::hash(body))
                    .region("Japan"),
                Rec::named("Other")
                    .crc(1)
                    .genre("Sports"),
            ]),
        );
        let h = meta_harness(mock.base_url(), |_| {}).await;
        std::fs::write(nes_dir(&h).join("Headered.nes"), &file).unwrap();
        let id = game_id(&h, "Headered").await;
        let d = detail_until(&h, &id, "region").await;
        assert_eq!(d["region"], "Japan");
        assert!(
            d.get("genre")
                .is_none()
        );
    }

    #[tokio::test]
    async fn zipped_rom_matches_by_archive_crc_without_extraction() {
        let inner = b"inner rom bytes, never unpacked";
        let mock = httpmock::MockServer::start_async().await;
        rdb_mock(
            &mock,
            build_rdb(&[Rec::named("Zipped (USA)")
                .crc(crc32fast::hash(inner))
                .genre("Puzzle")]),
        );
        let h = meta_harness(mock.base_url(), |_| {}).await;
        std::fs::write(
            nes_dir(&h).join("Zipped.zip"),
            zip_bytes(&[("inner.nes", inner)]),
        )
        .unwrap();
        let id = game_id(&h, "Zipped").await;
        let d = detail_until(&h, &id, "genre").await;
        assert_eq!(d["genre"], "Puzzle");
        assert_eq!(d["fileName"], "inner.nes");
        assert!(!extracted_dir(&h, &id).exists());
    }

    #[tokio::test]
    async fn name_fallback_exact_and_unambiguous_prefix_only() {
        let mock = httpmock::MockServer::start_async().await;
        rdb_mock(
            &mock,
            build_rdb(&[
                Rec::named("Name Only (USA)")
                    .crc(0x1234)
                    .genre("Sports"),
                Rec::named("Tetris (USA)")
                    .crc(0x2345)
                    .genre("Puzzle"),
                Rec::named("Tetris (EUR)")
                    .crc(0x3456)
                    .genre("Puzzle"),
                Rec::named("Super Mario Land (World)")
                    .crc(0x4567)
                    .genre("Platform"),
            ]),
        );
        // A 4 byte hash cap means no CRC for these tiny files: names only.
        let h = meta_harness(mock.base_url(), |c| c.games_metadata_max_hash_bytes = 4)
            .await;
        for f in ["Tetris.nes", "Super Mario Land.nes"] {
            std::fs::write(nes_dir(&h).join(f), b"some rom bytes").unwrap();
        }
        let id = game_id(&h, "Super Mario Land").await;
        assert_eq!(detail_until(&h, &id, "genre").await["genre"], "Platform");
        // Two equally close games: nothing rather than a guess.
        assert_no_meta(&detail(&h, &game_id(&h, "Tetris").await).await);
    }

    #[tokio::test]
    async fn exact_name_matches_when_the_crc_is_unknown() {
        let mock = httpmock::MockServer::start_async().await;
        rdb_mock(
            &mock,
            build_rdb(&[Rec::named("Name Only (USA)")
                .crc(0x1234)
                .genre("Sports")]),
        );
        let h = meta_harness(mock.base_url(), |_| {}).await;
        std::fs::write(nes_dir(&h).join("Name Only (USA).nes"), b"hacked rom data")
            .unwrap();
        let id = game_id(&h, "Name Only (USA)").await;
        assert_eq!(detail_until(&h, &id, "genre").await["genre"], "Sports");
    }

    #[tokio::test]
    async fn failed_downloads_leave_detail_bare_and_back_off() {
        for broken in [
            (500u16, Vec::new()),
            (200, b"definitely not an rdb".to_vec()),
        ] {
            let mock = httpmock::MockServer::start_async().await;
            let hit = mock.mock(|when, then| {
                when.method(httpmock::Method::GET)
                    .path(NES_RDB_PATH);
                then.status(broken.0)
                    .body(
                        broken
                            .1
                            .clone(),
                    );
            });
            let h = meta_harness(mock.base_url(), |c| c.games_metadata_retry_secs = 1)
                .await;
            let id = game_id(&h, "Super Mario (U)").await;

            assert_no_meta(&detail(&h, &id).await);
            assert_no_meta(&detail(&h, &id).await);
            hit.assert_hits(1);
            assert!(
                !h.data
                    .path()
                    .join(NES_RDB_FILE)
                    .exists()
            );

            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            assert_no_meta(&detail(&h, &id).await);
            hit.assert_hits(2);
        }
    }

    #[tokio::test]
    async fn disabled_metadata_makes_no_requests() {
        let mock = httpmock::MockServer::start_async().await;
        let any = mock.mock(|when, then| {
            when.method(httpmock::Method::GET);
            then.status(200)
                .body(build_rdb(&[mario_record()]));
        });
        let h =
            meta_harness(mock.base_url(), |c| c.games_metadata_enabled = false).await;
        let id = game_id(&h, "Super Mario (U)").await;
        assert_no_meta(&detail(&h, &id).await);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_no_meta(&detail(&h, &id).await);
        any.assert_hits(0);
    }

    fn seed_cached_rdb(h: &Harness, age_days: u64, etag: Option<&str>) -> Vec<u8> {
        let path = h
            .data
            .path()
            .join(NES_RDB_FILE);
        std::fs::create_dir_all(
            path.parent()
                .unwrap(),
        )
        .unwrap();
        let bytes = build_rdb(&[mario_record()]);
        std::fs::write(&path, &bytes).unwrap();
        if let Some(e) = etag {
            std::fs::write(format!("{}.etag", path.display()), e).unwrap();
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now()
                    - std::time::Duration::from_secs(age_days * 24 * 3600),
            )
            .unwrap();
        bytes
    }

    #[tokio::test]
    async fn stale_cache_revalidates_with_etag_and_keeps_the_file_on_304() {
        let mock = httpmock::MockServer::start_async().await;
        let hit = mock.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path(NES_RDB_PATH)
                .header("if-none-match", "\"v1\"");
            then.status(304);
        });
        let h = meta_harness(mock.base_url(), |_| {}).await;
        let before = seed_cached_rdb(&h, 40, Some("\"v1\""));
        let id = game_id(&h, "Super Mario (U)").await;

        assert_eq!(detail_until(&h, &id, "genre").await["genre"], "Platform");
        hit.assert_hits(1);
        let path = h
            .data
            .path()
            .join(NES_RDB_FILE);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Revalidated: the freshness clock restarted.
        let age = std::time::SystemTime::now()
            .duration_since(
                std::fs::metadata(&path)
                    .unwrap()
                    .modified()
                    .unwrap(),
            )
            .unwrap();
        assert!(age < std::time::Duration::from_secs(24 * 3600));
        // And a later request does not ask again.
        detail(&h, &id).await;
        hit.assert_hits(1);
    }

    #[tokio::test]
    async fn fresh_cache_needs_no_network() {
        let mock = httpmock::MockServer::start_async().await;
        let any = mock.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path(NES_RDB_PATH);
            then.status(500);
        });
        let h = meta_harness(mock.base_url(), |_| {}).await;
        seed_cached_rdb(&h, 1, Some("\"v1\""));
        let id = game_id(&h, "Super Mario (U)").await;
        assert_eq!(detail_until(&h, &id, "genre").await["genre"], "Platform");
        any.assert_hits(0);
    }

    #[tokio::test]
    async fn stale_cache_survives_an_unreachable_upstream() {
        let mock = httpmock::MockServer::start_async().await;
        let any = mock.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path(NES_RDB_PATH);
            then.status(503);
        });
        let h = meta_harness(mock.base_url(), |_| {}).await;
        seed_cached_rdb(&h, 90, None);
        let id = game_id(&h, "Super Mario (U)").await;
        assert_eq!(detail_until(&h, &id, "genre").await["genre"], "Platform");
        any.assert_hits(1);
    }

    #[tokio::test]
    async fn local_directory_base_is_read_in_place() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path()
                .join("Nintendo - Nintendo Entertainment System.rdb"),
            build_rdb(&[mario_record()]),
        )
        .unwrap();
        let base = dir
            .path()
            .to_string_lossy()
            .into_owned();
        let h = meta_harness(base, |_| {}).await;
        let id = game_id(&h, "Super Mario (U)").await;
        assert_eq!(detail_until(&h, &id, "genre").await["genre"], "Platform");
        assert!(
            !h.data
                .path()
                .join(NES_RDB_FILE)
                .exists()
        );
    }

    // -----------------------------------------------------------------------
    // LaunchBox descriptions (opt-in)
    // -----------------------------------------------------------------------

    const LB_PATH: &str = "/Metadata.zip";
    const LB_DIR: &str = "games/meta/launchbox";
    const NES: &str = "Nintendo Entertainment System";

    fn lb_game(platform: &str, name: &str, extra: &str) -> String {
        format!(
            "<Game><Name>{name}</Name><Platform>{platform}</Platform>{extra}</Game>"
        )
    }

    fn lb_zip(games: &[String]) -> Vec<u8> {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<LaunchBox>{}\
             <GameAlternateName><AlternateName>ignored</AlternateName></GameAlternateName>\
             </LaunchBox>",
            games.concat()
        );
        zip_bytes(&[("Metadata.xml", xml.as_bytes())])
    }

    fn lb_mock<'a>(
        mock: &'a httpmock::MockServer,
        zip: Vec<u8>,
        etag: &str,
    ) -> httpmock::Mock<'a> {
        let etag = etag.to_string();
        mock.mock(move |when, then| {
            when.method(httpmock::Method::GET)
                .path(LB_PATH);
            then.status(200)
                .header("etag", etag.as_str())
                .body(zip);
        })
    }

    async fn lb_harness(
        base: String,
        zip_url: String,
        tweak: impl FnOnce(&mut Config) + Send + 'static,
    ) -> Harness {
        meta_harness(base, move |c| {
            c.games_launchbox_enabled = true;
            c.games_launchbox_url = zip_url;
            tweak(c);
        })
        .await
    }

    fn lb_dir(h: &Harness) -> std::path::PathBuf {
        h.data
            .path()
            .join(LB_DIR)
    }

    fn mario_lb() -> String {
        lb_game(
            NES,
            "Super Mario",
            "<Overview><![CDATA[Run & jump <fast> through the Mushroom Kingdom.]]> \
             Rescue the princess &amp; save the day. Caf&#233;.</Overview>\
             <Genres>Action</Genres><Developer>Nintendo EAD</Developer>\
             <Publisher>Nintendo</Publisher>\
             <ReleaseDate>1985-09-13T00:00:00-07:00</ReleaseDate>\
             <MaxPlayers>2</MaxPlayers><CommunityRating>4.25</CommunityRating>\
             <CommunityRatingCount>120</CommunityRatingCount>",
        )
    }

    /// Waits until the background build is over (success or failure): the
    /// partial download is gone and at least `hits` requests were made.
    async fn lb_settled(
        dir: &std::path::Path,
        mock_hits: impl Fn() -> usize,
        hits: usize,
    ) {
        for _ in 0..400 {
            let leftovers = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .any(|e| {
                            e.file_name()
                                .to_string_lossy()
                                .starts_with("download-")
                        })
                })
                .unwrap_or(false);
            if mock_hits() >= hits && !leftovers {
                // Give the build task a beat to publish its result.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("LaunchBox update never settled");
    }

    #[tokio::test]
    async fn launchbox_overview_and_rating_reach_the_detail() {
        let mock = httpmock::MockServer::start_async().await;
        let hit = lb_mock(
            &mock,
            lb_zip(&[
                mario_lb(),
                // Not rated: overview only, no rating.
                lb_game(
                    NES,
                    "Zelda",
                    "<Overview>Hyrule.</Overview><CommunityRating>3.0</CommunityRating>\
                     <CommunityRatingCount>0</CommunityRatingCount>",
                ),
                // Systems the library has no games for are not kept.
                lb_game("Super Nintendo Entertainment System", "Super Mario", ""),
                lb_game("Arcade", "Super Mario", ""),
            ]),
            "\"v1\"",
        );
        let h = lb_harness(mock.base_url(), mock.url(LB_PATH), |_| {}).await;
        let id = game_id(&h, "Super Mario (U)").await;

        let d = detail_until(&h, &id, "overview").await;
        assert_eq!(
            d["overview"],
            "Run & jump <fast> through the Mushroom Kingdom. \
             Rescue the princess & save the day. Café."
        );
        // Gap fields come from LaunchBox when no libretro data is around.
        assert_eq!(d["genre"], "Action");
        assert_eq!(d["developer"], "Nintendo EAD");
        assert_eq!(d["publisher"], "Nintendo");
        assert_eq!(d["year"], json!(1985));
        assert_eq!(d["players"], json!(2));
        assert_eq!(d["rating"], json!(4.25));
        assert!(
            d.get("region")
                .is_none()
        );

        let zelda = game_id(&h, "Zelda").await;
        let z = detail_until(&h, &zelda, "overview").await;
        assert_eq!(z["overview"], "Hyrule.");
        assert!(
            z.get("rating")
                .is_none()
        );

        // The list is untouched.
        let list = get_json(&h, &format!("/Moonfin/Games/{}/Games", h.lib)).await;
        for g in list
            .as_array()
            .unwrap()
        {
            assert_no_meta(g);
        }

        // Only the systems in the library were kept (NES and PlayStation).
        hit.assert_hits(1);
        let stamp: Value = serde_json::from_slice(
            &std::fs::read(lb_dir(&h).join("stamp.json")).unwrap(),
        )
        .unwrap();
        let gen_dir = lb_dir(&h).join(format!(
            "gen-{}",
            stamp["generation"]
                .as_str()
                .unwrap()
        ));
        let mut files: Vec<String> = std::fs::read_dir(&gen_dir)
            .unwrap()
            .flatten()
            .map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        files.sort();
        assert_eq!(files, ["nes.json", "psx.json"]);
    }

    #[tokio::test]
    async fn launchbox_matches_by_the_rdb_canonical_name_and_rdb_wins_fields() {
        let mock = httpmock::MockServer::start_async().await;
        let file = b"an oddly named dump";
        rdb_mock(
            &mock,
            build_rdb(&[Rec::named("Super Mario (USA)")
                .crc(crc32fast::hash(file))
                .genre("Platform")
                .region("USA")
                .year(1986)]),
        );
        lb_mock(&mock, lb_zip(&[mario_lb()]), "\"v1\"");
        let h = lb_harness(mock.base_url(), mock.url(LB_PATH), |_| {}).await;
        std::fs::write(nes_dir(&h).join("weird.nes"), file).unwrap();
        let id = game_id(&h, "weird").await;

        let d = detail_until(&h, &id, "overview").await;
        // The file name says nothing; the CRC-matched canonical name does.
        assert!(
            d["overview"]
                .as_str()
                .unwrap()
                .starts_with("Run & jump")
        );
        // rdb wins what it has (genre, year), LaunchBox fills the rest, and
        // region only ever comes from rdb.
        assert_eq!(d["genre"], "Platform");
        assert_eq!(d["year"], json!(1986));
        assert_eq!(d["region"], "USA");
        assert_eq!(d["publisher"], "Nintendo");
        assert_eq!(d["rating"], json!(4.25));
    }

    #[tokio::test]
    async fn launchbox_stays_off_by_default() {
        let mock = httpmock::MockServer::start_async().await;
        let hit = lb_mock(&mock, lb_zip(&[mario_lb()]), "\"v1\"");
        rdb_mock(&mock, build_rdb(&[mario_record()]));
        // The URL is configured but the switch is not flipped.
        let h = meta_harness(mock.base_url(), {
            let url = mock.url(LB_PATH);
            move |c| c.games_launchbox_url = url
        })
        .await;
        let id = game_id(&h, "Super Mario (U)").await;
        let d = detail_until(&h, &id, "genre").await;
        for _ in 0..5 {
            detail(&h, &id).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        hit.assert_hits(0);
        assert!(
            d.get("overview")
                .is_none()
        );
        assert!(!lb_dir(&h).exists());
    }

    #[tokio::test]
    async fn launchbox_alone_needs_no_libretro_metadata() {
        let mock = httpmock::MockServer::start_async().await;
        lb_mock(&mock, lb_zip(&[mario_lb()]), "\"v1\"");
        let h = lb_harness(mock.base_url(), mock.url(LB_PATH), |c| {
            c.games_metadata_enabled = false;
        })
        .await;
        let id = game_id(&h, "Super Mario (U)").await;
        assert!(
            detail_until(&h, &id, "overview").await["overview"]
                .as_str()
                .is_some()
        );
    }

    #[tokio::test]
    async fn launchbox_bad_data_gives_no_data_and_backs_off() {
        let bad: [(&str, Vec<u8>); 3] = [
            (
                "malformed xml",
                zip_bytes(&[(
                    "Metadata.xml",
                    b"<LaunchBox><Game><Name>x</Name><Overview>oops</Game></LaunchBox>"
                        .as_slice(),
                )]),
            ),
            ("not a zip", b"<html>nope</html>".to_vec()),
            (
                "no Metadata.xml",
                zip_bytes(&[("Other.xml", b"<LaunchBox/>".as_slice())]),
            ),
        ];
        for (what, body) in bad {
            let mock = httpmock::MockServer::start_async().await;
            let hit = lb_mock(&mock, body, "\"v1\"");
            let h = lb_harness(mock.base_url(), mock.url(LB_PATH), |_| {}).await;
            let id = game_id(&h, "Super Mario (U)").await;
            lb_settled(&lb_dir(&h), || hit.hits(), 1).await;

            let d = detail(&h, &id).await;
            assert!(
                d.get("overview")
                    .is_none(),
                "{what}: {d}"
            );
            assert!(
                !lb_dir(&h)
                    .join("stamp.json")
                    .exists(),
                "{what}: stamp written"
            );
            let gens = std::fs::read_dir(lb_dir(&h))
                .unwrap()
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("gen-")
                })
                .count();
            assert_eq!(gens, 0, "{what}: partial generation left behind");

            // Backed off: more requests inside the window cause no new
            // download.
            for _ in 0..3 {
                detail(&h, &id).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            hit.assert_hits(1);
        }
    }

    #[tokio::test]
    async fn launchbox_retries_after_the_backoff() {
        let mock = httpmock::MockServer::start_async().await;
        let hit = mock.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path(LB_PATH);
            then.status(500);
        });
        let h = lb_harness(mock.base_url(), mock.url(LB_PATH), |c| {
            c.games_metadata_retry_secs = 1
        })
        .await;
        let id = game_id(&h, "Super Mario (U)").await;
        lb_settled(&lb_dir(&h), || hit.hits(), 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
        detail(&h, &id).await;
        lb_settled(&lb_dir(&h), || hit.hits(), 2).await;
    }

    #[tokio::test]
    async fn launchbox_index_survives_restart_without_reparsing() {
        let mock = httpmock::MockServer::start_async().await;
        let hit = lb_mock(&mock, lb_zip(&[mario_lb()]), "\"v1\"");
        let first = lb_harness(mock.base_url(), mock.url(LB_PATH), |_| {}).await;
        let id = game_id(&first, "Super Mario (U)").await;
        detail_until(&first, &id, "overview").await;
        hit.assert_hits(1);
        let stamp = lb_dir(&first).join("stamp.json");
        let before = std::fs::metadata(&stamp)
            .unwrap()
            .modified()
            .unwrap();
        let data = first
            .data
            .path()
            .to_path_buf();

        // A second service instance on the same data dir.
        let second = lb_harness(mock.base_url(), mock.url(LB_PATH), move |c| {
            c.data_dir = data
        })
        .await;
        let id = game_id(&second, "Super Mario (U)").await;
        let d = detail_until(&second, &id, "overview").await;
        assert_eq!(d["rating"], json!(4.25));
        for _ in 0..3 {
            detail(&second, &id).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        hit.assert_hits(1);
        assert_eq!(
            std::fs::metadata(&stamp)
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
    }

    /// Ages the stamp so the next start considers the data due for a check.
    fn age_stamp(dir: &std::path::Path) {
        let path = dir.join("stamp.json");
        let mut v: Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v["checked_at"] = json!(1);
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn launchbox_refresh_revalidates_with_etag_and_rebuilds_on_change() {
        let mock = httpmock::MockServer::start_async().await;
        let mut first_hit = lb_mock(&mock, lb_zip(&[mario_lb()]), "\"v1\"");
        let first = lb_harness(mock.base_url(), mock.url(LB_PATH), |_| {}).await;
        let id = game_id(&first, "Super Mario (U)").await;
        detail_until(&first, &id, "overview").await;
        let data = first
            .data
            .path()
            .to_path_buf();
        let generation_of = |dir: &std::path::Path| -> String {
            let v: Value =
                serde_json::from_slice(&std::fs::read(dir.join("stamp.json")).unwrap())
                    .unwrap();
            v["generation"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let gen1 = generation_of(&lb_dir(&first));

        // Stale + unchanged upstream: conditional request, 304, no rebuild.
        first_hit.delete();
        let mut not_modified = mock.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path(LB_PATH)
                .header("if-none-match", "\"v1\"");
            then.status(304);
        });
        age_stamp(&lb_dir(&first));
        let d2 = data.clone();
        let second =
            lb_harness(mock.base_url(), mock.url(LB_PATH), move |c| c.data_dir = d2)
                .await;
        let id = game_id(&second, "Super Mario (U)").await;
        lb_settled(&data.join(LB_DIR), || not_modified.hits(), 1).await;
        assert!(
            detail_until(&second, &id, "overview").await["overview"]
                .as_str()
                .is_some()
        );
        assert_eq!(generation_of(&data.join(LB_DIR)), gen1);
        let stamp: Value = serde_json::from_slice(
            &std::fs::read(
                data.join(LB_DIR)
                    .join("stamp.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            stamp["checked_at"]
                .as_u64()
                .unwrap()
                > 1000
        );

        // Stale + changed upstream: full download, atomic swap, old
        // generation removed, new text served.
        not_modified.delete();
        let changed = mock.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path(LB_PATH)
                .header("if-none-match", "\"v1\"");
            then.status(200)
                .header("etag", "\"v2\"")
                .body(lb_zip(&[lb_game(
                    NES,
                    "Super Mario",
                    "<Overview>Second edition.</Overview>",
                )]));
        });
        age_stamp(&data.join(LB_DIR));
        let d3 = data.clone();
        let third =
            lb_harness(mock.base_url(), mock.url(LB_PATH), move |c| c.data_dir = d3)
                .await;
        let id = game_id(&third, "Super Mario (U)").await;
        lb_settled(&data.join(LB_DIR), || changed.hits(), 1).await;
        let mut overview = Value::Null;
        for _ in 0..200 {
            overview = detail(&third, &id).await["overview"].clone();
            if overview == "Second edition." {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(overview, "Second edition.");
        let gen2 = generation_of(&data.join(LB_DIR));
        assert_ne!(gen2, gen1);
        assert!(
            !data
                .join(LB_DIR)
                .join(format!("gen-{gen1}"))
                .exists()
        );
    }

    #[tokio::test]
    async fn launchbox_rebuilds_when_the_library_gains_a_system() {
        let mock = httpmock::MockServer::start_async().await;
        let hit = lb_mock(&mock, lb_zip(&[mario_lb()]), "\"v1\"");
        let first = lb_harness(mock.base_url(), mock.url(LB_PATH), |_| {}).await;
        let id = game_id(&first, "Super Mario (U)").await;
        detail_until(&first, &id, "overview").await;
        let data = first
            .data
            .path()
            .to_path_buf();
        // Pretend the index was built before PlayStation games existed.
        let path = lb_dir(&first).join("stamp.json");
        let mut v: Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v["cores"] = json!(["nes"]);
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();

        let d2 = data.clone();
        let second =
            lb_harness(mock.base_url(), mock.url(LB_PATH), move |c| c.data_dir = d2)
                .await;
        game_id(&second, "Super Mario (U)").await;
        lb_settled(&data.join(LB_DIR), || hit.hits(), 2).await;
        let v: Value = serde_json::from_slice(
            &std::fs::read(
                data.join(LB_DIR)
                    .join("stamp.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(v["cores"], json!(["nes", "psx"]));
    }

    #[tokio::test]
    async fn launchbox_accepts_a_local_zip() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir
            .path()
            .join("Metadata.zip");
        std::fs::write(&zip, lb_zip(&[mario_lb()])).unwrap();
        // Nothing listens on this base: any network attempt would fail.
        let h = lb_harness(
            "http://127.0.0.1:9".to_string(),
            zip.to_string_lossy()
                .into_owned(),
            |c| c.games_metadata_enabled = false,
        )
        .await;
        let id = game_id(&h, "Super Mario (U)").await;
        let d = detail_until(&h, &id, "overview").await;
        assert_eq!(d["rating"], json!(4.25));
        // A local source is read in place, never copied or deleted.
        assert!(zip.is_file());
    }
}
