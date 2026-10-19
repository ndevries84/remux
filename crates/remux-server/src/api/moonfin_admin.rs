//! Admin API for the Moonfin settings that Remux hosts (all routes need an
//! admin session; anonymous callers get `401`, other users `403`):
//!
//! * `GET|PUT /Moonfin/Admin/Defaults`: the admin default settings, the
//!   lowest layer when a user's settings are resolved. A schemaless JSON
//!   object, except that keys Remux owns ([`ADMIN_BLOCKED_KEYS`]) are refused.
//! * `POST /Moonfin/Admin/Defaults/Apply`: push the defaults into users'
//!   stored profiles (merge or overwrite, for everyone or one user).
//! * `GET /Moonfin/Admin/Users`: every user with a summary of the Moonfin
//!   settings stored for them and the Moonfin devices they signed in with.
//! * `GET|DELETE /Moonfin/Admin/Users/{userId}/settings`: the stored and
//!   resolved settings of one user, and a reset of all or one profile.
//! * `GET /Moonfin/Admin/Status`: what this server offers Moonfin clients,
//!   for an admin page. It never includes secrets, only whether they are set.

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
};
use remux_macros::{delete, get, post, put};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::moonfin_settings::{
    ADMIN_BLOCKED_KEYS, MAX_PROFILE_BYTES, MAX_PROFILE_KEYS, MAX_SAVE_BYTES, Profile,
    StreamEvent, VALID_PROFILES, api_key_names_in, blocked_keys_in, internal_error,
    is_api_key_name, load_defaults, load_profiles, merge_profile, now_ms,
    parse_profile, parse_stored, ping_body, resolved_profile, save_defaults,
    within_profile_caps,
};
use crate::{AppState, db::auth::AuthSession, games};

pub(crate) fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// `Some(403)` response unless the caller is an admin.
pub(crate) fn require_admin(session: &AuthSession) -> Option<Response> {
    (!session
        .user
        .is_admin)
        .then(|| json_error(StatusCode::FORBIDDEN, "Admin access required"))
}

/// A user id as clients write it, dashed or not.
fn parse_user(raw: &str) -> Result<Uuid, Response> {
    Uuid::parse_str(raw)
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "Invalid user id"))
}

/// Dashless lowercase form of a user id, as the Jellyfin API prints it.
fn simple(id: Uuid) -> String {
    id.simple()
        .to_string()
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

fn defaults_body(settings: Map<String, Value>, updated_at: Option<i64>) -> Response {
    Json(json!({
        "settings": settings,
        "blockedKeys": ADMIN_BLOCKED_KEYS,
        "limits": { "maxKeys": MAX_PROFILE_KEYS, "maxBytes": MAX_PROFILE_BYTES },
        "updatedAt": updated_at,
    }))
    .into_response()
}

#[get("/moonfin/admin/defaults")]
pub async fn moonfin_admin_defaults_get(
    State(state): State<AppState>,
    session: AuthSession,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    match load_defaults(
        &state
            .ctx
            .db,
    )
    .await
    {
        Ok(Some(stored)) => defaults_body(stored.settings, Some(stored.updated_at)),
        Ok(None) => defaults_body(Map::new(), None),
        Err(e) => internal_error(e),
    }
}

#[put("/moonfin/admin/defaults")]
pub async fn moonfin_admin_defaults_put(
    State(state): State<AppState>,
    session: AuthSession,
    req: Request<Body>,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_SAVE_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
        }
    };
    let settings = match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(mut body)) => match body.remove("settings") {
            Some(Value::Object(settings)) => settings,
            _ => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "Body must be {\"settings\": {...}}",
                );
            }
        },
        _ => return json_error(StatusCode::BAD_REQUEST, "Body must be a JSON object"),
    };
    let blocked = blocked_keys_in(&settings);
    if !blocked.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "These settings are managed by Remux and cannot be defaults: {}",
                blocked.join(", ")
            ),
        );
    }
    let api_keys = api_key_names_in(&settings);
    if !api_keys.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "API keys cannot be admin defaults because every user would inherit them; \
                 set the server key in Server settings instead: {}",
                api_keys.join(", ")
            ),
        );
    }
    if !within_profile_caps(&settings) {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Default settings exceed the key-count or size limit",
        );
    }
    match save_defaults(
        &state.ctx,
        settings,
        session
            .user
            .id,
    )
    .await
    {
        Ok(stored) => defaults_body(stored.settings, Some(stored.updated_at)),
        Err(e) => internal_error(e),
    }
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

#[get("/moonfin/admin/users")]
pub async fn moonfin_admin_users_list(
    State(state): State<AppState>,
    session: AuthSession,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    match users_summary(&state).await {
        Ok(items) => Json(json!({ "items": items })).into_response(),
        Err(e) => internal_error(e),
    }
}

async fn users_summary(state: &AppState) -> Result<Vec<Value>, sqlx::Error> {
    let db = &state
        .ctx
        .db;
    let users: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, username FROM users ORDER BY username COLLATE NOCASE",
    )
    .fetch_all(db)
    .await?;
    let stored: Vec<(Uuid, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT user_id, profile, updated_at, updated_by FROM moonfin_settings",
    )
    .fetch_all(db)
    .await?;
    // Auth stores `devices.user_id` as a 16-byte uuid blob (it binds a `Uuid`),
    // so decode it as one. `LIKE` is ASCII case-insensitive, which is all the
    // app-name match needs.
    let devices: Vec<(Uuid, String, String, String, String, String)> = sqlx::query_as(
        "SELECT user_id, id, name, app_name, app_version, last_activity_at \
         FROM devices WHERE app_name LIKE '%Moonfin%' ORDER BY last_activity_at DESC",
    )
    .fetch_all(db)
    .await?;

    Ok(users
        .into_iter()
        .map(|(id, name)| {
            let profiles: Vec<Value> = VALID_PROFILES
                .iter()
                .filter_map(|p| {
                    let (_, _, at, by) = stored
                        .iter()
                        .find(|(u, profile, ..)| *u == id && profile == p)?;
                    Some(json!({ "profile": p, "updatedAt": at, "updatedBy": by }))
                })
                .collect();
            let devices: Vec<Value> = devices
                .iter()
                .filter(|(u, ..)| *u == id)
                .map(|(_, device_id, name, app, version, last)| {
                    json!({
                        "id": device_id,
                        "name": name,
                        "appName": app,
                        "appVersion": version,
                        "lastActivityAt": last,
                    })
                })
                .collect();
            json!({
                "userId": simple(id),
                "name": name,
                "profiles": profiles,
                "devices": devices,
            })
        })
        .collect())
}

async fn user_name(state: &AppState, id: Uuid) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT username FROM users WHERE id = ?1")
        .bind(id)
        .fetch_optional(
            &state
                .ctx
                .db,
        )
        .await
}

#[get("/moonfin/admin/users/{user_id}/settings")]
pub async fn moonfin_admin_user_settings(
    State(state): State<AppState>,
    session: AuthSession,
    Path(user_id): Path<String>,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    let id = match parse_user(&user_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match user_settings_view(&state, id).await {
        Ok(Some(view)) => Json(view).into_response(),
        Ok(None) => json_error(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => internal_error(e),
    }
}

/// Replaces credential values with `"set"` (a blank one is dropped), so the
/// admin view shows whether a key exists without exposing it.
fn redact_api_keys(settings: &mut Profile) {
    settings.retain(|key, value| {
        if !is_api_key_name(key) {
            return true;
        }
        match value {
            Value::String(s)
                if s.trim()
                    .is_empty() =>
            {
                false
            }
            Value::Null => false,
            _ => {
                *value = Value::String("set".into());
                true
            }
        }
    });
}

async fn user_settings_view(
    state: &AppState,
    id: Uuid,
) -> Result<Option<Value>, sqlx::Error> {
    let Some(name) = user_name(state, id).await? else {
        return Ok(None);
    };
    let stored = load_profiles(
        &state
            .ctx
            .db,
        id,
    )
    .await?;
    let mut profiles = Map::new();
    let mut resolved = Map::new();
    for profile in VALID_PROFILES {
        if let Some((settings, updated_at, updated_by)) = stored.get(profile) {
            let mut settings = settings.clone();
            redact_api_keys(&mut settings);
            profiles.insert(
                profile.to_string(),
                json!({
                    "settings": settings,
                    "updatedAt": updated_at,
                    "updatedBy": updated_by,
                }),
            );
        }
        let mut view = resolved_profile(state, id, profile).await?;
        redact_api_keys(&mut view);
        resolved.insert(profile.to_string(), Value::Object(view));
    }
    Ok(Some(json!({
        "userId": simple(id),
        "name": name,
        "profiles": profiles,
        "resolved": resolved,
    })))
}

#[derive(Deserialize)]
struct ResetQuery {
    profile: Option<String>,
}

#[delete("/moonfin/admin/users/{user_id}/settings")]
pub async fn moonfin_admin_user_settings_reset(
    State(state): State<AppState>,
    session: AuthSession,
    Path(user_id): Path<String>,
    Query(query): Query<ResetQuery>,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    let id = match parse_user(&user_id) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let profile = match query
        .profile
        .as_deref()
        .map(|p| parse_profile(p).ok_or(p))
        .transpose()
    {
        Ok(profile) => profile,
        Err(bad) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "Invalid profile: {bad}. Valid profiles: global, desktop, mobile, tv"
                ),
            );
        }
    };
    match user_name(&state, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "User not found"),
        Err(e) => return internal_error(e),
    }
    let hub = &state
        .ctx
        .moonfin_settings;
    let _write = hub
        .write_lock()
        .await;
    let db = &state
        .ctx
        .db;
    let result = match &profile {
        Some(p) => {
            sqlx::query(
                "DELETE FROM moonfin_settings WHERE user_id = ?1 AND profile = ?2",
            )
            .bind(id)
            .bind(p)
            .execute(db)
            .await
        }
        None => {
            sqlx::query("DELETE FROM moonfin_settings WHERE user_id = ?1")
                .bind(id)
                .execute(db)
                .await
        }
    };
    match result {
        Ok(done) => {
            let deleted = done.rows_affected();
            if deleted > 0 {
                hub.notify(id, StreamEvent::SettingsUpdated);
            }
            Json(json!({ "success": true, "deleted": deleted })).into_response()
        }
        Err(e) => internal_error(e),
    }
}

// ---------------------------------------------------------------------------
// Apply defaults to existing users
// ---------------------------------------------------------------------------

/// Moonfin apps upload all their settings on first sign-in, so new defaults
/// only reach users who have never synced. This pushes them into stored
/// profiles on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ApplyScope {
    All,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ApplyMode {
    /// Fill in default keys the user has no value for.
    Merge,
    /// Set every default key, replacing the user's values.
    Overwrite,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApplyRequest {
    scope: ApplyScope,
    user_id: Option<String>,
    mode: ApplyMode,
    profile: Option<String>,
}

/// The home layout (`homeSections` + `homeRowOrder`) is one setting that
/// resolves as a pair from a single layer, so merging treats it as a unit.
const HOME_LAYOUT_KEYS: [&str; 2] = ["homeSections", "homeRowOrder"];

fn has_value(profile: &Profile, key: &str) -> bool {
    profile
        .get(key)
        .is_some_and(|v| !v.is_null())
}

/// `stored` after applying `defaults`. Blocked keys, API keys (a blank one
/// must never clobber a user's) and nulls in `defaults`
/// are never written; keys outside `defaults` are never touched.
///
/// * Merge: a default key is written only where the user has no value. The
///   home layout pair is written only when the user has neither half.
/// * Overwrite: every default key replaces the user's value (with the same
///   rules as a normal save, e.g. a blank API key is ignored).
fn apply_defaults(stored: &Profile, defaults: &Profile, mode: ApplyMode) -> Profile {
    let mut incoming = Profile::new();
    let user_has_layout = HOME_LAYOUT_KEYS
        .iter()
        .any(|k| has_value(stored, k));
    for (key, value) in defaults {
        if value.is_null()
            || ADMIN_BLOCKED_KEYS.contains(&key.as_str())
            || is_api_key_name(key)
        {
            continue;
        }
        if mode == ApplyMode::Merge {
            let is_layout = HOME_LAYOUT_KEYS.contains(&key.as_str());
            if has_value(stored, key) || (is_layout && user_has_layout) {
                continue;
            }
        }
        incoming.insert(key.clone(), value.clone());
    }
    let mut after = stored.clone();
    if mode == ApplyMode::Overwrite {
        merge_profile(&mut after, &incoming);
    } else {
        for (key, value) in incoming {
            after.insert(key, value);
        }
    }
    after
}

/// Number of top-level keys that differ between two profiles.
fn changed_keys(before: &Profile, after: &Profile) -> usize {
    after
        .iter()
        .filter(|(k, v)| before.get(*k) != Some(*v))
        .count()
        + before
            .keys()
            .filter(|k| !after.contains_key(*k))
            .count()
}

/// Pushes the admin defaults into users' stored profile, for everyone or one
/// user. Merge only touches users that already have the profile stored (a
/// user with none resolves to the defaults anyway, and copying them would
/// freeze them against later default changes); overwrite also creates it.
/// Users whose result would exceed the profile size caps are skipped and
/// counted in `usersSkipped`.
#[post("/moonfin/admin/defaults/apply")]
pub async fn moonfin_admin_defaults_apply(
    State(state): State<AppState>,
    session: AuthSession,
    req: Request<Body>,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_SAVE_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
        }
    };
    let request: ApplyRequest = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "Body must be {{\"scope\": \"all\"|\"user\", \"userId\"?, \"mode\": \"merge\"|\"overwrite\", \"profile\"?}}: {e}"
                ),
            );
        }
    };
    let profile = match request
        .profile
        .as_deref()
        .map(|p| parse_profile(p).ok_or(p))
        .transpose()
    {
        Ok(p) => p.unwrap_or_else(|| "global".to_string()),
        Err(bad) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "Invalid profile: {bad}. Valid profiles: global, desktop, mobile, tv"
                ),
            );
        }
    };
    let targets: Vec<Uuid> = match request.scope {
        ApplyScope::User => {
            let Some(raw) = request
                .user_id
                .as_deref()
            else {
                return json_error(StatusCode::BAD_REQUEST, "userId is required");
            };
            let id = match parse_user(raw) {
                Ok(id) => id,
                Err(resp) => return resp,
            };
            match user_name(&state, id).await {
                Ok(Some(_)) => vec![id],
                Ok(None) => return json_error(StatusCode::NOT_FOUND, "User not found"),
                Err(e) => return internal_error(e),
            }
        }
        ApplyScope::All => {
            match sqlx::query_scalar::<_, Uuid>("SELECT id FROM users")
                .fetch_all(
                    &state
                        .ctx
                        .db,
                )
                .await
            {
                Ok(ids) => ids,
                Err(e) => return internal_error(e),
            }
        }
    };

    let hub = &state
        .ctx
        .moonfin_settings;
    let _write = hub
        .write_lock()
        .await;
    let db = &state
        .ctx
        .db;
    let defaults = match load_defaults(db).await {
        Ok(d) => d
            .map(|d| d.settings)
            .unwrap_or_default(),
        Err(e) => return internal_error(e),
    };
    let client_id = match request.mode {
        ApplyMode::Merge => "admin-default-merge",
        ApplyMode::Overwrite => "admin-default-overwrite",
    };

    let result: Result<(Vec<Uuid>, usize, usize), sqlx::Error> = async {
        let mut tx = db
            .begin()
            .await?;
        let mut affected = Vec::new();
        let mut keys_written = 0;
        let mut skipped = 0;
        for user in targets {
            let row: Option<String> = sqlx::query_scalar(
                "SELECT data FROM moonfin_settings WHERE user_id = ?1 AND profile = ?2",
            )
            .bind(user)
            .bind(&profile)
            .fetch_optional(&mut *tx)
            .await?;
            if row.is_none() && request.mode == ApplyMode::Merge {
                continue;
            }
            let before = row
                .as_deref()
                .map(parse_stored)
                .unwrap_or_default();
            let after = apply_defaults(&before, &defaults, request.mode);
            let changed = changed_keys(&before, &after);
            if changed == 0 {
                continue;
            }
            let too_many_keys = after.len() > MAX_PROFILE_KEYS;
            let serialized = Value::Object(after).to_string();
            if too_many_keys || serialized.len() > MAX_PROFILE_BYTES {
                skipped += 1;
                continue;
            }
            sqlx::query(
                "INSERT INTO moonfin_settings (user_id, profile, data, updated_at, updated_by) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(user_id, profile) DO UPDATE SET \
                 data = excluded.data, updated_at = excluded.updated_at, \
                 updated_by = excluded.updated_by",
            )
            .bind(user)
            .bind(&profile)
            .bind(&serialized)
            .bind(now_ms())
            .bind(client_id)
            .execute(&mut *tx)
            .await?;
            keys_written += changed;
            affected.push(user);
        }
        tx.commit()
            .await?;
        Ok((affected, keys_written, skipped))
    }
    .await;
    match result {
        Ok((affected, keys_written, skipped)) => {
            for user in &affected {
                hub.notify(*user, StreamEvent::SettingsUpdated);
            }
            Json(json!({
                "usersAffected": affected.len(),
                "keysWritten": keys_written,
                "usersSkipped": skipped,
            }))
            .into_response()
        }
        Err(e) => internal_error(e),
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[get("/moonfin/admin/status")]
pub async fn moonfin_admin_status(
    State(state): State<AppState>,
    session: AuthSession,
    headers: HeaderMap,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    match status(&state, &headers).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => internal_error(e),
    }
}

async fn status(state: &AppState, headers: &HeaderMap) -> Result<Value, sqlx::Error> {
    let ctx = &state.ctx;
    let config = &ctx.config;
    let eff = super::moonfin_server_settings::effective(state).await;

    let roots = config
        .games_roots
        .iter()
        .filter(|r| {
            !r.trim()
                .is_empty()
        })
        .count();
    let active = games::is_active(config);
    let game_count = if active {
        ctx.games
            .snapshot(config)
            .await
            .games
            .len()
    } else {
        0
    };

    let themes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moonfin_themes")
        .fetch_one(&ctx.db)
        .await?;
    let defaults_keys = load_defaults(&ctx.db)
        .await?
        .map_or(0, |d| {
            d.settings
                .len()
        });
    let web = web_app_status(state, &eff, headers).await;
    let web_app = web["present"] == true;

    Ok(json!({
        "ping": ping_body(eff.mdblist_key.is_some()),
        "games": {
            "enabled": config.games_enabled,
            "active": active,
            "rootsCount": roots,
            "gameCount": game_count,
        },
        "cores": ctx.cores.status(config).await,
        "emulatorjsCdnFallback": config.games_emulatorjs_cdn_fallback,
        "metadata": {
            "libretro": config.games_metadata_enabled,
            "launchbox": {
                "enabled": config.games_launchbox_enabled,
                "index": ctx.game_meta.launchbox_status(),
            },
        },
        // Whether a key is set and where it comes from, never the key.
        "mdblistConfigured": eff.mdblist_key.is_some(),
        "mdblistKeySource": eff.mdblist_key.as_ref().map(|(_, s)| s),
        "webAppPresent": web_app,
        "webApp": web,
        "themesCount": themes,
        "defaultsKeyCount": defaults_keys,
    }))
}

/// The hosted web app: whether its bundle is in place, which version it is
/// (Flutter's `version.json`), the effective `config.json` URLs and the URL
/// apps should use, derived from the request origin.
async fn web_app_status(
    state: &AppState,
    eff: &super::moonfin_server_settings::Effective,
    headers: &HeaderMap,
) -> Value {
    let root = super::moonfin_web::web_root(state);
    let present = tokio::fs::metadata(root.join("index.html"))
        .await
        .is_ok_and(|m| m.is_file());
    let version: Option<Value> = match tokio::fs::read(root.join("version.json")).await
    {
        Ok(bytes) => serde_json::from_slice(&bytes).ok(),
        Err(_) => None,
    };
    let field = |name: &str| {
        version
            .as_ref()
            .and_then(|v| v.get(name))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let origin = super::moonfin_web::request_origin(headers);
    json!({
        "present": present,
        "bundlePath": root.to_string_lossy(),
        "version": field("version"),
        "buildNumber": field("build_number"),
        "defaultServerUrl": eff.web_default_server_url.as_ref().map(|(u, _)| u),
        "forcedServerUrl": eff.web_forced_server_url.as_ref().map(|(u, _)| u),
        "enableWebRtcScan": eff.web_enable_webrtc_scan.0,
        "appUrl": origin.map(|o| format!("{o}/Moonfin/Web")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::moonfin_settings::StreamEvent;
    use crate::api::moonfin_test_support::Harness;
    use serde_json::json;

    const ADMIN_ROUTES: [(&str, &str); 6] = [
        ("GET", "/Moonfin/Admin/Defaults"),
        ("PUT", "/Moonfin/Admin/Defaults"),
        ("GET", "/Moonfin/Admin/Users"),
        ("GET", "/Moonfin/Admin/Users/{id}/Settings"),
        ("DELETE", "/Moonfin/Admin/Users/{id}/Settings"),
        ("GET", "/Moonfin/Admin/Status"),
    ];

    async fn user_id(h: &Harness, name: &str) -> Uuid {
        sqlx::query_scalar("SELECT id FROM users WHERE username = ?1")
            .bind(name)
            .fetch_one(
                &h.ctx
                    .db,
            )
            .await
            .unwrap()
    }

    async fn put_defaults(
        h: &Harness,
        token: &str,
        body: Value,
    ) -> axum_test::TestResponse {
        h.server
            .put("/Moonfin/Admin/Defaults")
            .add_header(http::header::AUTHORIZATION, auth(token))
            .json(&body)
            .await
    }

    fn auth(token: &str) -> http::HeaderValue {
        http::HeaderValue::from_str(&crate::integration_test::auth_header_with_token(
            token,
        ))
        .unwrap()
    }

    async fn push(h: &Harness, token: &str, profile: &str, settings: Value) {
        h.post(
            token,
            &format!("/Moonfin/Settings/Profile/{profile}"),
            &json!({ "profile": settings, "clientId": "moonfin-flutter" }),
        )
        .await
        .assert_status_ok();
    }

    #[tokio::test]
    async fn every_admin_route_needs_an_admin() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let id = simple(user_id(&h, "second").await);
        for (method, path) in ADMIN_ROUTES {
            let path = path.replace("{id}", &id);
            let call = |token: Option<&str>| {
                let mut req = match method {
                    "GET" => h
                        .server
                        .get(&path),
                    "PUT" => h
                        .server
                        .put(&path)
                        .json(&json!({ "settings": {} })),
                    _ => h
                        .server
                        .delete(&path),
                };
                if let Some(token) = token {
                    req = req.add_header(http::header::AUTHORIZATION, auth(token));
                }
                req
            };
            call(None)
                .await
                .assert_status(StatusCode::UNAUTHORIZED);
            call(Some(&other))
                .await
                .assert_status(StatusCode::FORBIDDEN);
            assert_ne!(
                call(Some(&h.token))
                    .await
                    .status_code(),
                StatusCode::FORBIDDEN,
                "{method} {path}"
            );
        }
    }

    #[tokio::test]
    async fn defaults_start_empty_and_can_be_replaced() {
        let h = Harness::new().await;
        let initial = h
            .get(&h.token, "/Moonfin/Admin/Defaults")
            .await;
        initial.assert_status_ok();
        assert_eq!(
            initial.json::<Value>(),
            json!({
                "settings": {},
                "blockedKeys": [
                    "defaultAudioLanguage",
                    "preferDefaultAudioTrack",
                    "autoplayNextEpisode",
                    "defaultSubtitleLanguage",
                    "subtitleMode",
                ],
                "limits": { "maxKeys": MAX_PROFILE_KEYS, "maxBytes": MAX_PROFILE_BYTES },
                "updatedAt": null,
            })
        );

        // Home rows, their toggles and keys for features Remux lacks are all
        // ordinary pass-through defaults.
        let settings = json!({
            "posterSize": "small",
            "homeSections": [{ "type": "latestmedia", "order": 0, "enabled": true }],
            "homeRowOrder": ["latestmedia"],
            "displayFavoritesRows": true,
            "homeRowImageTypes": "genres:thumb",
            "showSeerrButton": true,
            "bottomNavbarTabs": ["search"],
        });
        let saved = put_defaults(&h, &h.token, json!({ "settings": settings })).await;
        saved.assert_status_ok();
        let saved = saved.json::<Value>();
        assert_eq!(saved["settings"], settings);
        assert!(
            saved["updatedAt"]
                .as_i64()
                .unwrap()
                > 0
        );

        let read = h
            .get(&h.token, "/Moonfin/Admin/Defaults")
            .await
            .json::<Value>();
        assert_eq!(read["settings"], settings);
        assert_eq!(read["updatedAt"], saved["updatedAt"]);

        // A user without settings resolves to the defaults, home layout included.
        let resolved = h
            .get(
                &h.token,
                "/Moonfin/Settings/Resolved/desktop?client=moonfin",
            )
            .await
            .json::<Value>();
        assert_eq!(resolved, settings);

        // PUT replaces the whole object; nothing from before survives.
        put_defaults(
            &h,
            &h.token,
            json!({ "settings": { "navbarPosition": "top" } }),
        )
        .await
        .assert_status_ok();
        let read = h
            .get(&h.token, "/Moonfin/Admin/Defaults")
            .await
            .json::<Value>();
        assert_eq!(read["settings"], json!({ "navbarPosition": "top" }));

        // Clearing is a valid replacement too.
        put_defaults(&h, &h.token, json!({ "settings": {} }))
            .await
            .assert_status_ok();
        let resolved = h
            .get(
                &h.token,
                "/Moonfin/Settings/Resolved/desktop?client=moonfin",
            )
            .await
            .json::<Value>();
        assert_eq!(resolved, json!({}));
    }

    #[tokio::test]
    async fn blocked_keys_are_rejected_by_name_and_nothing_is_stored() {
        let h = Harness::new().await;
        put_defaults(
            &h,
            &h.token,
            json!({ "settings": { "posterSize": "small" } }),
        )
        .await
        .assert_status_ok();
        let resp = put_defaults(
            &h,
            &h.token,
            json!({ "settings": {
                "subtitleMode": "always",
                "defaultAudioLanguage": "eng",
                "posterSize": "large",
            } }),
        )
        .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        let message = resp.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(message.contains("subtitleMode"), "{message}");
        assert!(message.contains("defaultAudioLanguage"), "{message}");
        assert!(!message.contains("posterSize"), "{message}");
        assert!(!message.contains("autoplayNextEpisode"), "{message}");

        let read = h
            .get(&h.token, "/Moonfin/Admin/Defaults")
            .await
            .json::<Value>();
        assert_eq!(read["settings"], json!({ "posterSize": "small" }));

        // Every blocked key is refused on its own.
        for key in ADMIN_BLOCKED_KEYS {
            put_defaults(&h, &h.token, json!({ "settings": { key: null } }))
                .await
                .assert_status(StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn defaults_reject_malformed_and_oversized_bodies() {
        let h = Harness::new().await;
        for body in [
            json!([1]),
            json!({}),
            json!({ "settings": null }),
            json!({ "settings": [1] }),
            json!({ "posterSize": "small" }),
        ] {
            put_defaults(&h, &h.token, body.clone())
                .await
                .assert_status(StatusCode::BAD_REQUEST);
        }
        let huge = "x".repeat(MAX_SAVE_BYTES);
        put_defaults(&h, &h.token, json!({ "settings": { "big": huge } }))
            .await
            .assert_status(StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn changing_defaults_reaches_every_connected_user_but_only_on_change() {
        let h = Harness::new().await;
        h.second_user()
            .await;
        let mut admin = h
            .ctx
            .moonfin_settings
            .subscribe(user_id(&h, "test").await);
        let mut other = h
            .ctx
            .moonfin_settings
            .subscribe(user_id(&h, "second").await);

        put_defaults(
            &h,
            &h.token,
            json!({ "settings": { "posterSize": "small" } }),
        )
        .await
        .assert_status_ok();
        assert_eq!(admin.try_event(), Some(StreamEvent::SettingsUpdated));
        assert_eq!(other.try_event(), Some(StreamEvent::SettingsUpdated));

        put_defaults(
            &h,
            &h.token,
            json!({ "settings": { "posterSize": "small" } }),
        )
        .await
        .assert_status_ok();
        assert_eq!(other.try_event(), None);

        // A rejected save is not a change.
        put_defaults(
            &h,
            &h.token,
            json!({ "settings": { "subtitleMode": "none" } }),
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(other.try_event(), None);
    }

    #[tokio::test]
    async fn users_list_summarises_stored_profiles_and_moonfin_devices() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        push(&h, &other, "global", json!({ "a": 1 })).await;
        push(&h, &other, "tv", json!({ "b": 2 })).await;
        let second = user_id(&h, "second").await;
        for (id, app) in [
            ("d1", "Moonfin Flutter"),
            ("d2", "Jellyfin Web"),
            ("d3", "moonfin"),
        ] {
            let owner = second;
            sqlx::query(
                "INSERT INTO devices (user_id, id, access_token, name, app_name, app_version) \
                 VALUES (?1, ?2, ?3, 'Living room', ?4, '2.7.0')",
            )
            .bind(owner)
            .bind(id)
            .bind(format!("token-{id}"))
            .bind(app)
            .execute(&h.ctx.db)
            .await
            .unwrap();
        }

        let resp = h
            .get(&h.token, "/Moonfin/Admin/Users")
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        let items = body["items"]
            .as_array()
            .unwrap();
        assert_eq!(items.len(), 2);

        let admin = items
            .iter()
            .find(|u| u["name"] == "test")
            .unwrap();
        assert_eq!(admin["profiles"], json!([]));

        let second_entry = items
            .iter()
            .find(|u| u["name"] == "second")
            .unwrap();
        assert_eq!(second_entry["userId"], simple(second));
        let profiles = second_entry["profiles"]
            .as_array()
            .unwrap();
        let names: Vec<_> = profiles
            .iter()
            .map(|p| {
                p["profile"]
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(names, ["global", "tv"]);
        assert!(
            profiles[0]["updatedAt"]
                .as_i64()
                .unwrap()
                > 0
        );
        assert_eq!(profiles[0]["updatedBy"], "moonfin-flutter");

        let mut device_ids: Vec<_> = second_entry["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                d["id"]
                    .as_str()
                    .unwrap()
            })
            .collect();
        device_ids.sort();
        assert_eq!(device_ids, ["d1", "d3"]);
        assert_eq!(second_entry["devices"][0]["appVersion"], "2.7.0");
    }

    /// Regression: devices created by the real auth flow store `user_id` as a
    /// uuid blob; the users list used to decode it as text and return 500.
    #[tokio::test]
    async fn users_list_includes_devices_created_by_real_login() {
        let h = Harness::new().await;
        let resp = h
            .server
            .post("/Users/AuthenticateByName")
            .add_header(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_static(
                    "MediaBrowser Client=\"Moonfin Admin\", Device=\"Browser\", DeviceId=\"moonfin-admin-dev\", Version=\"1.0.0\"",
                ),
            )
            .json(&json!({ "Username": "test", "Pw": "test" }))
            .await;
        resp.assert_status_ok();

        let resp = h
            .get(&h.token, "/Moonfin/Admin/Users")
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        let admin = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["name"] == "test")
            .unwrap();
        let devices = admin["devices"]
            .as_array()
            .unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0]["id"], "moonfin-admin-dev");
        assert_eq!(devices[0]["appName"], "Moonfin Admin");
    }

    #[tokio::test]
    async fn user_detail_shows_stored_and_resolved_profiles() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let second = user_id(&h, "second").await;
        put_defaults(
            &h,
            &h.token,
            json!({ "settings": { "posterSize": "small", "navbarPosition": "top" } }),
        )
        .await
        .assert_status_ok();
        push(&h, &other, "global", json!({ "posterSize": "large" })).await;
        push(&h, &other, "tv", json!({ "navbarPosition": "left" })).await;

        for id in [simple(second), second.to_string()] {
            let resp = h
                .get(&h.token, &format!("/Moonfin/Admin/Users/{id}/Settings"))
                .await;
            resp.assert_status_ok();
            let body = resp.json::<Value>();
            assert_eq!(body["userId"], simple(second));
            assert_eq!(body["name"], "second");
            assert_eq!(
                body["profiles"]["global"]["settings"],
                json!({ "posterSize": "large" })
            );
            assert_eq!(
                body["profiles"]["tv"]["settings"],
                json!({ "navbarPosition": "left" })
            );
            assert!(body["profiles"]["desktop"].is_null());
            assert_eq!(
                body["resolved"]["tv"],
                json!({ "posterSize": "large", "navbarPosition": "left" })
            );
            assert_eq!(
                body["resolved"]["desktop"],
                json!({ "posterSize": "large", "navbarPosition": "top" })
            );
            assert_eq!(body["resolved"]["mobile"], body["resolved"]["desktop"]);
        }

        h.get(
            &h.token,
            &format!("/Moonfin/Admin/Users/{}/Settings", Uuid::new_v4()),
        )
        .await
        .assert_status(StatusCode::NOT_FOUND);
        h.get(&h.token, "/Moonfin/Admin/Users/not-a-user/Settings")
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn reset_clears_one_profile_or_all_and_tells_that_user_only() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let second = user_id(&h, "second").await;
        let id = simple(second);
        push(&h, &other, "global", json!({ "a": 1 })).await;
        push(&h, &other, "tv", json!({ "b": 2 })).await;
        push(&h, &h.token, "global", json!({ "mine": true })).await;
        let mut theirs = h
            .ctx
            .moonfin_settings
            .subscribe(second);
        let mut admins = h
            .ctx
            .moonfin_settings
            .subscribe(user_id(&h, "test").await);
        let path = format!("/Moonfin/Admin/Users/{id}/Settings");

        // One profile (any case, global included).
        let resp = h
            .delete(&h.token, &format!("{path}?profile=TV"))
            .await;
        resp.assert_status_ok();
        assert_eq!(
            resp.json::<Value>(),
            json!({ "success": true, "deleted": 1 })
        );
        assert_eq!(theirs.try_event(), Some(StreamEvent::SettingsUpdated));
        let detail = h
            .get(&h.token, &path)
            .await
            .json::<Value>();
        assert!(detail["profiles"]["tv"].is_null());
        assert_eq!(detail["profiles"]["global"]["settings"], json!({ "a": 1 }));

        // Everything.
        let resp = h
            .delete(&h.token, &path)
            .await;
        assert_eq!(
            resp.json::<Value>(),
            json!({ "success": true, "deleted": 1 })
        );
        assert_eq!(theirs.try_event(), Some(StreamEvent::SettingsUpdated));
        let detail = h
            .get(&h.token, &path)
            .await
            .json::<Value>();
        assert_eq!(detail["profiles"], json!({}));

        // Nothing left: still a success, and no pointless event.
        let resp = h
            .delete(&h.token, &path)
            .await;
        assert_eq!(
            resp.json::<Value>(),
            json!({ "success": true, "deleted": 0 })
        );
        assert_eq!(theirs.try_event(), None);

        // Other users are untouched and were not notified.
        assert_eq!(admins.try_event(), None);
        let mine = h
            .get(&h.token, "/Moonfin/Settings/Resolved/global")
            .await
            .json::<Value>();
        assert_eq!(mine, json!({ "mine": true }));

        h.delete(&h.token, &format!("{path}?profile=phone"))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        h.delete(
            &h.token,
            &format!("/Moonfin/Admin/Users/{}/Settings", Uuid::new_v4()),
        )
        .await
        .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn status_reports_the_server_without_leaking_secrets() {
        const KEY: &str = "mdblist-secret-key-123";
        let web = tempfile::tempdir().unwrap();
        std::fs::write(
            web.path()
                .join("index.html"),
            "<html></html>",
        )
        .unwrap();
        std::fs::write(
            web.path()
                .join("version.json"),
            r#"{"app_name":"moonfin","version":"1.2.3","build_number":"45"}"#,
        )
        .unwrap();
        let roots = tempfile::tempdir().unwrap();
        let web_path = web
            .path()
            .to_string_lossy()
            .into_owned();
        let root = roots
            .path()
            .to_string_lossy()
            .into_owned();
        let web_path_str = web_path.clone();
        let h = Harness::new_with(|c| {
            c.mdblist_api_key = Some(crate::Secret::new(KEY));
            c.moonfin_web_forced_server_url = Some("https://forced.example".into());
            c.moonfin_web_path = Some(web_path);
            c.games_enabled = true;
            c.games_roots = vec![root, "  ".into()];
            c.games_launchbox_enabled = false;
        })
        .await;
        put_defaults(&h, &h.token, json!({ "settings": { "a": 1, "b": 2 } }))
            .await
            .assert_status_ok();

        let resp = h
            .server
            .get("/Moonfin/Admin/Status")
            .add_header(http::header::AUTHORIZATION, auth(&h.token))
            .add_header(http::header::HOST, "remux.example:8096")
            .add_header("x-forwarded-proto", "https")
            .await;
        resp.assert_status_ok();
        let text = resp.text();
        assert!(!text.contains(KEY), "{text}");
        let body: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(body["ping"]["installed"], true);
        assert_eq!(body["ping"]["mdblistAvailable"], true);
        assert_eq!(body["mdblistConfigured"], true);
        assert_eq!(body["games"]["enabled"], true);
        assert_eq!(body["games"]["active"], true);
        assert_eq!(body["games"]["rootsCount"], 1);
        assert_eq!(body["games"]["gameCount"], 0);
        assert_eq!(body["cores"]["installed"], false);
        assert_eq!(body["emulatorjsCdnFallback"], false);
        assert_eq!(body["metadata"]["launchbox"]["enabled"], false);
        assert_eq!(body["metadata"]["launchbox"]["index"]["indexed"], false);
        assert!(body["metadata"]["libretro"].is_boolean());
        assert_eq!(body["webAppPresent"], true);
        assert_eq!(body["webApp"]["present"], true);
        assert_eq!(body["webApp"]["version"], "1.2.3");
        assert_eq!(body["webApp"]["buildNumber"], "45");
        assert_eq!(body["webApp"]["bundlePath"], web_path_str);
        assert_eq!(
            body["webApp"]["appUrl"],
            "https://remux.example:8096/Moonfin/Web"
        );
        assert_eq!(body["webApp"]["forcedServerUrl"], "https://forced.example");
        assert_eq!(body["webApp"]["enableWebRtcScan"], false);
        assert_eq!(body["themesCount"], 0);
        assert_eq!(body["defaultsKeyCount"], 2);
    }

    #[tokio::test]
    async fn status_reports_the_emulatorjs_cdn_fallback_flag() {
        let h = Harness::new_with(|c| c.games_emulatorjs_cdn_fallback = true).await;
        let body = h
            .get(&h.token, "/Moonfin/Admin/Status")
            .await
            .json::<Value>();
        assert_eq!(body["emulatorjsCdnFallback"], true);
    }

    #[tokio::test]
    async fn status_of_a_bare_server_shows_everything_off() {
        let h = Harness::new().await;
        let body = h
            .get(&h.token, "/Moonfin/Admin/Status")
            .await
            .json::<Value>();
        assert_eq!(body["mdblistConfigured"], false);
        assert_eq!(body["ping"]["mdblistAvailable"], false);
        assert_eq!(body["games"]["active"], false);
        assert_eq!(body["games"]["rootsCount"], 0);
        assert_eq!(body["webAppPresent"], false);
        assert_eq!(body["webApp"]["present"], false);
        assert_eq!(body["webApp"]["version"], Value::Null);
        assert_eq!(body["webApp"]["forcedServerUrl"], Value::Null);
        assert_eq!(body["defaultsKeyCount"], 0);
    }

    #[tokio::test]
    async fn api_keys_are_refused_as_defaults_and_nothing_is_stored() {
        let h = Harness::new().await;
        for key in ["mdblistApiKey", "tmdbApiKey", "anyOtherApiKey"] {
            let resp = put_defaults(
                &h,
                &h.token,
                json!({ "settings": { "posterSize": "small", key: "SECRETVAL" } }),
            )
            .await;
            resp.assert_status(StatusCode::BAD_REQUEST);
            let message = resp.json::<Value>()["error"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(message.contains(key), "{message}");
            assert!(message.contains("Server settings"), "{message}");
            assert!(!message.contains("SECRETVAL"), "{message}");
        }
        let read = h
            .get(&h.token, "/Moonfin/Admin/Defaults")
            .await
            .json::<Value>();
        assert_eq!(read["settings"], json!({}));
    }

    #[tokio::test]
    async fn legacy_api_keys_in_stored_defaults_never_resolve_or_apply() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let second = user_id(&h, "second").await;
        sqlx::query(
            "INSERT INTO moonfin_default_settings (id, data, updated_at) VALUES (1, ?1, 0)",
        )
        .bind(json!({ "a": 1, "mdblistApiKey": "SECRETVAL" }).to_string())
        .execute(
            &h.ctx
                .db,
        )
        .await
        .unwrap();
        let read = h
            .get(&h.token, "/Moonfin/Admin/Defaults")
            .await
            .json::<Value>();
        assert_eq!(read["settings"], json!({ "a": 1 }));
        push(&h, &other, "global", json!({ "b": 2 })).await;
        apply(&h, &h.token, json!({ "scope": "all", "mode": "overwrite" }))
            .await
            .assert_status_ok();
        let after = stored(&h, second, "global")
            .await
            .unwrap();
        assert_eq!(after, json!({ "a": 1, "b": 2 }));
    }

    #[test]
    fn apply_defaults_never_writes_api_keys_even_blank_ones() {
        let obj = |v: Value| {
            v.as_object()
                .unwrap()
                .clone()
        };
        let stored = obj(json!({ "tmdbApiKey": "mine" }));
        let defaults = obj(json!({ "tmdbApiKey": "", "mdblistApiKey": "x", "a": 1 }));
        for mode in [ApplyMode::Merge, ApplyMode::Overwrite] {
            let after = apply_defaults(&stored, &defaults, mode);
            assert_eq!(after.get("tmdbApiKey"), Some(&json!("mine")));
            assert!(!after.contains_key("mdblistApiKey"));
            assert_eq!(after.get("a"), Some(&json!(1)));
        }
    }

    #[tokio::test]
    async fn oversized_defaults_are_refused_like_user_saves() {
        let h = Harness::new().await;
        let many: serde_json::Map<String, Value> = (0..=MAX_PROFILE_KEYS)
            .map(|i| (format!("k{i}"), json!(1)))
            .collect();
        put_defaults(&h, &h.token, json!({ "settings": many }))
            .await
            .assert_status(StatusCode::PAYLOAD_TOO_LARGE);
        let read = h
            .get(&h.token, "/Moonfin/Admin/Defaults")
            .await
            .json::<Value>();
        assert_eq!(read["settings"], json!({}));
    }

    #[tokio::test]
    async fn admin_user_view_hides_api_key_values() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let second = user_id(&h, "second").await;
        push(
            &h,
            &other,
            "global",
            json!({ "mdblistApiKey": "SECRETVAL", "tmdbApiKey": "OTHERSECRET", "a": 1 }),
        )
        .await;
        let resp = h
            .get(
                &h.token,
                &format!("/Moonfin/Admin/Users/{}/Settings", simple(second)),
            )
            .await;
        resp.assert_status_ok();
        let text = resp.text();
        assert!(!text.contains("SECRETVAL"), "{text}");
        assert!(!text.contains("OTHERSECRET"), "{text}");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            body["profiles"]["global"]["settings"],
            json!({ "mdblistApiKey": "set", "tmdbApiKey": "set", "a": 1 })
        );
        assert_eq!(body["resolved"]["global"]["mdblistApiKey"], "set");
    }

    async fn apply(h: &Harness, token: &str, body: Value) -> axum_test::TestResponse {
        h.post(token, "/Moonfin/Admin/Defaults/Apply", &body)
            .await
    }

    async fn stored(h: &Harness, id: Uuid, profile: &str) -> Option<Value> {
        let body = h
            .get(
                &h.token,
                &format!("/Moonfin/Admin/Users/{}/Settings", simple(id)),
            )
            .await
            .json::<Value>();
        let settings = body["profiles"][profile]["settings"].clone();
        (!settings.is_null()).then_some(settings)
    }

    async fn set_defaults(h: &Harness, settings: Value) {
        put_defaults(h, &h.token, json!({ "settings": settings }))
            .await
            .assert_status_ok();
    }

    #[test]
    fn apply_defaults_merge_keeps_user_values_and_overwrite_replaces_them() {
        let obj = |v: Value| {
            v.as_object()
                .unwrap()
                .clone()
        };
        let stored = obj(json!({ "a": "mine", "own": 1, "n": null }));
        let defaults = obj(json!({
            "a": "default", "b": "new", "n": "filled", "nothing": null,
            "subtitleMode": "always",
        }));
        let merged = apply_defaults(&stored, &defaults, ApplyMode::Merge);
        assert_eq!(
            Value::Object(merged),
            json!({ "a": "mine", "own": 1, "b": "new", "n": "filled" })
        );
        let over = apply_defaults(&stored, &defaults, ApplyMode::Overwrite);
        assert_eq!(
            Value::Object(over),
            json!({ "a": "default", "own": 1, "b": "new", "n": "filled" })
        );
    }

    #[test]
    fn apply_defaults_treats_the_home_layout_as_a_pair() {
        let obj = |v: Value| {
            v.as_object()
                .unwrap()
                .clone()
        };
        let defaults =
            obj(json!({ "homeSections": [{ "type": "x" }], "homeRowOrder": ["x"] }));
        // The user has one half: merge leaves the layout alone entirely.
        let stored = obj(json!({ "homeSections": [{ "type": "mine" }] }));
        let merged = apply_defaults(&stored, &defaults, ApplyMode::Merge);
        assert_eq!(merged, stored);
        // With neither half, both arrive.
        let merged = apply_defaults(&obj(json!({})), &defaults, ApplyMode::Merge);
        assert_eq!(Value::Object(merged), Value::Object(defaults.clone()));
    }

    #[tokio::test]
    async fn merge_fills_gaps_for_existing_users_only_and_reports_counts() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let second = user_id(&h, "second").await;
        let admin = user_id(&h, "test").await;
        // The second user has synced; the admin never has.
        push(&h, &other, "global", json!({ "a": "mine" })).await;
        push(&h, &other, "tv", json!({ "a": "tv" })).await;
        set_defaults(&h, json!({ "a": "default", "b": 1, "c": 2 })).await;

        let resp =
            apply(&h, &h.token, json!({ "scope": "all", "mode": "merge" })).await;
        resp.assert_status_ok();
        assert_eq!(
            resp.json::<Value>(),
            json!({ "usersAffected": 1, "keysWritten": 2, "usersSkipped": 0 })
        );
        assert_eq!(
            stored(&h, second, "global")
                .await
                .unwrap(),
            json!({ "a": "mine", "b": 1, "c": 2 })
        );
        // Other profiles are untouched, and a user with nothing stored still
        // just resolves to the defaults.
        assert_eq!(
            stored(&h, second, "tv")
                .await
                .unwrap(),
            json!({ "a": "tv" })
        );
        assert!(
            stored(&h, admin, "global")
                .await
                .is_none()
        );

        // A second run has nothing left to fill.
        let again = apply(&h, &h.token, json!({ "scope": "all", "mode": "merge" }))
            .await
            .json::<Value>();
        assert_eq!(again["usersAffected"], 0);
        assert_eq!(again["keysWritten"], 0);
    }

    #[tokio::test]
    async fn overwrite_sets_default_keys_keeps_the_rest_and_creates_missing_profiles() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let second = user_id(&h, "second").await;
        let admin = user_id(&h, "test").await;
        push(&h, &other, "global", json!({ "a": "mine", "own": true })).await;
        set_defaults(&h, json!({ "a": "default", "b": 1 })).await;

        let resp =
            apply(&h, &h.token, json!({ "scope": "all", "mode": "overwrite" })).await;
        resp.assert_status_ok();
        // second: a changed + b added; admin: a + b created.
        assert_eq!(
            resp.json::<Value>(),
            json!({ "usersAffected": 2, "keysWritten": 4, "usersSkipped": 0 })
        );
        assert_eq!(
            stored(&h, second, "global")
                .await
                .unwrap(),
            json!({ "a": "default", "b": 1, "own": true })
        );
        assert_eq!(
            stored(&h, admin, "global")
                .await
                .unwrap(),
            json!({ "a": "default", "b": 1 })
        );
    }

    #[tokio::test]
    async fn apply_to_one_user_and_a_chosen_profile() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let second = user_id(&h, "second").await;
        let admin = user_id(&h, "test").await;
        push(&h, &other, "tv", json!({ "own": 1 })).await;
        set_defaults(&h, json!({ "b": 1 })).await;

        let resp = apply(
            &h,
            &h.token,
            json!({
                "scope": "user", "userId": simple(second),
                "mode": "merge", "profile": "TV",
            }),
        )
        .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<Value>()["usersAffected"], 1);
        assert_eq!(
            stored(&h, second, "tv")
                .await
                .unwrap(),
            json!({ "own": 1, "b": 1 })
        );
        assert!(
            stored(&h, admin, "tv")
                .await
                .is_none()
        );

        for (body, status) in [
            (json!({ "scope": "user", "mode": "merge" }), 400),
            (
                json!({ "scope": "user", "userId": "nope", "mode": "merge" }),
                400,
            ),
            (
                json!({ "scope": "user", "userId": simple(Uuid::new_v4()), "mode": "merge" }),
                404,
            ),
            (json!({ "scope": "all", "mode": "replace" }), 400),
            (
                json!({ "scope": "all", "mode": "merge", "profile": "watch" }),
                400,
            ),
            (json!({ "mode": "merge" }), 400),
        ] {
            assert_eq!(
                apply(&h, &h.token, body.clone())
                    .await
                    .status_code(),
                status,
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn apply_tells_only_the_users_it_changed() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let second = user_id(&h, "second").await;
        push(&h, &other, "global", json!({ "a": 1 })).await;
        set_defaults(&h, json!({ "b": 1 })).await;
        let mut theirs = h
            .ctx
            .moonfin_settings
            .subscribe(second);
        let mut admins = h
            .ctx
            .moonfin_settings
            .subscribe(user_id(&h, "test").await);
        // Defaults saved above already notified; start from a clean slate.
        while theirs
            .try_event()
            .is_some()
        {}
        while admins
            .try_event()
            .is_some()
        {}

        apply(&h, &h.token, json!({ "scope": "all", "mode": "merge" }))
            .await
            .assert_status_ok();
        assert_eq!(theirs.try_event(), Some(StreamEvent::SettingsUpdated));
        assert_eq!(admins.try_event(), None);

        // Nothing changes the second time, so nobody is told.
        apply(&h, &h.token, json!({ "scope": "all", "mode": "merge" }))
            .await
            .assert_status_ok();
        assert_eq!(theirs.try_event(), None);
    }

    #[tokio::test]
    async fn apply_skips_users_whose_profile_would_exceed_the_caps() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let second = user_id(&h, "second").await;
        // Fill the second user's profile to the key cap directly.
        let full: serde_json::Map<String, Value> = (0..MAX_PROFILE_KEYS)
            .map(|i| (format!("k{i}"), json!(1)))
            .collect();
        sqlx::query(
            "INSERT INTO moonfin_settings (user_id, profile, data, updated_at, updated_by) \
             VALUES (?1, 'global', ?2, 0, 'test')",
        )
        .bind(second)
        .bind(Value::Object(full).to_string())
        .execute(&h.ctx.db)
        .await
        .unwrap();
        let _ = other;
        set_defaults(&h, json!({ "extra": 1 })).await;

        let body = apply(&h, &h.token, json!({ "scope": "all", "mode": "overwrite" }))
            .await
            .json::<Value>();
        // Only the admin (no stored profile) got the default.
        assert_eq!(body["usersAffected"], 1);
        assert_eq!(body["usersSkipped"], 1);
        let kept = stored(&h, second, "global")
            .await
            .unwrap();
        assert!(
            kept.get("extra")
                .is_none()
        );
        assert_eq!(
            kept.as_object()
                .unwrap()
                .len(),
            MAX_PROFILE_KEYS
        );
    }

    #[tokio::test]
    async fn apply_with_no_defaults_changes_nothing() {
        let h = Harness::new().await;
        let body = apply(&h, &h.token, json!({ "scope": "all", "mode": "overwrite" }))
            .await
            .json::<Value>();
        assert_eq!(
            body,
            json!({ "usersAffected": 0, "keysWritten": 0, "usersSkipped": 0 })
        );
    }

    #[tokio::test]
    async fn apply_needs_an_admin() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let body = json!({ "scope": "all", "mode": "merge" });
        assert_eq!(
            h.server
                .post("/Moonfin/Admin/Defaults/Apply")
                .json(&body)
                .await
                .status_code(),
            401
        );
        assert_eq!(
            apply(&h, &other, body)
                .await
                .status_code(),
            403
        );
    }
}
