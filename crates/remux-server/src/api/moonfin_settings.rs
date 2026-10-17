//! Moonfin `Ping` and settings sync (`/Moonfin/Settings/...`).
//!
//! Wire behaviour follows Moonbase's `MoonfinController` as used by the
//! Moonfin client (see `plugin_sync_service.dart`). Per-user, per-profile
//! settings are stored as schemaless JSON objects (`moonfin_settings`
//! table), so keys added by newer clients round-trip untouched.
//!
//! * Profiles: `global`, `desktop`, `mobile`, `tv` (case-insensitive).
//! * Save merges incoming top-level keys into the stored profile (nulls and
//!   blank API keys never overwrite; values are replaced whole).
//! * Resolve layers `[device, global, admin defaults]`, first non-null value
//!   per key wins. A user with nothing stored gets `200 {}` (not 404), or
//!   the admin defaults, so the client's first-sign-in sync can initialise.
//! * Admin defaults live in the database (`moonfin_default_settings`) and are
//!   edited through the admin API in `moonfin_admin`. A handful of keys that
//!   Remux itself owns can never be defaults ([`ADMIN_BLOCKED_KEYS`]).
//! * `/moonfin/settings/stream` is a per-user SSE feed that emits
//!   `settingsUpdated` when a save changed that user's stored settings, and
//!   to every connected user when the admin defaults change.
//!
//! Routes are registered under lowercase paths; `rewrite_request_uri`
//! lowercases the Moonfin client's mixed-case URLs.

use std::{collections::HashMap, convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderName, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use dashmap::DashMap;
use remux_macros::{delete, get, post};
use serde_json::{Map, Value, json};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{error, warn};
use uuid::Uuid;

use crate::{AppContext, AppState, db::auth::AuthSession};

/// Largest accepted profile-save body (real profiles are ~10-30 KB).
const MAX_SAVE_BYTES: usize = 1024 * 1024;
/// SSE heartbeat; the client treats 75 s of silence as a dead stream.
const HEARTBEAT: Duration = Duration::from_secs(30);

const VALID_PROFILES: [&str; 4] = ["global", "desktop", "mobile", "tv"];

/// Keys the admin defaults may not carry, because Remux already owns the
/// setting: they duplicate the Jellyfin user configuration (audio/subtitle
/// language and mode, autoplay) that Remux uses to pick default streams.
/// Users can still sync them per profile; only the shared defaults layer
/// refuses them.
pub(crate) const ADMIN_BLOCKED_KEYS: [&str; 5] = [
    "defaultAudioLanguage",
    "preferDefaultAudioTrack",
    "autoplayNextEpisode",
    "defaultSubtitleLanguage",
    "subtitleMode",
];

pub(crate) type Profile = Map<String, Value>;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Process-wide Moonfin settings state held in [`crate::AppContext`].
#[derive(Clone, Default)]
pub struct SettingsHub {
    inner: Arc<HubInner>,
}

/// An event pushed down every open settings stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamEvent {
    /// Stored or default settings changed; the client re-resolves them.
    SettingsUpdated,
}

impl StreamEvent {
    /// The SSE frame for this event.
    fn frame(self) -> Bytes {
        match self {
            Self::SettingsUpdated => {
                Bytes::from_static(b"data: {\"type\":\"settingsUpdated\"}\n\n")
            }
        }
    }
}

#[derive(Default)]
struct HubInner {
    /// One broadcast channel per user with at least one open stream.
    streams: DashMap<Uuid, broadcast::Sender<StreamEvent>>,
    /// Serializes read-modify-write saves (low volume, so one lock is fine).
    write_lock: tokio::sync::Mutex<()>,
}

impl SettingsHub {
    /// Opens a settings stream for `user`.
    fn subscribe(&self, user: Uuid) -> StreamGuard {
        let rx = self
            .inner
            .streams
            .entry(user)
            .or_insert_with(|| broadcast::channel(16).0)
            .subscribe();
        StreamGuard {
            hub: self.clone(),
            user,
            rx: Some(rx),
        }
    }

    /// Sends `event` to `user`'s open streams, if any.
    pub(crate) fn notify(&self, user: Uuid, event: StreamEvent) {
        if let Some(tx) = self
            .inner
            .streams
            .get(&user)
        {
            // No receivers is fine: nobody is listening.
            let _ = tx.send(event);
        }
    }

    /// Sends `event` to every user with an open stream.
    pub(crate) fn notify_all(&self, event: StreamEvent) {
        for tx in self
            .inner
            .streams
            .iter()
        {
            let _ = tx.send(event);
        }
    }

    /// Serializes writers of Moonfin settings (user profiles and defaults).
    pub(crate) async fn write_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner
            .write_lock
            .lock()
            .await
    }
}

/// Holds a stream's receiver and drops the user's channel once the last
/// stream for that user disconnects.
struct StreamGuard {
    hub: SettingsHub,
    user: Uuid,
    rx: Option<broadcast::Receiver<StreamEvent>>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.rx
            .take();
        self.hub
            .inner
            .streams
            .remove_if(&self.user, |_, tx| tx.receiver_count() == 0);
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

pub(crate) fn error_response(
    status: StatusCode,
    message: impl Into<String>,
) -> Response {
    (status, Json(json!({ "Error": message.into() }))).into_response()
}

pub(crate) fn internal_error(err: impl std::fmt::Display) -> Response {
    error!(%err, "moonfin settings storage error");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
}

/// Lowercased profile name if it is one of the four valid profiles.
fn parse_profile(raw: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    VALID_PROFILES
        .contains(&lower.as_str())
        .then_some(lower)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Merges `incoming` into `stored` (top-level keys, whole-value replace).
fn merge_profile(stored: &mut Profile, incoming: &Profile) {
    let present = |k: &str| {
        incoming
            .get(k)
            .is_some_and(|v| !v.is_null())
    };
    // A row order pushed without sections means the sections were removed.
    if !present("homeSections") && present("homeRowOrder") {
        stored.remove("homeSections");
    }
    for (key, value) in incoming {
        if value.is_null() {
            continue;
        }
        if key.contains("ApiKey")
            && let Some(s) = value.as_str()
            && (s
                .trim()
                .is_empty()
                || s == "null")
        {
            continue;
        }
        stored.insert(key.clone(), value.clone());
    }
}

/// Row order derived from home sections: enabled, non-dynamic, typed
/// sections sorted by `order` (missing sorts last).
fn derive_row_order(sections: &Value) -> Option<Value> {
    let mut rows: Vec<(f64, &str)> = sections
        .as_array()?
        .iter()
        .filter_map(|s| {
            let s = s.as_object()?;
            let kind = s
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("");
            if kind.eq_ignore_ascii_case("pluginDynamic") {
                return None;
            }
            if s.get("enabled") == Some(&Value::Bool(false)) {
                return None;
            }
            let ty = s
                .get("type")
                .and_then(Value::as_str)?;
            if ty
                .trim()
                .is_empty()
                || ty.eq_ignore_ascii_case("none")
            {
                return None;
            }
            let order = s
                .get("order")
                .and_then(Value::as_f64)
                .unwrap_or(f64::MAX);
            Some((order, ty))
        })
        .collect();
    rows.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
    });
    if rows.is_empty() {
        return None;
    }
    Some(Value::Array(
        rows.into_iter()
            .map(|(_, ty)| Value::String(ty.to_string()))
            .collect(),
    ))
}

/// Resolves a profile from its layers, highest precedence first.
fn resolve(layers: &[&Profile]) -> Profile {
    let mut out = Profile::new();
    for layer in layers {
        for (key, value) in layer.iter() {
            if !value.is_null() && !out.contains_key(key) {
                out.insert(key.clone(), value.clone());
            }
        }
    }

    // The home layout is taken as a pair from a single layer.
    out.remove("homeSections");
    out.remove("homeRowOrder");
    let has = |layer: &Profile, k: &str| {
        layer
            .get(k)
            .is_some_and(|v| !v.is_null())
    };
    if let Some(layer) = layers
        .iter()
        .find(|l| has(l, "homeSections") || has(l, "homeRowOrder"))
    {
        let sections = layer
            .get("homeSections")
            .filter(|v| !v.is_null());
        let order = layer
            .get("homeRowOrder")
            .filter(|v| !v.is_null())
            .cloned()
            .or_else(|| sections.and_then(derive_row_order));
        if let Some(s) = sections {
            out.insert("homeSections".into(), s.clone());
        }
        if let Some(o) = order {
            out.insert("homeRowOrder".into(), o);
        }
    }

    // The client names this source differently from Moonbase's stored value.
    if let Some(Value::Array(sources)) = out.get_mut("mdblistRatingSources") {
        for s in sources.iter_mut() {
            if s.as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case("rtAudience"))
            {
                *s = Value::String("tomatoes_audience".into());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

fn parse_stored(data: &str) -> Profile {
    match serde_json::from_str::<Value>(data) {
        Ok(Value::Object(map)) => map,
        _ => Profile::new(),
    }
}

async fn load_profiles(
    db: &SqlitePool,
    user: Uuid,
) -> Result<HashMap<String, (Profile, i64, Option<String>)>, sqlx::Error> {
    let rows: Vec<(String, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT profile, data, updated_at, updated_by FROM moonfin_settings \
         WHERE user_id = ?1",
    )
    .bind(user)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(p, d, at, by)| (p, (parse_stored(&d), at, by)))
        .collect())
}

// ---------------------------------------------------------------------------
// Admin defaults
// ---------------------------------------------------------------------------

/// The admin default settings as stored.
pub(crate) struct StoredDefaults {
    pub settings: Profile,
    /// Epoch milliseconds.
    pub updated_at: i64,
}

/// The [`ADMIN_BLOCKED_KEYS`] present in `settings`, in blocklist order.
pub(crate) fn blocked_keys_in(settings: &Profile) -> Vec<&'static str> {
    ADMIN_BLOCKED_KEYS
        .into_iter()
        .filter(|k| settings.contains_key(*k))
        .collect()
}

/// Admin defaults from the database, or `None` when none were ever saved.
/// Blocked keys that older data may still carry are dropped on the way out,
/// so they can never reach a resolved profile.
pub(crate) async fn load_defaults(
    db: &SqlitePool,
) -> Result<Option<StoredDefaults>, sqlx::Error> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT data, updated_at FROM moonfin_default_settings WHERE id = 1",
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(data, updated_at)| {
        let mut settings = parse_stored(&data);
        for key in ADMIN_BLOCKED_KEYS {
            settings.remove(key);
        }
        StoredDefaults {
            settings,
            updated_at,
        }
    }))
}

/// Replaces the admin defaults. The caller has already rejected blocked
/// keys. Connected users are told to re-resolve when the stored value
/// actually changed.
pub(crate) async fn save_defaults(
    ctx: &AppContext,
    settings: Profile,
    updated_by: Uuid,
) -> Result<StoredDefaults, sqlx::Error> {
    let _write = ctx
        .moonfin_settings
        .write_lock()
        .await;
    let before = load_defaults(&ctx.db).await?;
    if let Some(existing) = before
        && existing.settings == settings
    {
        return Ok(existing);
    }
    let updated_at = now_ms();
    sqlx::query(
        "INSERT INTO moonfin_default_settings (id, data, updated_at, updated_by) \
         VALUES (1, ?1, ?2, ?3) \
         ON CONFLICT(id) DO UPDATE SET data = excluded.data, \
         updated_at = excluded.updated_at, updated_by = excluded.updated_by",
    )
    .bind(Value::Object(settings.clone()).to_string())
    .bind(updated_at)
    .bind(updated_by)
    .execute(&ctx.db)
    .await?;
    ctx.moonfin_settings
        .notify_all(StreamEvent::SettingsUpdated);
    Ok(StoredDefaults {
        settings,
        updated_at,
    })
}

/// One-time seed from `Config.moonfin_default_settings_path`: when the file
/// is configured and the database holds no defaults yet, its contents become
/// the stored defaults (minus blocked keys). Afterwards the database wins and
/// the file is never read again. Problems are logged, never fatal.
pub(crate) async fn import_defaults_file(ctx: &AppContext) {
    let Some(path) = ctx
        .config
        .moonfin_default_settings_path
        .as_deref()
    else {
        return;
    };
    let _write = ctx
        .moonfin_settings
        .write_lock()
        .await;
    match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM moonfin_default_settings)",
    )
    .fetch_one(&ctx.db)
    .await
    {
        Ok(false) => {}
        Ok(true) => return,
        Err(e) => {
            error!(%e, "could not check stored Moonfin default settings");
            return;
        }
    }
    let loaded = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| e.to_string())
        .and_then(|s| match serde_json::from_str::<Value>(&s) {
            Ok(Value::Object(map)) => Ok(map),
            Ok(_) => Err("expected a JSON object".to_string()),
            Err(e) => Err(e.to_string()),
        });
    let mut settings = match loaded {
        Ok(map) => map,
        Err(e) => {
            warn!(path, err = %e, "ignoring Moonfin default settings file");
            return;
        }
    };
    let dropped = blocked_keys_in(&settings);
    for key in &dropped {
        settings.remove(*key);
    }
    let count = settings.len();
    let stored = sqlx::query(
        "INSERT INTO moonfin_default_settings (id, data, updated_at, updated_by) \
         VALUES (1, ?1, ?2, NULL)",
    )
    .bind(Value::Object(settings).to_string())
    .bind(now_ms())
    .execute(&ctx.db)
    .await;
    match stored {
        Ok(_) => tracing::info!(
            path,
            keys = count,
            ignored = ?dropped,
            "imported Moonfin default settings file into the database"
        ),
        Err(e) => error!(%e, "could not store imported Moonfin default settings"),
    }
}

// ---------------------------------------------------------------------------
// Ping
// ---------------------------------------------------------------------------

#[get("/moonfin/ping")]
pub async fn moonfin_ping(
    State(state): State<AppState>,
    _session: AuthSession,
) -> Response {
    Json(json!({
        "installed": true,
        "version": env!("CARGO_PKG_VERSION"),
        "settingsSyncEnabled": true,
        "serverName": "Remux",
        "seerrEnabled": false,
        "seerrUrl": null,
        "mdblistAvailable": state.ctx.config.mdblist_key().is_some(),
        "tmdbAvailable": false,
        "tmdbThumbsSupported": false,
        "messagesSupported": false,
        "recommendationsSupported": true,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Seerr stubs: the plugin is "installed" but Seerr is never configured, so
// the client's probes return at once without attempting a sign-in.
// ---------------------------------------------------------------------------

#[get("/moonfin/seerr/config")]
pub async fn moonfin_seerr_config(_session: AuthSession) -> Response {
    Json(json!({ "enabled": false, "userEnabled": false })).into_response()
}

#[get("/moonfin/seerr/status")]
pub async fn moonfin_seerr_status(_session: AuthSession) -> Response {
    Json(json!({ "enabled": false, "authenticated": false })).into_response()
}

// ---------------------------------------------------------------------------
// Resolve / read
// ---------------------------------------------------------------------------

#[get("/moonfin/settings/resolved/{profile}")]
pub async fn moonfin_settings_resolved(
    State(state): State<AppState>,
    session: AuthSession,
    Path(profile): Path<String>,
) -> Response {
    let Some(profile) = parse_profile(&profile) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid profile: {profile}. Valid profiles: global, desktop, mobile, tv"
            ),
        );
    };
    match resolved_profile(
        &state,
        session
            .user
            .id,
        &profile,
    )
    .await
    {
        Ok(resolved) => Json(Value::Object(resolved)).into_response(),
        Err(e) => internal_error(e),
    }
}

/// Resolves `profile` (already validated lowercase) for `user` from
/// `[device, global, admin defaults]`.
pub(crate) async fn resolved_profile(
    state: &AppState,
    user: Uuid,
    profile: &str,
) -> Result<Profile, sqlx::Error> {
    let stored = load_profiles(
        &state
            .ctx
            .db,
        user,
    )
    .await?;
    let defaults = load_defaults(
        &state
            .ctx
            .db,
    )
    .await?
    .map(|d| d.settings)
    .unwrap_or_default();
    let mut layers: Vec<&Profile> = Vec::with_capacity(3);
    if profile != "global"
        && let Some((device, ..)) = stored.get(profile)
    {
        layers.push(device);
    }
    if let Some((global, ..)) = stored.get("global") {
        layers.push(global);
    }
    layers.push(&defaults);
    Ok(resolve(&layers))
}

#[get("/moonfin/settings")]
pub async fn moonfin_settings_get(
    State(state): State<AppState>,
    session: AuthSession,
) -> Response {
    let user = session
        .user
        .id;
    let stored = match load_profiles(
        &state
            .ctx
            .db,
        user,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return internal_error(e),
    };
    if stored.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "Error": "No settings found for user",
                "UserId": user.to_string(),
            })),
        )
            .into_response();
    }
    let latest = stored
        .values()
        .max_by_key(|(_, at, _)| *at);
    let mut envelope = json!({
        "schemaVersion": 2,
        "lastUpdated": latest.map(|(_, at, _)| *at).unwrap_or_default(),
        "lastUpdatedBy": latest.and_then(|(_, _, by)| by.clone()),
        "syncEnabled": true,
    });
    for name in VALID_PROFILES {
        if let Some((profile, ..)) = stored.get(name) {
            envelope[name] = Value::Object(profile.clone());
        }
    }
    Json(envelope).into_response()
}

// ---------------------------------------------------------------------------
// Save
// ---------------------------------------------------------------------------

/// Case-insensitive top-level key lookup (ASP.NET binding is case-insensitive).
fn field<'a>(body: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    body.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

#[post("/moonfin/settings/profile/{profile}")]
pub async fn moonfin_settings_save(
    State(state): State<AppState>,
    session: AuthSession,
    Path(profile): Path<String>,
    req: Request<Body>,
) -> Response {
    let Some(profile) = parse_profile(&profile) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid profile: {profile}"),
        );
    };
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_SAVE_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body too large",
            );
        }
    };
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&bytes) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Profile settings are required",
        );
    };
    let incoming = match field(&body, "profile") {
        Some(Value::Object(map)) => map,
        Some(Value::Null) | None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Profile settings are required",
            );
        }
        Some(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Profile settings must be a JSON object",
            );
        }
    };
    let client_id = field(&body, "clientId")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    let user = session
        .user
        .id;
    let hub = &state
        .ctx
        .moonfin_settings;
    let _write = hub
        .write_lock()
        .await;
    let db = &state
        .ctx
        .db;
    let result: Result<(bool, bool), sqlx::Error> = async {
        let mut tx = db
            .begin()
            .await?;
        let had_any: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM moonfin_settings WHERE user_id = ?1)")
                .bind(user)
                .fetch_one(&mut *tx)
                .await?;
        let row: Option<String> = sqlx::query_scalar(
            "SELECT data FROM moonfin_settings WHERE user_id = ?1 AND profile = ?2",
        )
        .bind(user)
        .bind(&profile)
        .fetch_optional(&mut *tx)
        .await?;
        let before = row
            .as_deref()
            .map(parse_stored)
            .unwrap_or_default();
        let mut after = before.clone();
        merge_profile(&mut after, incoming);
        let changed = after != before;
        if changed || row.is_none() {
            sqlx::query(
                "INSERT INTO moonfin_settings (user_id, profile, data, updated_at, updated_by) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(user_id, profile) DO UPDATE SET \
                 data = excluded.data, updated_at = excluded.updated_at, \
                 updated_by = excluded.updated_by",
            )
            .bind(user)
            .bind(&profile)
            .bind(Value::Object(after).to_string())
            .bind(now_ms())
            .bind(client_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit()
            .await?;
        Ok((!had_any, changed))
    }
    .await;
    let (created, changed) = match result {
        Ok(r) => r,
        Err(e) => return internal_error(e),
    };
    if changed {
        hub.notify(user, StreamEvent::SettingsUpdated);
    }
    Json(json!({
        "Success": true,
        "Created": created,
        "UserId": user.to_string(),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Delete (no SSE notification, matching Moonbase)
// ---------------------------------------------------------------------------

#[delete("/moonfin/settings")]
pub async fn moonfin_settings_delete_all(
    State(state): State<AppState>,
    session: AuthSession,
) -> Response {
    let _write = state
        .ctx
        .moonfin_settings
        .write_lock()
        .await;
    match sqlx::query("DELETE FROM moonfin_settings WHERE user_id = ?1")
        .bind(
            session
                .user
                .id,
        )
        .execute(
            &state
                .ctx
                .db,
        )
        .await
    {
        Ok(_) => Json(json!({ "Success": true, "Message": "Settings deleted" }))
            .into_response(),
        Err(e) => internal_error(e),
    }
}

#[delete("/moonfin/settings/profile/{profile}")]
pub async fn moonfin_settings_delete_profile(
    State(state): State<AppState>,
    session: AuthSession,
    Path(profile): Path<String>,
) -> Response {
    // `global` is rejected before validity, like Moonbase.
    if profile.eq_ignore_ascii_case("global") {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Cannot delete the global profile. Use DELETE /Settings to remove all settings.",
        );
    }
    let Some(name) = parse_profile(&profile) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid profile: {profile}"),
        );
    };
    let _write = state
        .ctx
        .moonfin_settings
        .write_lock()
        .await;
    match sqlx::query(
        "DELETE FROM moonfin_settings WHERE user_id = ?1 AND profile = ?2",
    )
    .bind(
        session
            .user
            .id,
    )
    .bind(&name)
    .execute(
        &state
            .ctx
            .db,
    )
    .await
    {
        Ok(_) => Json(json!({
            "Success": true,
            "Message": format!("Profile '{name}' deleted"),
        }))
        .into_response(),
        Err(e) => internal_error(e),
    }
}

// ---------------------------------------------------------------------------
// SSE stream
// ---------------------------------------------------------------------------

#[get("/moonfin/settings/stream")]
pub async fn moonfin_settings_stream(
    State(state): State<AppState>,
    session: AuthSession,
) -> Response {
    let mut guard = state
        .ctx
        .moonfin_settings
        .subscribe(
            session
                .user
                .id,
        );
    // Raw frames rather than `axum::response::Sse`, which writes comments as
    // `: text` while the contract (and Moonbase) uses `:connected` exactly.
    let stream = async_stream::stream! {
        // Sent immediately so the client's open call completes at once.
        yield Ok::<Bytes, Infallible>(Bytes::from_static(b":connected\n\n"));
        loop {
            let Some(rx) = guard.rx.as_mut() else { break };
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(event) => yield Ok(event.frame()),
                    // A lagged receiver missed events; one refresh covers them.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        yield Ok(StreamEvent::SettingsUpdated.frame());
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                // Silence for a full interval: keep the connection alive.
                _ = tokio::time::sleep(HEARTBEAT) => {
                    yield Ok(Bytes::from_static(b":heartbeat\n\n"));
                }
            }
        }
    };
    (
        [
            (header::CONTENT_TYPE, "text/event-stream; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use crate::{
        Config, init_app_with_ctx,
        integration_test::auth_header_with_token,
        integration_test::{AUTH_HEADER, TestGuard},
        rewrite_request_uri,
    };
    use axum::Router;
    use axum_test::{TestResponse, TestServer};
    use http::{StatusCode, header};
    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tower::Layer;
    use tower::util::MapRequestLayer;

    struct Harness {
        server: TestServer,
        app: Router,
        _guard: TestGuard,
        token: String,
        _data: tempfile::TempDir,
    }

    impl Harness {
        fn auth_for(token: &str) -> http::HeaderValue {
            http::HeaderValue::from_str(&auth_header_with_token(token)).unwrap()
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

        /// Creates another regular user and returns its access token.
        async fn second_user(&self) -> String {
            self.server
                .post("/Users/New")
                .add_header(header::AUTHORIZATION, Self::auth_for(&self.token))
                .json(&json!({ "Name": "second", "Password": "pw" }))
                .await;
            self.login("second", "pw")
                .await
        }

        async fn get(&self, token: &str, path: &str) -> TestResponse {
            self.server
                .get(path)
                .add_header(header::AUTHORIZATION, Self::auth_for(token))
                .await
        }

        async fn post(&self, token: &str, path: &str, body: &Value) -> TestResponse {
            self.server
                .post(path)
                .add_header(header::AUTHORIZATION, Self::auth_for(token))
                .json(body)
                .await
        }

        async fn delete(&self, token: &str, path: &str) -> TestResponse {
            self.server
                .delete(path)
                .add_header(header::AUTHORIZATION, Self::auth_for(token))
                .await
        }

        /// Pushes `profile` the way the Flutter client does.
        async fn push(
            &self,
            token: &str,
            profile: &str,
            settings: Value,
        ) -> TestResponse {
            self.post(
                token,
                &format!("/Moonfin/Settings/Profile/{profile}"),
                &json!({ "profile": settings, "clientId": "moonfin-flutter" }),
            )
            .await
        }

        /// Resolved settings for `profile`, asserting a 200.
        async fn resolved(&self, token: &str, profile: &str) -> Value {
            let resp = self
                .get(
                    token,
                    &format!("/Moonfin/Settings/Resolved/{profile}?client=moonfin"),
                )
                .await;
            resp.assert_status_ok();
            resp.json::<Value>()
        }
    }

    /// Server behind the production URI rewrite layer, so mixed-case Moonfin
    /// URLs reach the lowercase routes (as in `bind_and_serve`).
    async fn harness() -> Harness {
        harness_with(|_, _| {}).await
    }

    async fn harness_with(
        tweak: impl FnOnce(&mut Config, &std::path::Path),
    ) -> Harness {
        let data = tempfile::tempdir().unwrap();
        let mut config = Config {
            database_url: Some("sqlite::memory:".into()),
            torrent_http_port: None,
            torrent_peer_port: None,
            disable_dht: true,
            data_dir: data
                .path()
                .to_path_buf(),
            ..Default::default()
        };
        tweak(&mut config, data.path());
        let (router, ctx) = init_app_with_ctx(config)
            .await
            .unwrap();
        let app = Router::new()
            .fallback_service(MapRequestLayer::new(rewrite_request_uri).layer(router));
        let server = TestServer::builder()
            .mock_transport()
            .build(app.clone())
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
            app,
            _guard: TestGuard(ctx),
            token: String::new(),
            _data: data,
        };
        h.token = h
            .login("test", "test")
            .await;
        h
    }

    /// Harness whose admin-defaults file contains `contents` verbatim.
    async fn harness_with_defaults(contents: &str) -> Harness {
        let contents = contents.to_string();
        harness_with(move |config, dir| {
            let path = dir.join("moonfin-defaults.json");
            std::fs::write(&path, contents).unwrap();
            config.moonfin_default_settings_path = Some(
                path.to_string_lossy()
                    .into_owned(),
            );
        })
        .await
    }

    fn normalized_guid(s: &str) -> String {
        s.replace('-', "")
            .to_lowercase()
    }

    // ------------------------------------------------------------------
    // Ping (spec P1-P4)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn ping_requires_authentication() {
        let h = harness().await;
        h.server
            .get("/Moonfin/Ping")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ping_reports_settings_sync_and_no_optional_features() {
        let h = harness().await;
        let resp = h
            .get(&h.token, "/Moonfin/Ping")
            .await;
        resp.assert_status_ok();
        let content_type = resp
            .header(header::CONTENT_TYPE)
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            content_type.starts_with("application/json"),
            "{content_type}"
        );
        let mut body = resp.json::<Value>();
        let obj = body
            .as_object_mut()
            .unwrap();
        for key in ["version", "serverName"] {
            let value = obj
                .remove(key)
                .unwrap_or_else(|| panic!("missing {key}"));
            assert!(
                value
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
                "{key} must be a non-empty string"
            );
        }
        assert_eq!(
            body,
            json!({
                "installed": true,
                "settingsSyncEnabled": true,
                "seerrEnabled": false,
                "seerrUrl": null,
                "mdblistAvailable": false,
                "tmdbAvailable": false,
                "tmdbThumbsSupported": false,
                "messagesSupported": false,
                "recommendationsSupported": true,
            })
        );
    }

    #[tokio::test]
    async fn ping_does_not_depend_on_games_and_never_leaks_defaults() {
        // Games are off (default) and an admin defaults file holds a secret.
        let h =
            harness_with_defaults(r#"{"tmdbApiKey":"SECRET","posterSize":"small"}"#)
                .await;
        let resp = h
            .get(&h.token, "/Moonfin/Ping")
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        assert_eq!(body["installed"], json!(true));
        assert_eq!(body["settingsSyncEnabled"], json!(true));
        assert!(
            body.get("defaultSettings")
                .is_none()
        );
        assert!(
            !body
                .to_string()
                .contains("SECRET")
        );
    }

    #[tokio::test]
    async fn ping_routes_with_emby_prefix_and_any_case() {
        let h = harness().await;
        for path in ["/emby/Moonfin/Ping", "/emby/moonfin/ping", "/MOONFIN/PING"] {
            let resp = h
                .get(&h.token, path)
                .await;
            resp.assert_status_ok();
            assert_eq!(resp.json::<Value>()["installed"], json!(true), "{path}");
        }
    }

    // ------------------------------------------------------------------
    // Resolved: basics (E1, E8, S11)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn resolved_requires_authentication() {
        let h = harness().await;
        h.server
            .get("/Moonfin/Settings/Resolved/Mobile")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn resolved_for_user_without_settings_is_empty_object_not_404() {
        let h = harness().await;
        for profile in ["Desktop", "Mobile", "TV", "Global"] {
            assert_eq!(
                h.resolved(&h.token, profile)
                    .await,
                json!({}),
                "{profile}"
            );
        }
    }

    #[tokio::test]
    async fn resolved_rejects_unknown_profile_with_400() {
        let h = harness().await;
        for profile in ["bogus", "phone"] {
            h.get(&h.token, &format!("/Moonfin/Settings/Resolved/{profile}"))
                .await
                .assert_status(StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn profile_names_are_case_insensitive() {
        let h = harness().await;
        h.push(&h.token, "TV", json!({ "a": 1 }))
            .await
            .assert_status_ok();
        for profile in ["tv", "TV", "Tv"] {
            assert_eq!(
                h.resolved(&h.token, profile)
                    .await,
                json!({ "a": 1 }),
                "{profile}"
            );
        }
        // The device profile does not leak into other devices.
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({})
        );
    }

    // ------------------------------------------------------------------
    // Save (S1-S19)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn save_requires_authentication_and_stores_nothing() {
        let h = harness().await;
        h.server
            .post("/Moonfin/Settings/Profile/global")
            .json(&json!({ "profile": { "a": 1 } }))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({})
        );
    }

    #[tokio::test]
    async fn first_save_reports_success_created_and_user_id() {
        let h = harness().await;
        let me = h
            .get(&h.token, "/Users/Me")
            .await
            .json::<Value>();
        let resp = h
            .push(&h.token, "Global", json!({ "a": 1, "b": "x" }))
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        assert_eq!(body["Success"], json!(true));
        assert_eq!(body["Created"], json!(true));
        assert_eq!(
            normalized_guid(
                body["UserId"]
                    .as_str()
                    .unwrap()
            ),
            normalized_guid(
                me["Id"]
                    .as_str()
                    .unwrap()
            )
        );
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1, "b": "x" })
        );
    }

    #[tokio::test]
    async fn created_is_per_user_not_per_profile() {
        let h = harness().await;
        let first = h
            .push(&h.token, "tv", json!({ "a": 1 }))
            .await
            .json::<Value>();
        let second = h
            .push(&h.token, "desktop", json!({ "a": 2 }))
            .await
            .json::<Value>();
        let third = h
            .push(&h.token, "tv", json!({ "a": 3 }))
            .await
            .json::<Value>();
        assert_eq!(first["Created"], json!(true));
        assert_eq!(second["Created"], json!(false));
        assert_eq!(third["Created"], json!(false));
    }

    #[tokio::test]
    async fn save_merges_top_level_keys_and_keeps_untouched_ones() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": 1, "b": "x" }))
            .await;
        let resp = h
            .push(&h.token, "global", json!({ "b": "y", "c": true }))
            .await;
        assert_eq!(resp.json::<Value>()["Created"], json!(false));
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1, "b": "y", "c": true })
        );
    }

    #[tokio::test]
    async fn null_values_never_overwrite_stored_ones() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        h.push(&h.token, "global", json!({ "a": null, "b": null }))
            .await
            .assert_status_ok();
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1 })
        );
    }

    #[tokio::test]
    async fn blank_api_keys_are_skipped_but_real_ones_replace() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "tmdbApiKey": "K" }))
            .await;
        for blank in ["", "   ", "null"] {
            h.push(&h.token, "global", json!({ "tmdbApiKey": blank }))
                .await
                .assert_status_ok();
            assert_eq!(
                h.resolved(&h.token, "global")
                    .await,
                json!({ "tmdbApiKey": "K" }),
                "blank {blank:?} must not clear the key"
            );
        }
        h.push(&h.token, "global", json!({ "tmdbApiKey": "K2" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "tmdbApiKey": "K2" })
        );
    }

    #[tokio::test]
    async fn blank_value_for_non_api_key_field_is_stored() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "customThemeId": "x" }))
            .await;
        h.push(&h.token, "global", json!({ "customThemeId": "" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "customThemeId": "" })
        );
    }

    #[tokio::test]
    async fn nested_values_are_replaced_whole_not_deep_merged() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({ "seerrRows": { "trendingMovies": true, "rowOrder": ["a"] } }),
        )
        .await;
        h.push(
            &h.token,
            "global",
            json!({ "seerrRows": { "rowOrder": ["b"] } }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "seerrRows": { "rowOrder": ["b"] } })
        );
    }

    #[tokio::test]
    async fn pushing_row_order_alone_drops_stored_home_sections() {
        let h = harness().await;
        let s1 = json!([{ "type": "resume", "order": 0, "enabled": true }]);
        h.push(
            &h.token,
            "global",
            json!({ "homeSections": s1, "homeRowOrder": ["a"] }),
        )
        .await;
        h.push(&h.token, "global", json!({ "homeRowOrder": ["b"] }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "homeRowOrder": ["b"] })
        );
    }

    #[tokio::test]
    async fn pushing_sections_with_row_order_replaces_both() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({
                "homeSections": [{ "type": "resume", "order": 0, "enabled": true }],
                "homeRowOrder": ["a"],
            }),
        )
        .await;
        let s2 = json!([{ "type": "nextUp", "order": 0, "enabled": true }]);
        h.push(
            &h.token,
            "global",
            json!({ "homeSections": s2, "homeRowOrder": ["b"] }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "homeSections": s2, "homeRowOrder": ["b"] })
        );
    }

    #[tokio::test]
    async fn unrelated_push_keeps_stored_home_layout() {
        let h = harness().await;
        let sections = json!([{ "type": "resume", "order": 0, "enabled": true }]);
        h.push(
            &h.token,
            "global",
            json!({ "homeSections": sections, "homeRowOrder": ["a"] }),
        )
        .await;
        h.push(&h.token, "global", json!({ "posterSize": "s" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({
                "homeSections": sections,
                "homeRowOrder": ["a"],
                "posterSize": "s",
            })
        );
    }

    #[tokio::test]
    async fn saving_a_device_profile_leaves_global_untouched() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        h.push(&h.token, "tv", json!({ "a": 2 }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "a": 2 })
        );
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1 })
        );
    }

    #[tokio::test]
    async fn save_rejects_unknown_profile_name() {
        let h = harness().await;
        h.push(&h.token, "phone", json!({ "a": 1 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({})
        );
    }

    #[tokio::test]
    async fn save_rejects_missing_null_or_non_object_profile() {
        let h = harness().await;
        for body in [
            json!({ "clientId": "c" }),
            json!({ "profile": null }),
            json!({ "profile": [1, 2] }),
            json!({ "profile": "str" }),
        ] {
            h.post(&h.token, "/Moonfin/Settings/Profile/global", &body)
                .await
                .assert_status(StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({})
        );
    }

    #[tokio::test]
    async fn save_rejects_oversized_body_with_413() {
        let h = harness().await;
        let big = "x".repeat(1024 * 1024 + 1024);
        h.push(&h.token, "global", json!({ "blob": big }))
            .await
            .assert_status(StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({})
        );
    }

    #[tokio::test]
    async fn unknown_keys_are_preserved_verbatim() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({ "zzzFuture": { "n": 1, "list": [true, null] }, "posterSize": "l" }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "zzzFuture": { "n": 1, "list": [true, null] }, "posterSize": "l" })
        );
    }

    #[tokio::test]
    async fn settings_are_isolated_per_user() {
        let h = harness().await;
        let other = h
            .second_user()
            .await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        // The second user has nothing yet, and their first save is "created".
        assert_eq!(
            h.resolved(&other, "global")
                .await,
            json!({})
        );
        let resp = h
            .push(&other, "global", json!({ "a": 9 }))
            .await;
        assert_eq!(resp.json::<Value>()["Created"], json!(true));
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1 })
        );
        assert_eq!(
            h.resolved(&other, "global")
                .await,
            json!({ "a": 9 })
        );
    }

    // ------------------------------------------------------------------
    // Resolve precedence (R1-R15, E2, E3)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn device_profile_overrides_global_for_that_device_only() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "posterSize": "large" }))
            .await;
        h.push(&h.token, "desktop", json!({ "posterSize": "small" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await["posterSize"],
            json!("small")
        );
        assert_eq!(
            h.resolved(&h.token, "mobile")
                .await["posterSize"],
            json!("large")
        );
        assert_eq!(
            h.resolved(&h.token, "global")
                .await["posterSize"],
            json!("large")
        );
    }

    #[tokio::test]
    async fn device_profile_alone_resolves_without_global() {
        let h = harness().await;
        h.push(&h.token, "tv", json!({ "visualTheme": "dark" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "visualTheme": "dark" })
        );
    }

    #[tokio::test]
    async fn keys_missing_from_device_profile_fall_through_to_global() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": "g", "b": "g" }))
            .await;
        h.push(&h.token, "tv", json!({ "b": "tv", "c": "tv" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "a": "g", "b": "tv", "c": "tv" })
        );
    }

    #[tokio::test]
    async fn empty_and_falsy_device_values_win_over_global() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({ "s": "x", "f": true, "n": 5, "l": ["a"], "o": { "k": 1 } }),
        )
        .await;
        h.push(
            &h.token,
            "tv",
            json!({ "s": "", "f": false, "n": 0, "l": [], "o": {} }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "s": "", "f": false, "n": 0, "l": [], "o": {} })
        );
    }

    #[tokio::test]
    async fn arrays_and_objects_resolve_whole_never_merged_across_layers() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({
                "mdblistRatingSources": ["b", "c"],
                "seerrRows": { "trendingMovies": true, "rowOrder": ["y"] },
            }),
        )
        .await;
        h.push(
            &h.token,
            "tv",
            json!({
                "mdblistRatingSources": ["a"],
                "seerrRows": { "rowOrder": ["x"] },
            }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({
                "mdblistRatingSources": ["a"],
                "seerrRows": { "rowOrder": ["x"] },
            })
        );
    }

    #[tokio::test]
    async fn home_row_order_is_derived_from_enabled_sorted_sections() {
        let h = harness().await;
        let sections = json!([
            { "type": "resume", "order": 1, "enabled": true },
            { "type": "nextUp", "order": 0, "enabled": true },
            { "type": "latestMedia", "order": 2, "enabled": false },
            { "type": "x", "kind": "pluginDynamic", "order": 3 },
        ]);
        h.push(&h.token, "global", json!({ "homeSections": sections }))
            .await;
        let resolved = h
            .resolved(&h.token, "global")
            .await;
        assert_eq!(resolved["homeSections"], sections);
        assert_eq!(resolved["homeRowOrder"], json!(["nextUp", "resume"]));
    }

    #[tokio::test]
    async fn home_layout_comes_from_a_single_layer() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({
                "homeSections": [{ "type": "resume", "order": 0, "enabled": true }],
                "homeRowOrder": ["b"],
            }),
        )
        .await;
        h.push(&h.token, "tv", json!({ "homeRowOrder": ["a"] }))
            .await;
        // The device layer wins as a pair, so global's sections are not mixed in.
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "homeRowOrder": ["a"] })
        );

        // A device layer without either key defers to global's pair entirely.
        h.push(&h.token, "mobile", json!({ "posterSize": "s" }))
            .await;
        let mobile = h
            .resolved(&h.token, "mobile")
            .await;
        assert_eq!(mobile["homeRowOrder"], json!(["b"]));
        assert_eq!(
            mobile["homeSections"],
            json!([{ "type": "resume", "order": 0, "enabled": true }])
        );
    }

    #[tokio::test]
    async fn rt_audience_rating_source_is_renamed_for_the_client() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({ "mdblistRatingSources": ["rtAudience", "imdb"] }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await["mdblistRatingSources"],
            json!(["tomatoes_audience", "imdb"])
        );
    }

    // ------------------------------------------------------------------
    // Deletes (E4-E7)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn deletes_require_authentication() {
        let h = harness().await;
        h.server
            .delete("/Moonfin/Settings")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        h.server
            .delete("/Moonfin/Settings/Profile/desktop")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn deleting_a_device_profile_falls_back_to_global() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "posterSize": "large" }))
            .await;
        h.push(&h.token, "desktop", json!({ "posterSize": "small" }))
            .await;
        let resp = h
            .delete(&h.token, "/Moonfin/Settings/Profile/Desktop")
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<Value>()["Success"], json!(true));
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({ "posterSize": "large" })
        );
    }

    #[tokio::test]
    async fn deleting_global_or_unknown_profile_is_a_400_and_changes_nothing() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        for profile in ["global", "Global", "nope"] {
            h.delete(&h.token, &format!("/Moonfin/Settings/Profile/{profile}"))
                .await
                .assert_status(StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1 })
        );
    }

    #[tokio::test]
    async fn deleting_a_missing_profile_succeeds() {
        let h = harness().await;
        h.delete(&h.token, "/Moonfin/Settings/Profile/tv")
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn delete_all_clears_only_that_users_settings_and_is_idempotent() {
        let h = harness().await;
        let other = h
            .second_user()
            .await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        h.push(&h.token, "tv", json!({ "a": 2 }))
            .await;
        h.push(&other, "global", json!({ "a": 9 }))
            .await;

        let resp = h
            .delete(&h.token, "/Moonfin/Settings")
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<Value>()["Success"], json!(true));
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({})
        );
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({})
        );
        assert_eq!(
            h.resolved(&other, "global")
                .await,
            json!({ "a": 9 })
        );

        // Nothing left to delete is still a success.
        h.delete(&h.token, "/Moonfin/Settings")
            .await
            .assert_status_ok();
        // And the user can save again afterwards, as a first save.
        let again = h
            .push(&h.token, "global", json!({ "a": 3 }))
            .await;
        assert_eq!(again.json::<Value>()["Created"], json!(true));
    }

    // ------------------------------------------------------------------
    // Admin defaults file (R5, R6, R13, R14, R16, R17)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn admin_defaults_are_served_to_users_without_settings() {
        let h =
            harness_with_defaults(r#"{"posterSize":"small","navbarPosition":"top"}"#)
                .await;
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({ "posterSize": "small", "navbarPosition": "top" })
        );
    }

    #[tokio::test]
    async fn admin_defaults_are_the_lowest_precedence_layer() {
        let h = harness_with_defaults(
            r#"{"posterSize":"small","navbarPosition":"top","x":1}"#,
        )
        .await;
        h.push(&h.token, "global", json!({ "posterSize": "large" }))
            .await;
        h.push(&h.token, "tv", json!({ "navbarPosition": "left" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "posterSize": "large", "navbarPosition": "left", "x": 1 })
        );
        assert_eq!(
            h.resolved(&h.token, "mobile")
                .await,
            json!({ "posterSize": "large", "navbarPosition": "top", "x": 1 })
        );
    }

    #[tokio::test]
    async fn admin_defaults_null_values_are_absent() {
        let h = harness_with_defaults(r#"{"x":null,"posterSize":"small"}"#).await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "posterSize": "small" })
        );
    }

    #[tokio::test]
    async fn admin_defaults_home_layout_is_a_pair_and_never_mixes_with_user_layers() {
        let defaults = r#"{"homeSections":[{"type":"none","order":0,"enabled":true}]}"#;
        let h = harness_with_defaults(defaults).await;
        // Only the defaults layer has a layout: sections echoed, derived order empty.
        let resolved = h
            .resolved(&h.token, "desktop")
            .await;
        assert_eq!(
            resolved,
            json!({ "homeSections": [{ "type": "none", "order": 0, "enabled": true }] })
        );

        // A user layer with a row order wins as a pair, so the default
        // sections do not leak in next to it.
        h.push(&h.token, "global", json!({ "homeRowOrder": ["b"] }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({ "homeRowOrder": ["b"] })
        );
    }

    #[tokio::test]
    async fn missing_admin_defaults_file_behaves_as_no_defaults() {
        let h = harness_with(|config, dir| {
            config.moonfin_default_settings_path = Some(
                dir.join("does-not-exist.json")
                    .to_string_lossy()
                    .into_owned(),
            );
        })
        .await;
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({})
        );
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({ "a": 1 })
        );
    }

    #[tokio::test]
    async fn invalid_admin_defaults_file_behaves_as_no_defaults() {
        for contents in ["not json at all", "[1,2,3]", "\"str\""] {
            let h = harness_with_defaults(contents).await;
            assert_eq!(
                h.resolved(&h.token, "desktop")
                    .await,
                json!({}),
                "{contents}"
            );
        }
    }

    // ------------------------------------------------------------------
    // Admin defaults in the database
    // ------------------------------------------------------------------

    use crate::api::moonfin_test_support::Harness as Shared;

    fn profile(v: Value) -> super::Profile {
        v.as_object()
            .unwrap()
            .clone()
    }

    async fn admin_id(h: &Shared) -> uuid::Uuid {
        sqlx::query_scalar("SELECT id FROM users LIMIT 1")
            .fetch_one(
                &h.ctx
                    .db,
            )
            .await
            .unwrap()
    }

    async fn resolved_for(h: &Shared, profile: &str) -> Value {
        let resp = h
            .get(
                &h.token,
                &format!("/Moonfin/Settings/Resolved/{profile}?client=moonfin"),
            )
            .await;
        resp.assert_status_ok();
        resp.json::<Value>()
    }

    #[tokio::test]
    async fn defaults_file_is_imported_once_and_the_database_then_wins() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join("defaults.json");
        std::fs::write(
            &file,
            r#"{"posterSize":"small","subtitleMode":"always","defaultAudioLanguage":"eng"}"#,
        )
        .unwrap();
        let path = file
            .to_string_lossy()
            .into_owned();
        let h =
            Shared::new_with(|c| c.moonfin_default_settings_path = Some(path)).await;

        // Imported at startup, minus the keys Remux owns.
        assert_eq!(
            resolved_for(&h, "desktop").await,
            json!({ "posterSize": "small" })
        );
        let stored = super::load_defaults(
            &h.ctx
                .db,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stored.settings, profile(json!({ "posterSize": "small" })));

        // Once the database holds defaults the file is never read again.
        super::save_defaults(&h.ctx, profile(json!({ "x": 1 })), admin_id(&h).await)
            .await
            .unwrap();
        std::fs::write(&file, r#"{"posterSize":"large"}"#).unwrap();
        super::import_defaults_file(&h.ctx).await;
        assert_eq!(resolved_for(&h, "desktop").await, json!({ "x": 1 }));

        // An admin who saved an empty object keeps it empty too.
        super::save_defaults(&h.ctx, profile(json!({})), admin_id(&h).await)
            .await
            .unwrap();
        super::import_defaults_file(&h.ctx).await;
        assert_eq!(resolved_for(&h, "desktop").await, json!({}));
    }

    #[tokio::test]
    async fn invalid_defaults_file_is_not_imported() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join("defaults.json");
        std::fs::write(&file, "[1,2,3]").unwrap();
        let path = file
            .to_string_lossy()
            .into_owned();
        let h =
            Shared::new_with(|c| c.moonfin_default_settings_path = Some(path)).await;
        assert!(
            super::load_defaults(
                &h.ctx
                    .db
            )
            .await
            .unwrap()
            .is_none()
        );
        assert_eq!(resolved_for(&h, "desktop").await, json!({}));
    }

    #[tokio::test]
    async fn blocked_keys_in_legacy_defaults_are_ignored_but_user_sync_is_not() {
        let h = Shared::new().await;
        // Data written before the blocklist existed.
        sqlx::query(
            "INSERT INTO moonfin_default_settings (id, data, updated_at) \
             VALUES (1, ?1, 0)",
        )
        .bind(
            json!({
                "posterSize": "small",
                "defaultAudioLanguage": "eng",
                "preferDefaultAudioTrack": true,
                "autoplayNextEpisode": false,
                "defaultSubtitleLanguage": "eng",
                "subtitleMode": "always",
            })
            .to_string(),
        )
        .execute(
            &h.ctx
                .db,
        )
        .await
        .unwrap();
        assert_eq!(
            resolved_for(&h, "global").await,
            json!({ "posterSize": "small" })
        );

        // The user's own copies of those keys still sync as before.
        h.post(
            &h.token,
            "/Moonfin/Settings/Profile/global",
            &json!({ "profile": { "subtitleMode": "none" }, "clientId": "t" }),
        )
        .await;
        assert_eq!(
            resolved_for(&h, "global").await,
            json!({ "posterSize": "small", "subtitleMode": "none" })
        );
    }

    #[tokio::test]
    async fn changing_defaults_notifies_every_stream_but_only_when_changed() {
        let h = Shared::new().await;
        let hub = &h
            .ctx
            .moonfin_settings;
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let mut stream_a = hub.subscribe(a);
        let mut stream_b = hub.subscribe(b);
        let admin = admin_id(&h).await;

        super::save_defaults(&h.ctx, profile(json!({ "x": 1 })), admin)
            .await
            .unwrap();
        for stream in [&mut stream_a, &mut stream_b] {
            let event = stream
                .rx
                .as_mut()
                .unwrap()
                .try_recv()
                .unwrap();
            assert_eq!(event, super::StreamEvent::SettingsUpdated);
        }

        super::save_defaults(&h.ctx, profile(json!({ "x": 1 })), admin)
            .await
            .unwrap();
        assert!(
            stream_a
                .rx
                .as_mut()
                .unwrap()
                .try_recv()
                .is_err()
        );
    }

    // ------------------------------------------------------------------
    // Keys added by newer clients round-trip untouched
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn newer_client_keys_round_trip_through_save_and_resolve() {
        let h = harness().await;
        let settings = json!({
            "bottomNavbarStyle": "split",
            "bottomNavbarTabs": ["search", "libraries", "favorites"],
            "showFriendsButton": true,
            "seasonalSurprise": "snow",
            "seasonalDensity": "heavy",
            "seasonalRowEnabled": true,
            "seasonalRowCountry": "NL",
            "seasonalRowHiddenHolidays": ["newYear", "easter"],
            "hiddenDetailSectionsTv": ["cast"],
            "hiddenDetailSectionsMobile": [],
            "hiddenDetailSectionsDesktop": ["similar", "extras"],
            "libraryStyle": "docked",
            "homeRowImageTypes": "genres:thumb,nextup:poster",
            "preferTextSubtitles": true,
            "preferExternalSubtitles": false,
        });
        h.push(&h.token, "global", settings.clone())
            .await
            .assert_status_ok();
        for profile in ["global", "desktop", "tv", "mobile"] {
            assert_eq!(
                h.resolved(&h.token, profile)
                    .await,
                settings,
                "{profile}"
            );
        }
        // A device profile replaces arrays whole, including with `[]`.
        h.push(&h.token, "tv", json!({ "bottomNavbarTabs": [] }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await["bottomNavbarTabs"],
            json!([])
        );
    }

    // ------------------------------------------------------------------
    // Seerr stub: plugin is "installed" but Seerr is not configured
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn seerr_config_reports_disabled() {
        let h = harness().await;
        h.server
            .get("/Moonfin/Seerr/Config")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        let resp = h
            .get(&h.token, "/Moonfin/Seerr/Config")
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        assert_eq!(body["enabled"], json!(false));
        // Absent or false: the client must not turn Seerr on for this user.
        assert!(
            body.get("userEnabled")
                .is_none_or(|v| v == &json!(false))
        );
    }

    #[tokio::test]
    async fn seerr_status_reports_disabled_so_login_bootstrap_returns_at_once() {
        // The client's post-login Seerr bootstrap calls this and returns
        // without a sign-in attempt when `enabled` is false.
        let h = harness().await;
        h.server
            .get("/Moonfin/Seerr/Status")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        let resp = h
            .get(&h.token, "/Moonfin/Seerr/Status")
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        assert_eq!(body["enabled"], json!(false));
        assert_eq!(body["authenticated"], json!(false));
    }

    // ------------------------------------------------------------------
    // SSE stream (SSE1-SSE3, SSE5, SSE6)
    // ------------------------------------------------------------------

    /// Minimal raw-HTTP SSE reader over a real socket (the mock transport
    /// buffers whole bodies, which an open stream never finishes).
    struct SseConn {
        stream: TcpStream,
        seen: String,
    }

    impl SseConn {
        async fn open(app: &Router, token: &str) -> SseConn {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap();
            let addr = listener
                .local_addr()
                .unwrap();
            let app = app.clone();
            tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .ok();
            });
            let mut stream = TcpStream::connect(addr)
                .await
                .unwrap();
            let request = format!(
                "GET /Moonfin/Settings/Stream HTTP/1.1\r\nHost: localhost\r\n\
                 Accept: text/event-stream\r\nAuthorization: {}\r\n\r\n",
                auth_header_with_token(token)
            );
            stream
                .write_all(request.as_bytes())
                .await
                .unwrap();
            SseConn {
                stream,
                seen: String::new(),
            }
        }

        async fn read_some(&mut self, wait: Duration) {
            let mut buf = [0u8; 4096];
            if let Ok(Ok(n)) = tokio::time::timeout(
                wait,
                self.stream
                    .read(&mut buf),
            )
            .await
            {
                self.seen
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }

        async fn wait_for(&mut self, needle: &str) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !self
                .seen
                .contains(needle)
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for {needle:?}; saw {:?}",
                    self.seen
                );
                self.read_some(Duration::from_millis(200))
                    .await;
            }
        }

        /// True if `needle` shows up within `wait`.
        async fn appears_within(&mut self, needle: &str, wait: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + wait;
            while tokio::time::Instant::now() < deadline {
                if self
                    .seen
                    .contains(needle)
                {
                    return true;
                }
                self.read_some(Duration::from_millis(50))
                    .await;
            }
            self.seen
                .contains(needle)
        }
    }

    const EVENT: &str = r#"data: {"type":"settingsUpdated"}"#;
    const QUIET: Duration = Duration::from_millis(600);

    #[tokio::test]
    async fn stream_requires_authentication() {
        let h = harness().await;
        h.server
            .get("/Moonfin/Settings/Stream")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn stream_is_event_stream_and_announces_itself_immediately() {
        let h = harness().await;
        let mut sse = SseConn::open(&h.app, &h.token).await;
        sse.wait_for(":connected")
            .await;
        let head = sse
            .seen
            .to_lowercase();
        assert!(head.starts_with("http/1.1 200"), "{head}");
        assert!(head.contains("content-type: text/event-stream"), "{head}");
    }

    #[tokio::test]
    async fn changed_save_notifies_the_same_users_stream_only() {
        let h = harness().await;
        let other = h
            .second_user()
            .await;
        let mut mine = SseConn::open(&h.app, &h.token).await;
        let mut theirs = SseConn::open(&h.app, &other).await;
        mine.wait_for(":connected")
            .await;
        theirs
            .wait_for(":connected")
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        h.push(&h.token, "tv", json!({ "a": 1 }))
            .await
            .assert_status_ok();

        mine.wait_for(EVENT)
            .await;
        assert!(
            !theirs
                .appears_within(EVENT, QUIET)
                .await,
            "other user's stream must stay quiet: {:?}",
            theirs.seen
        );
    }

    #[tokio::test]
    async fn saving_an_identical_payload_does_not_notify_again() {
        let h = harness().await;
        let mut sse = SseConn::open(&h.app, &h.token).await;
        sse.wait_for(":connected")
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        h.push(&h.token, "tv", json!({ "a": 1 }))
            .await;
        sse.wait_for(EVENT)
            .await;
        let before = sse
            .seen
            .matches(EVENT)
            .count();

        h.push(&h.token, "tv", json!({ "a": 1 }))
            .await
            .assert_status_ok();
        let _ = sse
            .appears_within("never-appears", QUIET)
            .await;
        assert_eq!(
            sse.seen
                .matches(EVENT)
                .count(),
            before,
            "{:?}",
            sse.seen
        );
    }

    #[tokio::test]
    async fn deletes_do_not_notify_the_stream() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        h.push(&h.token, "tv", json!({ "a": 2 }))
            .await;
        let mut sse = SseConn::open(&h.app, &h.token).await;
        sse.wait_for(":connected")
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        h.delete(&h.token, "/Moonfin/Settings/Profile/tv")
            .await
            .assert_status_ok();
        h.delete(&h.token, "/Moonfin/Settings")
            .await
            .assert_status_ok();
        assert!(
            !sse.appears_within(EVENT, QUIET)
                .await,
            "{:?}",
            sse.seen
        );
    }
}
