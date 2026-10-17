//! Moonfin server-hosted themes
//! (`GET /Moonfin/Themes[/{id}]`, admin `GET|POST /Moonfin/Admin/Themes` and
//! `DELETE /Moonfin/Admin/Themes/{id}`).
//!
//! Wire behaviour follows Moonbase's `MoonfinThemesController` and
//! `MoonfinThemeValidator`:
//!
//! * Any signed-in user lists themes (a bare JSON array of the stored theme
//!   objects, ordered by display name) or fetches one by id.
//! * Admins upload a theme by POSTing the theme JSON object. Posting an id
//!   that already exists replaces it. Uploads are validated against the
//!   cross-client schema and answered `400` with an `errors` list otherwise.
//! * The submitted text is stored verbatim, so unknown keys and key order
//!   reach the client untouched.
//! * Remux ships no themes: the list starts empty. The four ids the client
//!   bundles (`moonfin`, `neon_pulse`, `glass`, `8bit_hero`) are refused,
//!   because the client silently drops a server theme with one of them.
//! * Non-admins get `403` (not the `401` of the shared admin extractor, which
//!   a client may treat as an expired session).

use std::fmt::Write as _;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use remux_macros::{delete, get, post};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{
    AppState,
    api::moonfin_settings::{StreamEvent, internal_error},
    db::auth::AuthSession,
};

/// Largest accepted theme body.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Theme ids the client bundles; a server theme with one of these is dropped
/// by the client, so uploads are refused up front.
const RESERVED_IDS: [&str; 4] = ["moonfin", "neon_pulse", "glass", "8bit_hero"];

const COLOR_KEYS: [&str; 29] = [
    "background",
    "onBackground",
    "surface",
    "onSurface",
    "surfaceVariant",
    "scrim",
    "accent",
    "onAccent",
    "buttonNormal",
    "buttonFocused",
    "buttonDisabled",
    "buttonActive",
    "onButtonNormal",
    "onButtonFocused",
    "onButtonDisabled",
    "inputBackground",
    "inputFocused",
    "inputBorder",
    "inputBorderFocused",
    "rangeTrack",
    "rangeProgress",
    "rangeThumb",
    "seekbarBuffered",
    "badgeBackground",
    "onBadge",
    "badgeUnplayed",
    "badgeWatched",
    "recordingActive",
    "recordingScheduled",
];
const SEMANTIC_KEYS: [&str; 6] = [
    "statusAvailable",
    "statusRequested",
    "statusPending",
    "statusDownloading",
    "mediaTypeBadgeMovie",
    "mediaTypeBadgeShow",
];
const BOOK_KEYS: [&str; 11] = [
    "background",
    "accent",
    "mutedText",
    "primaryText",
    "sectionTitle",
    "divider",
    "placeholder",
    "shadow",
    "gradientTop",
    "gradientBottom",
    "inactiveChip",
];
const CORNER_KEYS: [&str; 4] = ["topLeft", "topRight", "bottomLeft", "bottomRight"];

fn is_valid_id(id: &str) -> bool {
    (2..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

fn is_hex_color(s: &str) -> bool {
    s.strip_prefix('#')
        .is_some_and(|h| {
            matches!(h.len(), 6 | 8)
                && h.bytes()
                    .all(|b| b.is_ascii_hexdigit())
        })
}

/// A theme that passed validation: the verbatim text plus the fields stored
/// alongside it.
#[derive(Debug)]
struct Theme {
    id: String,
    display_name: String,
    text: String,
}

/// Why a body is not a storable theme.
#[derive(Debug)]
enum ThemeRejection {
    TooLarge,
    Invalid {
        error: &'static str,
        errors: Vec<String>,
    },
}

impl Theme {
    fn parse(body: &[u8]) -> Result<Self, ThemeRejection> {
        if body.len() > MAX_BODY_BYTES {
            return Err(ThemeRejection::TooLarge);
        }
        let invalid = |error, msg: &str| ThemeRejection::Invalid {
            error,
            errors: vec![msg.to_string()],
        };
        let text = std::str::from_utf8(body)
            .map_err(|_| invalid("Invalid JSON body.", "Body must be UTF-8 JSON."))?;
        let value: Value = serde_json::from_str(text).map_err(|e| {
            invalid("Invalid JSON body.", &format!("Invalid JSON: {e}"))
        })?;
        let Value::Object(obj) = &value else {
            return Err(invalid(
                "Theme payload must be a JSON object.",
                "Theme payload must be a JSON object.",
            ));
        };
        let mut errors = Vec::new();
        let (id, display_name) = validate(obj, &mut errors);
        if !errors.is_empty() {
            return Err(ThemeRejection::Invalid {
                error: "Theme validation failed.",
                errors,
            });
        }
        Ok(Self {
            id,
            display_name,
            text: text.to_string(),
        })
    }
}

fn required_string(
    obj: &Map<String, Value>,
    key: &str,
    errors: &mut Vec<String>,
) -> String {
    match obj.get(key) {
        None => errors.push(format!("{key} is required.")),
        Some(Value::String(s))
            if s.trim()
                .is_empty() =>
        {
            errors.push(format!("{key} cannot be empty."))
        }
        Some(Value::String(s)) => {
            return s
                .trim()
                .to_string();
        }
        Some(_) => errors.push(format!("{key} must be a string.")),
    }
    String::new()
}

fn has_script_tag(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.contains("<script") || lower.contains("</script")
}

fn required_object<'a>(
    obj: &'a Map<String, Value>,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<&'a Map<String, Value>> {
    match obj.get(key) {
        None => errors.push(format!("{key} is required.")),
        Some(Value::Object(o)) => return Some(o),
        Some(_) => errors.push(format!("{key} must be an object.")),
    }
    None
}

fn check_color(v: &Value, path: &str, errors: &mut Vec<String>) {
    match v {
        Value::String(s) if is_hex_color(s) => {}
        Value::String(_) => errors.push(format!(
            "{path} must be a valid color (#RRGGBB or #AARRGGBB)."
        )),
        _ => errors.push(format!(
            "{path} must be a string color (#RRGGBB or #AARRGGBB)."
        )),
    }
}

fn required_color(
    obj: &Map<String, Value>,
    key: &str,
    path: &str,
    errors: &mut Vec<String>,
) {
    match obj.get(key) {
        None => errors.push(format!("{path} is required.")),
        Some(v) => check_color(v, path, errors),
    }
}

fn optional_color(
    obj: &Map<String, Value>,
    key: &str,
    path: &str,
    errors: &mut Vec<String>,
) {
    if let Some(v) = obj.get(key) {
        check_color(v, path, errors);
    }
}

fn check_number(v: &Value, path: &str, min: f64, max: f64, errors: &mut Vec<String>) {
    match v.as_f64() {
        None => errors.push(format!("{path} must be a number.")),
        Some(n) if n < min || n > max => {
            errors.push(format!("{path} must be between {min} and {max}."))
        }
        Some(_) => {}
    }
}

fn required_number(
    obj: &Map<String, Value>,
    key: &str,
    path: &str,
    min: f64,
    max: f64,
    errors: &mut Vec<String>,
) {
    match obj.get(key) {
        None => errors.push(format!("{path} is required.")),
        Some(v) => check_number(v, path, min, max, errors),
    }
}

fn check_border(border: &Map<String, Value>, path: &str, errors: &mut Vec<String>) {
    required_color(border, "color", &format!("{path}.color"), errors);
    required_number(border, "width", &format!("{path}.width"), 0.0, 16.0, errors);
}

fn required_border(
    obj: &Map<String, Value>,
    key: &str,
    path: &str,
    errors: &mut Vec<String>,
) {
    match obj.get(key) {
        None => errors.push(format!("{path} is required.")),
        Some(Value::Object(o)) => check_border(o, path, errors),
        Some(_) => errors.push(format!("{path} must be an object.")),
    }
}

fn check_radius(
    obj: &Map<String, Value>,
    key: &str,
    path: &str,
    errors: &mut Vec<String>,
) {
    match obj.get(key) {
        None => errors.push(format!("{path} is required.")),
        Some(Value::Number(_)) => check_number(&obj[key], path, 0.0, 9999.0, errors),
        Some(Value::Object(o)) => {
            for corner in CORNER_KEYS {
                required_number(
                    o,
                    corner,
                    &format!("{path}.{corner}"),
                    0.0,
                    9999.0,
                    errors,
                );
            }
        }
        Some(_) => errors.push(format!("{path} must be a number or corner object.")),
    }
}

fn check_shadows(v: &Value, path: &str, allow_spread: bool, errors: &mut Vec<String>) {
    let Value::Array(items) = v else {
        errors.push(format!("{path} must be an array."));
        return;
    };
    if items.len() > 8 {
        errors.push(format!("{path} must contain at most 8 entries."));
    }
    for (i, item) in items
        .iter()
        .enumerate()
    {
        let p = format!("{path}[{i}]");
        let Value::Object(o) = item else {
            errors.push(format!("{p} must be an object."));
            continue;
        };
        required_color(o, "color", &format!("{p}.color"), errors);
        required_number(
            o,
            "blurRadius",
            &format!("{p}.blurRadius"),
            0.0,
            64.0,
            errors,
        );
        required_number(o, "offsetX", &format!("{p}.offsetX"), -500.0, 500.0, errors);
        required_number(o, "offsetY", &format!("{p}.offsetY"), -500.0, 500.0, errors);
        match o.get("spreadRadius") {
            Some(s) if allow_spread => {
                check_number(s, &format!("{p}.spreadRadius"), -32.0, 32.0, errors)
            }
            Some(s) => {
                if s.as_f64() != Some(0.0) {
                    errors.push(format!("{p}.spreadRadius must be 0 for textGlow."));
                }
            }
            None if allow_spread => {
                errors.push(format!("{p}.spreadRadius is required."))
            }
            None => {}
        }
    }
}

fn check_color_array(
    v: &Value,
    path: &str,
    range: std::ops::RangeInclusive<usize>,
    count_msg: &str,
    errors: &mut Vec<String>,
) {
    let Value::Array(items) = v else {
        errors.push(format!("{path} must be an array."));
        return;
    };
    if !range.contains(&items.len()) {
        errors.push(count_msg.to_string());
    }
    for (i, c) in items
        .iter()
        .enumerate()
    {
        check_color(c, &format!("{path}[{i}]"), errors);
    }
}

/// Runs the full Moonbase schema check; returns the trimmed `(id,
/// displayName)` (empty when absent or invalid, in which case `errors` is
/// non-empty).
fn validate(obj: &Map<String, Value>, errors: &mut Vec<String>) -> (String, String) {
    if let Some(v) = obj.get("schemaVersion") {
        match v.as_i64() {
            None => errors.push("schemaVersion must be an integer.".into()),
            Some(n) if n > 1 => errors.push("schemaVersion must be 1 or lower.".into()),
            Some(_) => {}
        }
    }

    let id = required_string(obj, "id", errors);
    if !id.is_empty() {
        if !is_valid_id(&id) {
            errors.push("id must match ^[a-z0-9_-]{2,40}$.".into());
        } else if RESERVED_IDS.contains(&id.as_str()) {
            errors.push("id conflicts with a protected built-in theme.".into());
        }
    }

    let display_name = required_string(obj, "displayName", errors);
    if has_script_tag(&display_name) {
        errors.push("displayName cannot contain script tags.".into());
    }
    for key in ["description", "fontFamily"] {
        match obj.get(key) {
            Some(Value::String(s)) if key == "description" && has_script_tag(s) => {
                errors.push("description cannot contain script tags.".into())
            }
            Some(Value::String(_)) | None => {}
            Some(_) => errors.push(format!("{key} must be a string.")),
        }
    }
    for key in ["transparentNavbarSurface", "isGlass", "isPixel"] {
        if obj
            .get(key)
            .is_some_and(|v| !v.is_boolean())
        {
            errors.push(format!("{key} must be true or false."));
        }
    }

    if let Some(colors) = required_object(obj, "colors", errors) {
        for key in COLOR_KEYS {
            required_color(colors, key, &format!("colors.{key}"), errors);
        }
        for key in ["error", "card"] {
            optional_color(colors, key, &format!("colors.{key}"), errors);
        }
    }

    if let Some(semantic) = required_object(obj, "semantic", errors) {
        for key in SEMANTIC_KEYS {
            required_color(semantic, key, &format!("semantic.{key}"), errors);
        }
        optional_color(semantic, "statusError", "semantic.statusError", errors);
    }

    if let Some(book) = required_object(obj, "book", errors) {
        for key in BOOK_KEYS {
            required_color(book, key, &format!("book.{key}"), errors);
        }
        match book.get("placeholderPalette") {
            None => errors.push("book.placeholderPalette is required.".into()),
            Some(v) => check_color_array(
                v,
                "book.placeholderPalette",
                1..=16,
                "book.placeholderPalette must contain 1 to 16 colors.",
                errors,
            ),
        }
    }

    if let Some(borders) = required_object(obj, "borders", errors) {
        for key in ["cardBorder", "chipBorder", "focusBorder"] {
            required_border(borders, key, &format!("borders.{key}"), errors);
        }
        check_radius(borders, "cardRadius", "borders.cardRadius", errors);
        check_radius(borders, "chipRadius", "borders.chipRadius", errors);
        required_color(borders, "chipBackground", "borders.chipBackground", errors);
        match borders.get("navBorder") {
            None | Some(Value::Null) => {}
            Some(Value::Object(o)) => check_border(o, "borders.navBorder", errors),
            Some(_) => {
                errors.push("borders.navBorder must be an object when provided.".into())
            }
        }
        match borders.get("focusGlow") {
            None => errors.push("borders.focusGlow is required.".into()),
            Some(v) => check_shadows(v, "borders.focusGlow", true, errors),
        }
    }

    if let Some(v) = obj.get("textGlow") {
        check_shadows(v, "textGlow", false, errors);
    }
    if let Some(v) = obj.get("navColorCycle") {
        check_color_array(
            v,
            "navColorCycle",
            0..=16,
            "navColorCycle must contain at most 16 colors.",
            errors,
        );
    }

    (id, display_name)
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "Theme not found")
}

fn json_text(status: StatusCode, text: String) -> Response {
    (
        status,
        [(http::header::CONTENT_TYPE, "application/json")],
        text,
    )
        .into_response()
}

/// `Some(403)` response unless the caller is an admin.
fn require_admin(session: &AuthSession) -> Option<Response> {
    (!session
        .user
        .is_admin)
        .then(|| json_error(StatusCode::FORBIDDEN, "Admin access required"))
}

#[derive(sqlx::FromRow)]
struct ThemeRow {
    id: String,
    display_name: String,
    json: String,
    size_bytes: i64,
    sha256: String,
    uploaded_at: i64,
    uploaded_by: Option<uuid::Uuid>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThemeEntry {
    id: String,
    display_name: String,
    file_name: String,
    size_bytes: i64,
    uploaded_at_utc: String,
    uploaded_by_user_id: Option<String>,
    checksum_sha256: String,
}

impl From<ThemeRow> for ThemeEntry {
    fn from(row: ThemeRow) -> Self {
        Self {
            file_name: format!("{}.json", row.id),
            uploaded_at_utc: chrono::DateTime::from_timestamp_millis(row.uploaded_at)
                .unwrap_or_default()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            uploaded_by_user_id: row
                .uploaded_by
                .map(|u| {
                    u.simple()
                        .to_string()
                }),
            id: row.id,
            display_name: row.display_name,
            size_bytes: row.size_bytes,
            checksum_sha256: row.sha256,
        }
    }
}

const SELECT_ROWS: &str = "SELECT id, display_name, json, size_bytes, sha256, uploaded_at, \
                           uploaded_by FROM moonfin_themes";

async fn all_rows(state: &AppState) -> Result<Vec<ThemeRow>, sqlx::Error> {
    let mut rows: Vec<ThemeRow> = sqlx::query_as(SELECT_ROWS)
        .fetch_all(
            &state
                .ctx
                .db,
        )
        .await?;
    rows.sort_by_cached_key(|r| {
        (
            r.display_name
                .to_lowercase(),
            r.id.clone(),
        )
    });
    Ok(rows)
}

#[get("/moonfin/themes")]
pub async fn moonfin_themes_list(
    State(state): State<AppState>,
    _session: AuthSession,
) -> Response {
    match all_rows(&state).await {
        Ok(rows) => {
            // Raw stored objects, so unknown keys and key order are untouched.
            let mut body = String::from("[");
            for (i, row) in rows
                .iter()
                .enumerate()
            {
                if i > 0 {
                    body.push(',');
                }
                body.push_str(&row.json);
            }
            body.push(']');
            json_text(StatusCode::OK, body)
        }
        Err(e) => internal_error(e),
    }
}

#[get("/moonfin/themes/{theme_id}")]
pub async fn moonfin_themes_get(
    State(state): State<AppState>,
    _session: AuthSession,
    Path(theme_id): Path<String>,
) -> Response {
    let stored: Result<Option<String>, _> =
        sqlx::query_scalar("SELECT json FROM moonfin_themes WHERE id = ?1")
            .bind(theme_id)
            .fetch_optional(
                &state
                    .ctx
                    .db,
            )
            .await;
    match stored {
        Ok(Some(text)) => json_text(StatusCode::OK, text),
        Ok(None) => not_found(),
        Err(e) => internal_error(e),
    }
}

#[get("/moonfin/admin/themes")]
pub async fn moonfin_admin_themes_list(
    State(state): State<AppState>,
    session: AuthSession,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    match all_rows(&state).await {
        Ok(rows) => Json(json!({
            "items": rows
                .into_iter()
                .map(ThemeEntry::from)
                .collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => internal_error(e),
    }
}

/// Tells every open settings stream to re-fetch the theme list.
fn notify_themes_changed(state: &AppState) {
    state
        .ctx
        .moonfin_settings
        .notify_all(StreamEvent::ThemesChanged);
}

#[post("/moonfin/admin/themes")]
pub async fn moonfin_admin_themes_save(
    State(state): State<AppState>,
    session: AuthSession,
    body: Bytes,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    let theme = match Theme::parse(&body) {
        Ok(theme) => theme,
        Err(ThemeRejection::TooLarge) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({
                    "error": "Theme validation failed.",
                    "errors": [format!("Theme JSON exceeds max size ({MAX_BODY_BYTES} bytes).")],
                })),
            )
                .into_response();
        }
        Err(ThemeRejection::Invalid { error, errors }) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error, "errors": errors })),
            )
                .into_response();
        }
    };

    let mut sha = String::with_capacity(64);
    for byte in Sha256::digest(
        theme
            .text
            .as_bytes(),
    ) {
        let _ = write!(sha, "{byte:02x}");
    }
    // A re-upload of identical content changes nothing, so it announces nothing.
    let unchanged = sqlx::query_scalar::<_, String>(
        "SELECT sha256 FROM moonfin_themes WHERE id = ?1",
    )
    .bind(&theme.id)
    .fetch_optional(
        &state
            .ctx
            .db,
    )
    .await
    .ok()
    .flatten()
    .is_some_and(|existing| existing == sha);
    let row: Result<ThemeRow, _> = sqlx::query_as(
        "INSERT INTO moonfin_themes \
         (id, display_name, json, size_bytes, sha256, uploaded_at, uploaded_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT (id) DO UPDATE SET \
         display_name = excluded.display_name, json = excluded.json, \
         size_bytes = excluded.size_bytes, sha256 = excluded.sha256, \
         uploaded_at = excluded.uploaded_at, uploaded_by = excluded.uploaded_by \
         RETURNING id, display_name, json, size_bytes, sha256, uploaded_at, uploaded_by",
    )
    .bind(&theme.id)
    .bind(&theme.display_name)
    .bind(&theme.text)
    .bind(theme.text.len() as i64)
    .bind(&sha)
    .bind(chrono::Utc::now().timestamp_millis())
    .bind(
        session
            .user
            .id,
    )
    .fetch_one(
        &state
            .ctx
            .db,
    )
    .await;
    match row {
        Ok(row) => {
            if !unchanged {
                notify_themes_changed(&state);
            }
            Json(json!({ "success": true, "item": ThemeEntry::from(row) }))
                .into_response()
        }
        Err(e) => internal_error(e),
    }
}

#[delete("/moonfin/admin/themes/{theme_id}")]
pub async fn moonfin_admin_themes_delete(
    State(state): State<AppState>,
    session: AuthSession,
    Path(theme_id): Path<String>,
) -> Response {
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    let result = sqlx::query("DELETE FROM moonfin_themes WHERE id = ?1")
        .bind(theme_id)
        .execute(
            &state
                .ctx
                .db,
        )
        .await;
    match result {
        Ok(done) if done.rows_affected() > 0 => {
            notify_themes_changed(&state);
            Json(json!({ "success": true })).into_response()
        }
        Ok(_) => not_found(),
        Err(e) => internal_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::moonfin_test_support::Harness;

    const COLOR: &str = "#FF112233";

    fn colors(keys: &[&str]) -> Value {
        Value::Object(
            keys.iter()
                .map(|k| (k.to_string(), json!(COLOR)))
                .collect(),
        )
    }

    fn border() -> Value {
        json!({ "color": COLOR, "width": 1 })
    }

    fn shadow(spread: i64) -> Value {
        json!({
            "color": COLOR, "blurRadius": 4, "offsetX": 0, "offsetY": 1, "spreadRadius": spread
        })
    }

    fn theme(id: &str) -> Value {
        let mut book = colors(&BOOK_KEYS);
        book["placeholderPalette"] = json!([COLOR]);
        json!({
            "schemaVersion": 1,
            "id": id,
            "displayName": "Midnight Blue",
            "colors": colors(&COLOR_KEYS),
            "semantic": colors(&SEMANTIC_KEYS),
            "book": book,
            "borders": {
                "cardBorder": border(),
                "chipBorder": border(),
                "focusBorder": border(),
                "cardRadius": 8,
                "chipRadius": 4,
                "chipBackground": COLOR,
                "focusGlow": [shadow(2)],
            },
        })
    }

    async fn list(h: &Harness, token: &str) -> Value {
        let resp = h
            .get(token, "/Moonfin/Themes")
            .await;
        resp.assert_status_ok();
        resp.json::<Value>()
    }

    async fn rejected(h: &Harness, body: Value, status: StatusCode, field: &str) {
        let resp = h
            .post(&h.token, "/Moonfin/Admin/Themes", &body)
            .await;
        resp.assert_status(status);
        let json = resp.json::<Value>();
        assert!(
            json["errors"]
                .to_string()
                .contains(field),
            "expected {field} in {json}"
        );
        assert_eq!(list(h, &h.token).await, json!([]), "nothing was stored");
    }

    #[tokio::test]
    async fn empty_store_lists_nothing_and_unknown_id_is_404() {
        let h = Harness::new().await;
        assert_eq!(list(&h, &h.token).await, json!([]));
        let resp = h
            .get(&h.token, "/Moonfin/Themes/nope")
            .await;
        resp.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(resp.json::<Value>(), json!({ "error": "Theme not found" }));
    }

    #[tokio::test]
    async fn requires_authentication() {
        let h = Harness::new().await;
        h.server
            .get("/Moonfin/Themes")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        h.server
            .get("/Moonfin/Themes/midnight-blue")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        h.server
            .post("/Moonfin/Admin/Themes")
            .json(&theme("midnight-blue"))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_create_appears_in_list_and_get_with_unknown_keys_intact() {
        let h = Harness::new().await;
        let mut sent = theme("midnight-blue");
        sent["futureKey"] = json!({ "kept": [1, 2, 3] });
        let resp = h
            .post(&h.token, "/Moonfin/Admin/Themes", &sent)
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        assert_eq!(body["success"], true);
        assert_eq!(body["item"]["id"], "midnight-blue");
        assert_eq!(body["item"]["displayName"], "Midnight Blue");
        assert_eq!(body["item"]["fileName"], "midnight-blue.json");
        assert_eq!(
            body["item"]["checksumSha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );

        // A non-admin user sees it too.
        let other = h
            .second_user()
            .await;
        for token in [&h.token, &other] {
            assert_eq!(list(&h, token).await, json!([sent]));
            let one = h
                .get(token, "/Moonfin/Themes/midnight-blue")
                .await;
            one.assert_status_ok();
            assert_eq!(one.json::<Value>(), sent);
        }

        // /emby prefix and case-insensitive paths.
        let resp = h
            .get(&h.token, "/emby/moonfin/THEMES")
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<Value>(), json!([sent]));

        let admin = h
            .get(&h.token, "/Moonfin/Admin/Themes")
            .await
            .json::<Value>();
        assert_eq!(admin["items"][0]["id"], "midnight-blue");
    }

    #[tokio::test]
    async fn list_is_ordered_by_display_name_case_insensitively() {
        let h = Harness::new().await;
        for (id, name) in [("t-one", "zebra"), ("t-two", "Apple"), ("t-three", "mango")]
        {
            let mut t = theme(id);
            t["displayName"] = json!(name);
            h.post(&h.token, "/Moonfin/Admin/Themes", &t)
                .await
                .assert_status_ok();
        }
        let names: Vec<_> = list(&h, &h.token)
            .await
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                t["displayName"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(names, ["Apple", "mango", "zebra"]);
    }

    #[tokio::test]
    async fn posting_an_existing_id_replaces_it() {
        let h = Harness::new().await;
        h.post(&h.token, "/Moonfin/Admin/Themes", &theme("midnight-blue"))
            .await
            .assert_status_ok();
        let mut updated = theme("midnight-blue");
        updated["displayName"] = json!("Midnight Blue v2");
        h.post(&h.token, "/Moonfin/Admin/Themes", &updated)
            .await
            .assert_status_ok();
        assert_eq!(list(&h, &h.token).await, json!([updated]));
    }

    #[tokio::test]
    async fn admin_delete_removes_and_unknown_is_404() {
        let h = Harness::new().await;
        h.post(&h.token, "/Moonfin/Admin/Themes", &theme("midnight-blue"))
            .await
            .assert_status_ok();
        h.post(&h.token, "/Moonfin/Admin/Themes", &theme("other-theme"))
            .await
            .assert_status_ok();
        let resp = h
            .delete(&h.token, "/Moonfin/Admin/Themes/midnight-blue")
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<Value>(), json!({ "success": true }));
        assert_eq!(
            list(&h, &h.token)
                .await
                .as_array()
                .unwrap()
                .len(),
            1
        );
        h.get(&h.token, "/Moonfin/Themes/midnight-blue")
            .await
            .assert_status(StatusCode::NOT_FOUND);
        let resp = h
            .delete(&h.token, "/Moonfin/Admin/Themes/midnight-blue")
            .await;
        resp.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(resp.json::<Value>(), json!({ "error": "Theme not found" }));
    }

    #[tokio::test]
    async fn non_admin_cannot_write_or_see_admin_list() {
        let h = Harness::new().await;
        h.post(&h.token, "/Moonfin/Admin/Themes", &theme("midnight-blue"))
            .await
            .assert_status_ok();
        let other = h
            .second_user()
            .await;
        h.post(&other, "/Moonfin/Admin/Themes", &theme("another-one"))
            .await
            .assert_status(StatusCode::FORBIDDEN);
        h.post(&other, "/Moonfin/Admin/Themes", &theme("midnight-blue"))
            .await
            .assert_status(StatusCode::FORBIDDEN);
        h.delete(&other, "/Moonfin/Admin/Themes/midnight-blue")
            .await
            .assert_status(StatusCode::FORBIDDEN);
        h.get(&other, "/Moonfin/Admin/Themes")
            .await
            .assert_status(StatusCode::FORBIDDEN);
        assert_eq!(
            list(&h, &h.token)
                .await
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn the_four_client_builtins_are_rejected() {
        let h = Harness::new().await;
        for id in RESERVED_IDS {
            rejected(
                &h,
                theme(id),
                StatusCode::BAD_REQUEST,
                "protected built-in theme",
            )
            .await;
        }
    }

    #[tokio::test]
    async fn schema_violations_are_400_naming_the_field() {
        let h = Harness::new().await;
        let cases: Vec<(Box<dyn Fn(&mut Value)>, &str)> = vec![
            (
                Box::new(|t| {
                    t["colors"]
                        .as_object_mut()
                        .unwrap()
                        .remove("accent");
                }),
                "colors.accent is required.",
            ),
            (
                Box::new(|t| t["colors"]["accent"] = json!("#12345")),
                "colors.accent must be a valid color",
            ),
            (Box::new(|t| t["id"] = json!("A")), "id must match"),
            (
                Box::new(|t| t["id"] = json!("a".repeat(41))),
                "id must match",
            ),
            (
                Box::new(|t| t["schemaVersion"] = json!(2)),
                "schemaVersion must be 1 or lower.",
            ),
            (
                Box::new(|t| t["displayName"] = json!("x<SCRIPT>")),
                "displayName cannot contain script tags.",
            ),
            (
                Box::new(|t| t["isGlass"] = json!("yes")),
                "isGlass must be true or false.",
            ),
            (
                Box::new(|t| t["colors"]["card"] = json!(5)),
                "colors.card must be a string color",
            ),
            (
                Box::new(|t| t["textGlow"] = json!([shadow(1)])),
                "textGlow[0].spreadRadius must be 0",
            ),
            (
                Box::new(|t| t["book"]["placeholderPalette"] = json!([])),
                "book.placeholderPalette must contain 1 to 16 colors.",
            ),
            (
                Box::new(|t| t["borders"]["focusBorder"]["width"] = json!(17)),
                "borders.focusBorder.width must be between",
            ),
            (
                Box::new(|t| {
                    t["borders"]
                        .as_object_mut()
                        .unwrap()
                        .remove("focusGlow");
                }),
                "borders.focusGlow is required.",
            ),
            (
                Box::new(|t| t["navColorCycle"] = json!(vec![COLOR; 17])),
                "navColorCycle must contain at most 16 colors.",
            ),
        ];
        for (mutate, field) in cases {
            let mut t = theme("midnight-blue");
            mutate(&mut t);
            rejected(&h, t, StatusCode::BAD_REQUEST, field).await;
        }
    }

    #[tokio::test]
    async fn optional_fields_of_the_right_shape_are_accepted() {
        let h = Harness::new().await;
        let mut t = theme("full-theme");
        t["description"] = json!("d");
        t["fontFamily"] = json!("Inter");
        t["isPixel"] = json!(true);
        t["colors"]["card"] = json!("#112233");
        t["semantic"]["statusError"] = json!(COLOR);
        t["textGlow"] = json!([shadow(0)]);
        t["navColorCycle"] = json!([COLOR, COLOR]);
        t["borders"]["navBorder"] = border();
        t["borders"]["cardRadius"] =
            json!({ "topLeft": 1, "topRight": 2, "bottomLeft": 3, "bottomRight": 4 });
        h.post(&h.token, "/Moonfin/Admin/Themes", &t)
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn bad_json_and_non_object_bodies_are_400() {
        let h = Harness::new().await;
        let resp = h
            .server
            .post("/Moonfin/Admin/Themes")
            .add_header(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(
                    &crate::integration_test::auth_header_with_token(&h.token),
                )
                .unwrap(),
            )
            .text("{not json")
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(resp.json::<Value>()["error"], "Invalid JSON body.");
        rejected(
            &h,
            json!([1, 2]),
            StatusCode::BAD_REQUEST,
            "must be a JSON object",
        )
        .await;
    }

    #[tokio::test]
    async fn oversized_body_is_413() {
        let h = Harness::new().await;
        let mut t = theme("midnight-blue");
        t["description"] = json!("x".repeat(MAX_BODY_BYTES));
        rejected(&h, t, StatusCode::PAYLOAD_TOO_LARGE, "exceeds max size").await;
    }

    #[tokio::test]
    async fn every_parser_key_is_served() {
        let h = Harness::new().await;
        h.post(&h.token, "/Moonfin/Admin/Themes", &theme("midnight-blue"))
            .await
            .assert_status_ok();
        let served = h
            .get(&h.token, "/Moonfin/Themes")
            .await
            .json::<Value>();
        let t = &served[0];
        for k in COLOR_KEYS {
            assert!(t["colors"][k].is_string(), "colors.{k}");
        }
        for k in SEMANTIC_KEYS {
            assert!(t["semantic"][k].is_string(), "semantic.{k}");
        }
        for k in BOOK_KEYS {
            assert!(t["book"][k].is_string(), "book.{k}");
        }
        assert!(t["book"]["placeholderPalette"].is_array());
        for k in ["cardBorder", "chipBorder", "focusBorder"] {
            assert!(t["borders"][k]["width"].is_number(), "borders.{k}");
        }
    }

    #[tokio::test]
    async fn themes_survive_a_cold_read_from_the_database() {
        // The store is SQLite, not memory: a fresh pool-level read sees it.
        let h = Harness::new().await;
        h.post(&h.token, "/Moonfin/Admin/Themes", &theme("midnight-blue"))
            .await
            .assert_status_ok();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moonfin_themes")
            .fetch_one(
                &h.ctx
                    .db,
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    // ------------------------------------------------------------------
    // Live updates over the settings stream
    // ------------------------------------------------------------------

    use crate::api::moonfin_test_support::SseConn;
    use std::time::Duration;

    const CHANGED: &str = r#"data: {"type":"themesChanged"}"#;
    const QUIET: Duration = Duration::from_millis(600);

    /// Two users with open, settled streams.
    async fn two_streams(h: &Harness, other: &str) -> (SseConn, SseConn) {
        let mut admin = SseConn::open(&h.app, &h.token).await;
        let mut user = SseConn::open(&h.app, other).await;
        admin
            .wait_for(":connected")
            .await;
        user.wait_for(":connected")
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        (admin, user)
    }

    async fn assert_quiet(streams: &mut [&mut SseConn]) {
        for sse in streams {
            assert!(
                !sse.appears_within(CHANGED, QUIET)
                    .await,
                "unexpected themesChanged: {:?}",
                sse.seen
            );
        }
    }

    #[tokio::test]
    async fn create_and_delete_push_themes_changed_to_every_stream() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let (mut admin, mut user) = two_streams(&h, &other).await;

        h.post(&h.token, "/Moonfin/Admin/Themes", &theme("midnight-blue"))
            .await
            .assert_status_ok();
        admin
            .wait_for(CHANGED)
            .await;
        user.wait_for(CHANGED)
            .await;

        h.delete(&h.token, "/Moonfin/Admin/Themes/midnight-blue")
            .await
            .assert_status_ok();
        for sse in [&mut admin, &mut user] {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while sse
                .seen
                .matches(CHANGED)
                .count()
                < 2
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no second event: {:?}",
                    sse.seen
                );
                sse.appears_within("never-appears", Duration::from_millis(100))
                    .await;
            }
        }
    }

    #[tokio::test]
    async fn replacing_pushes_but_identical_content_does_not() {
        let h = Harness::new().await;
        h.post(&h.token, "/Moonfin/Admin/Themes", &theme("midnight-blue"))
            .await
            .assert_status_ok();
        let other = h
            .second_user()
            .await;
        let (mut admin, mut user) = two_streams(&h, &other).await;

        h.post(&h.token, "/Moonfin/Admin/Themes", &theme("midnight-blue"))
            .await
            .assert_status_ok();
        assert_quiet(&mut [&mut admin, &mut user]).await;

        let mut updated = theme("midnight-blue");
        updated["displayName"] = json!("Midnight Blue v2");
        h.post(&h.token, "/Moonfin/Admin/Themes", &updated)
            .await
            .assert_status_ok();
        admin
            .wait_for(CHANGED)
            .await;
        user.wait_for(CHANGED)
            .await;
    }

    #[tokio::test]
    async fn failures_do_not_push() {
        let h = Harness::new().await;
        h.post(&h.token, "/Moonfin/Admin/Themes", &theme("midnight-blue"))
            .await
            .assert_status_ok();
        let other = h
            .second_user()
            .await;
        let (mut admin, mut user) = two_streams(&h, &other).await;

        let mut bad = theme("bad-one");
        bad["colors"] = json!("not an object");
        h.post(&h.token, "/Moonfin/Admin/Themes", &bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        h.post(&other, "/Moonfin/Admin/Themes", &theme("sneaky"))
            .await
            .assert_status(StatusCode::FORBIDDEN);
        h.delete(&other, "/Moonfin/Admin/Themes/midnight-blue")
            .await
            .assert_status(StatusCode::FORBIDDEN);
        h.delete(&h.token, "/Moonfin/Admin/Themes/no-such-theme")
            .await
            .assert_status(StatusCode::NOT_FOUND);
        assert_quiet(&mut [&mut admin, &mut user]).await;
    }

    #[tokio::test]
    async fn settings_updated_still_reaches_the_stream() {
        let h = Harness::new().await;
        let mut admin = SseConn::open(&h.app, &h.token).await;
        admin
            .wait_for(":connected")
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.post(
            &h.token,
            "/Moonfin/Settings/Profile/tv",
            &json!({ "profile": { "posterSize": "small" }, "clientId": "t" }),
        )
        .await
        .assert_status_ok();
        admin
            .wait_for(r#"data: {"type":"settingsUpdated"}"#)
            .await;
    }
}
