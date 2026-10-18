//! Runtime-editable Moonfin server settings, an override layer over the
//! operator `Config` (`GET|PUT /moonfin/admin/server-settings`, admin only).
//!
//! Remux's own precedent is `/system/configuration`: settings an admin edits
//! live in the database (here the generic `settings` key/value table, one JSON
//! document) and are changed from the dashboard without a restart. Four
//! Moonfin settings that were environment-only are editable the same way:
//!
//! | field                  | config key                         |
//! |------------------------|------------------------------------|
//! | `mdblistApiKey` (secret) | `mdblist_api_key`                |
//! | `webDefaultServerUrl`  | `moonfin_web_default_server_url`   |
//! | `webForcedServerUrl`   | `moonfin_web_forced_server_url`    |
//! | `webEnableWebrtcScan`  | `moonfin_web_enable_webrtc_scan`   |
//!
//! Precedence per field: a database value, when set, wins; otherwise the
//! `Config` (environment/file) value applies. Clearing the override
//! (`null` or `""`) falls back to the `Config` value. The MDBList key is
//! write-only: it is never returned, never logged, only reported as
//! configured or not and where it comes from. Every consumer (the MDBList
//! proxy, `Ping`, the web app `config.json`, the admin status) reads the
//! effective value through [`effective`].

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use remux_macros::{get, put};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::SqlitePool;

use super::{
    moonfin_admin::{json_error, require_admin},
    moonfin_settings::internal_error,
};
use crate::{AppState, Config, db::auth::AuthSession};

const SETTINGS_KEY: &str = "moonfin_server_settings";
const MAX_BODY_BYTES: usize = 16 * 1024;
const MAX_KEY_CHARS: usize = 256;
const MAX_URL_CHARS: usize = 2048;

/// The database overrides as stored. `None` means "not overridden".
#[derive(Default, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Overrides {
    pub mdblist_api_key: Option<String>,
    pub web_default_server_url: Option<String>,
    pub web_forced_server_url: Option<String>,
    pub web_enable_webrtc_scan: Option<bool>,
}

// A derived `Debug` would print the key.
impl std::fmt::Debug for Overrides {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Overrides")
            .field(
                "mdblist_api_key",
                &self
                    .mdblist_api_key
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .field("web_default_server_url", &self.web_default_server_url)
            .field("web_forced_server_url", &self.web_forced_server_url)
            .field("web_enable_webrtc_scan", &self.web_enable_webrtc_scan)
            .finish()
    }
}

/// Where an effective value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Source {
    Database,
    Environment,
}

/// The settings consumers act on: database override, else `Config`.
pub(crate) struct Effective {
    /// Trimmed, non-blank key and its origin.
    pub mdblist_key: Option<(String, Source)>,
    pub web_default_server_url: Option<(String, Source)>,
    pub web_forced_server_url: Option<(String, Source)>,
    pub web_enable_webrtc_scan: (bool, Source),
}

impl Effective {
    pub fn mdblist_key(&self) -> Option<&str> {
        self.mdblist_key
            .as_ref()
            .map(|(k, _)| k.as_str())
    }
}

fn non_blank(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn pick(
    db_value: Option<&str>,
    config_value: Option<&str>,
) -> Option<(String, Source)> {
    non_blank(db_value)
        .map(|v| (v, Source::Database))
        .or_else(|| non_blank(config_value).map(|v| (v, Source::Environment)))
}

/// Pure precedence rule: database value if set, else `config`.
pub(crate) fn merge(config: &Config, overrides: &Overrides) -> Effective {
    Effective {
        mdblist_key: pick(
            overrides
                .mdblist_api_key
                .as_deref(),
            config.mdblist_key(),
        ),
        web_default_server_url: pick(
            overrides
                .web_default_server_url
                .as_deref(),
            config
                .moonfin_web_default_server_url
                .as_deref(),
        ),
        web_forced_server_url: pick(
            overrides
                .web_forced_server_url
                .as_deref(),
            config
                .moonfin_web_forced_server_url
                .as_deref(),
        ),
        web_enable_webrtc_scan: match overrides.web_enable_webrtc_scan {
            Some(v) => (v, Source::Database),
            None => (config.moonfin_web_enable_webrtc_scan, Source::Environment),
        },
    }
}

/// The stored overrides; a missing or unreadable document means none.
pub(crate) async fn load_overrides(db: &SqlitePool) -> Overrides {
    match crate::db::Settings::get(db, SETTINGS_KEY).await {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
        Ok(None) => Overrides::default(),
        Err(e) => {
            tracing::error!(err = %e, "Failed to load Moonfin server settings");
            Overrides::default()
        }
    }
}

/// The effective settings for the running server.
pub(crate) async fn effective(state: &AppState) -> Effective {
    merge(
        &state
            .ctx
            .config,
        &load_overrides(
            &state
                .ctx
                .db,
        )
        .await,
    )
}

fn source_name(source: Option<Source>) -> Value {
    match source {
        Some(Source::Database) => json!("database"),
        Some(Source::Environment) => json!("environment"),
        None => Value::Null,
    }
}

/// The admin-facing view. Never contains the MDBList key.
pub(crate) fn view(eff: &Effective) -> Value {
    json!({
        "mdblistKeyConfigured": eff.mdblist_key.is_some(),
        "mdblistKeySource": source_name(eff.mdblist_key.as_ref().map(|(_, s)| *s)),
        "webDefaultServerUrl": eff.web_default_server_url.as_ref().map(|(v, _)| v),
        "webForcedServerUrl": eff.web_forced_server_url.as_ref().map(|(v, _)| v),
        "webEnableWebrtcScan": eff.web_enable_webrtc_scan.0,
        "sources": {
            "webDefaultServerUrl": source_name(eff.web_default_server_url.as_ref().map(|(_, s)| *s)),
            "webForcedServerUrl": source_name(eff.web_forced_server_url.as_ref().map(|(_, s)| *s)),
            "webEnableWebrtcScan": source_name(Some(eff.web_enable_webrtc_scan.1)),
        },
    })
}

#[get("/moonfin/admin/server-settings")]
pub async fn moonfin_admin_server_settings_get(
    State(state): State<AppState>,
    session: AuthSession,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    Json(view(&effective(&state).await)).into_response()
}

/// How a PUT field changes an override.
enum Change<T> {
    Keep,
    Clear,
    Set(T),
}

fn text_change(
    body: &Map<String, Value>,
    field: &str,
    max_chars: usize,
    validate: fn(&str) -> Result<(), &'static str>,
) -> Result<Change<String>, String> {
    match body.get(field) {
        None => Ok(Change::Keep),
        Some(Value::Null) => Ok(Change::Clear),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                return Ok(Change::Clear);
            }
            if s.chars()
                .count()
                > max_chars
            {
                return Err(format!("{field} is too long"));
            }
            validate(s).map_err(|why| format!("{field}: {why}"))?;
            Ok(Change::Set(s.to_string()))
        }
        Some(_) => Err(format!("{field} must be a string or null")),
    }
}

fn validate_key(key: &str) -> Result<(), &'static str> {
    if key
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("must not contain whitespace");
    }
    Ok(())
}

fn validate_url(url: &str) -> Result<(), &'static str> {
    match reqwest::Url::parse(url) {
        Ok(u)
            if matches!(u.scheme(), "http" | "https")
                && u.host()
                    .is_some() =>
        {
            Ok(())
        }
        _ => Err("must be an absolute http(s) URL"),
    }
}

fn apply<T>(slot: &mut Option<T>, change: Change<T>) {
    match change {
        Change::Keep => {}
        Change::Clear => *slot = None,
        Change::Set(v) => *slot = Some(v),
    }
}

/// Sets or clears each present field: a value sets the override, `null` or
/// `""` clears it, an absent field is left alone.
#[put("/moonfin/admin/server-settings")]
pub async fn moonfin_admin_server_settings_put(
    State(state): State<AppState>,
    session: AuthSession,
    req: Request<Body>,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
        }
    };
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&bytes) else {
        return json_error(StatusCode::BAD_REQUEST, "Body must be a JSON object");
    };
    let parsed = (|| {
        let key = text_change(&body, "mdblistApiKey", MAX_KEY_CHARS, validate_key)?;
        let default_url =
            text_change(&body, "webDefaultServerUrl", MAX_URL_CHARS, validate_url)?;
        let forced_url =
            text_change(&body, "webForcedServerUrl", MAX_URL_CHARS, validate_url)?;
        let webrtc = match body.get("webEnableWebrtcScan") {
            None => Change::Keep,
            Some(Value::Null) => Change::Clear,
            Some(Value::Bool(b)) => Change::Set(*b),
            Some(_) => {
                return Err("webEnableWebrtcScan must be a boolean or null".to_string());
            }
        };
        Ok::<_, String>((key, default_url, forced_url, webrtc))
    })();
    let (key, default_url, forced_url, webrtc) = match parsed {
        Ok(p) => p,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };

    let _write = state
        .ctx
        .moonfin_settings
        .write_lock()
        .await;
    let db = &state
        .ctx
        .db;
    let mut overrides = load_overrides(db).await;
    apply(&mut overrides.mdblist_api_key, key);
    apply(&mut overrides.web_default_server_url, default_url);
    apply(&mut overrides.web_forced_server_url, forced_url);
    apply(&mut overrides.web_enable_webrtc_scan, webrtc);
    let stored = match serde_json::to_string(&overrides) {
        Ok(s) => s,
        Err(e) => return internal_error(e),
    };
    if let Err(e) = crate::db::Settings::set(db, SETTINGS_KEY, &stored).await {
        return internal_error(e);
    }
    Json(view(&merge(
        &state
            .ctx
            .config,
        &overrides,
    )))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Secret, api::moonfin_test_support::Harness};

    const KEY: &str = "fake-db-key-777";
    const ENV_KEY: &str = "fake-env-key-888";

    #[test]
    fn database_wins_over_config_and_blank_falls_back() {
        let config = Config {
            mdblist_api_key: Some(Secret::new(ENV_KEY)),
            moonfin_web_default_server_url: Some("https://env.example".into()),
            moonfin_web_enable_webrtc_scan: true,
            ..Default::default()
        };
        let none = merge(&config, &Overrides::default());
        assert_eq!(
            none.mdblist_key,
            Some((ENV_KEY.into(), Source::Environment))
        );
        assert_eq!(none.web_enable_webrtc_scan, (true, Source::Environment));
        assert_eq!(none.web_forced_server_url, None);

        let over = Overrides {
            mdblist_api_key: Some(KEY.into()),
            web_default_server_url: Some("https://db.example".into()),
            web_enable_webrtc_scan: Some(false),
            ..Default::default()
        };
        let eff = merge(&config, &over);
        assert_eq!(eff.mdblist_key, Some((KEY.into(), Source::Database)));
        assert_eq!(
            eff.web_default_server_url,
            Some(("https://db.example".into(), Source::Database))
        );
        assert_eq!(eff.web_enable_webrtc_scan, (false, Source::Database));

        let blank = Overrides {
            mdblist_api_key: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(
            merge(&config, &blank).mdblist_key,
            Some((ENV_KEY.into(), Source::Environment))
        );
    }

    #[test]
    fn overrides_debug_hides_the_key() {
        let over = Overrides {
            mdblist_api_key: Some(KEY.into()),
            ..Default::default()
        };
        assert!(!format!("{over:?}").contains(KEY));
    }

    #[tokio::test]
    async fn routes_need_an_admin() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        assert_eq!(
            h.server
                .get("/moonfin/admin/server-settings")
                .await
                .status_code(),
            401
        );
        assert_eq!(
            h.get(&other, "/moonfin/admin/server-settings")
                .await
                .status_code(),
            403
        );
        let resp = h
            .server
            .put("/moonfin/admin/server-settings")
            .json(&json!({}))
            .await;
        assert_eq!(resp.status_code(), 401);
        let resp = h
            .server
            .put("/moonfin/admin/server-settings")
            .add_header(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(
                    &crate::integration_test::auth_header_with_token(&other),
                )
                .unwrap(),
            )
            .json(&json!({}))
            .await;
        assert_eq!(resp.status_code(), 403);
    }

    async fn put(h: &Harness, body: Value) -> axum_test::TestResponse {
        h.server
            .put("/moonfin/admin/server-settings")
            .add_header(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(
                    &crate::integration_test::auth_header_with_token(&h.token),
                )
                .unwrap(),
            )
            .json(&body)
            .await
    }

    #[tokio::test]
    async fn set_get_clear_round_trip_never_returns_the_key() {
        let h = Harness::new_with(|c| {
            c.moonfin_web_default_server_url = Some("https://env.example/".into());
        })
        .await;
        let before = h
            .get(&h.token, "/moonfin/admin/server-settings")
            .await
            .json::<Value>();
        assert_eq!(before["mdblistKeyConfigured"], false);
        assert_eq!(before["mdblistKeySource"], Value::Null);
        assert_eq!(before["webDefaultServerUrl"], "https://env.example/");
        assert_eq!(before["sources"]["webDefaultServerUrl"], "environment");

        let resp = put(
            &h,
            json!({
                "mdblistApiKey": KEY,
                "webDefaultServerUrl": "https://db.example",
                "webForcedServerUrl": "https://forced.example",
                "webEnableWebrtcScan": true,
            }),
        )
        .await;
        resp.assert_status_ok();
        assert!(
            !resp
                .text()
                .contains(KEY)
        );
        let got = h
            .get(&h.token, "/moonfin/admin/server-settings")
            .await;
        assert!(
            !got.text()
                .contains(KEY)
        );
        let got = got.json::<Value>();
        assert_eq!(got["mdblistKeyConfigured"], true);
        assert_eq!(got["mdblistKeySource"], "database");
        assert_eq!(got["webDefaultServerUrl"], "https://db.example");
        assert_eq!(got["webForcedServerUrl"], "https://forced.example");
        assert_eq!(got["webEnableWebrtcScan"], true);
        assert_eq!(got["sources"]["webDefaultServerUrl"], "database");

        // An absent field is untouched; an empty string / null clears.
        put(&h, json!({ "webForcedServerUrl": "" }))
            .await
            .assert_status_ok();
        let after = put(
            &h,
            json!({ "mdblistApiKey": null, "webDefaultServerUrl": null }),
        )
        .await
        .json::<Value>();
        assert_eq!(after["mdblistKeyConfigured"], false);
        assert_eq!(after["webForcedServerUrl"], Value::Null);
        // Cleared override falls back to the environment value.
        assert_eq!(after["webDefaultServerUrl"], "https://env.example/");
        assert_eq!(after["sources"]["webDefaultServerUrl"], "environment");
        assert_eq!(after["webEnableWebrtcScan"], true);
    }

    #[tokio::test]
    async fn env_key_is_reported_as_environment_and_db_overrides_it() {
        let h = Harness::new_with(|c| {
            c.mdblist_api_key = Some(Secret::new(ENV_KEY));
        })
        .await;
        let got = h
            .get(&h.token, "/moonfin/admin/server-settings")
            .await
            .json::<Value>();
        assert_eq!(got["mdblistKeySource"], "environment");
        assert!(
            !got.to_string()
                .contains(ENV_KEY)
        );
        let got = put(&h, json!({ "mdblistApiKey": KEY }))
            .await
            .json::<Value>();
        assert_eq!(got["mdblistKeySource"], "database");
    }

    #[tokio::test]
    async fn invalid_values_are_rejected_and_change_nothing() {
        let h = Harness::new().await;
        for bad in [
            json!({ "webDefaultServerUrl": "ftp://x" }),
            json!({ "webForcedServerUrl": "not a url" }),
            json!({ "mdblistApiKey": "has space" }),
            json!({ "mdblistApiKey": 5 }),
            json!({ "webEnableWebrtcScan": "yes" }),
        ] {
            assert_eq!(
                put(&h, bad)
                    .await
                    .status_code(),
                400
            );
        }
        // One bad field rejects the whole request.
        assert_eq!(
            put(
                &h,
                json!({ "mdblistApiKey": KEY, "webDefaultServerUrl": "nope" })
            )
            .await
            .status_code(),
            400
        );
        let got = h
            .get(&h.token, "/moonfin/admin/server-settings")
            .await
            .json::<Value>();
        assert_eq!(got["mdblistKeyConfigured"], false);
    }

    #[tokio::test]
    async fn consumers_read_the_effective_value() {
        let h = Harness::new().await;
        // Ping and the admin status follow the database key.
        assert_eq!(
            h.get(&h.token, "/Moonfin/Ping")
                .await
                .json::<Value>()["mdblistAvailable"],
            false
        );
        put(
            &h,
            json!({
                "mdblistApiKey": KEY,
                "webDefaultServerUrl": "https://db.example/",
                "webForcedServerUrl": "https://forced.example/web/",
                "webEnableWebrtcScan": true,
            }),
        )
        .await
        .assert_status_ok();
        assert_eq!(
            h.get(&h.token, "/Moonfin/Ping")
                .await
                .json::<Value>()["mdblistAvailable"],
            true
        );
        let status = h
            .get(&h.token, "/Moonfin/Admin/Status")
            .await
            .json::<Value>();
        assert_eq!(status["mdblistConfigured"], true);
        assert_eq!(status["mdblistKeySource"], "database");
        assert!(
            !status
                .to_string()
                .contains(KEY)
        );
        let cfg = h
            .server
            .get("/Moonfin/Web/config.json")
            .await
            .json::<Value>();
        assert_eq!(cfg["defaultServerUrl"], "https://db.example");
        assert_eq!(cfg["forcedServerUrl"], "https://forced.example");
        assert_eq!(cfg["enableWebRtcScan"], true);
    }
}
