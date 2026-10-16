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
    Some(
        state
            .ctx
            .games
            .snapshot(
                &state
                    .ctx
                    .config,
            )
            .await,
    )
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
    Json(GameDetailDto {
        summary: game.into(),
        backend_override_supported: false,
        size_bytes: game.size_bytes,
        bios,
    })
    .into_response()
}

/// Artwork is not implemented; the client treats a 404 as "no art" (Moonbase
/// itself answers 404 for missing artwork).
#[get("/moonfin/games/{lib}/thumb/{game_id}")]
pub async fn games_thumb(
    _session: AuthSession,
    Path((_lib, _game_id)): Path<(String, String)>,
) -> StatusCode {
    StatusCode::NOT_FOUND
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
        served = %game.file_name,
        "games: rom request"
    );
    serve_file(
        game.path
            .clone(),
        &game.file_name,
        req,
    )
    .await
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
}
