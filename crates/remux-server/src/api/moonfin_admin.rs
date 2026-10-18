//! Admin API for the Moonfin settings that Remux hosts (all routes need an
//! admin session; anonymous callers get `401`, other users `403`):
//!
//! * `GET|PUT /Moonfin/Admin/Defaults`: the admin default settings, the
//!   lowest layer when a user's settings are resolved. A schemaless JSON
//!   object, except that keys Remux owns ([`ADMIN_BLOCKED_KEYS`]) are refused.
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
use remux_macros::{delete, get, put};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::moonfin_settings::{
    ADMIN_BLOCKED_KEYS, MAX_SAVE_BYTES, StreamEvent, VALID_PROFILES, blocked_keys_in,
    internal_error, load_defaults, load_profiles, parse_profile, ping_body,
    resolved_profile, save_defaults,
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
            profiles.insert(
                profile.to_string(),
                json!({
                    "settings": settings,
                    "updatedAt": updated_at,
                    "updatedBy": updated_by,
                }),
            );
        }
        resolved.insert(
            profile.to_string(),
            Value::Object(resolved_profile(state, id, profile).await?),
        );
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
}
