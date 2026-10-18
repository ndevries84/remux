//! The standalone Moonfin admin page (`/Moonfin/Admin`).
//!
//! A small static shell (HTML + two scripts + one stylesheet, all embedded in
//! the binary) that manages only the Moonfin-specific things Remux hosts:
//! default client settings, per-user settings, themes, cores and a status
//! overview. Everything else stays in the Remux dashboard (`/admin`).
//!
//! The page itself is served anonymously; every piece of data comes from the
//! admin-only JSON APIs (`moonfin_admin`, `moonfin_themes`, `moonfin_cores`),
//! which the script calls with an admin access token. The URI rewrite layer
//! lowercases `/Moonfin/Admin` and trims a trailing slash before routing.

use axum::{
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use remux_macros::get;

const PAGE_HTML: &str = include_str!("../../assets/moonfin/admin.html");
const PAGE_JS: &str = include_str!("../../assets/moonfin/admin.js");
const PAGE_DEFAULTS_JS: &str = include_str!("../../assets/moonfin/admin-defaults.js");
const PAGE_SERVER_SETTINGS_JS: &str =
    include_str!("../../assets/moonfin/admin-server-settings.js");
const PAGE_HOME_ROWS_JS: &str = include_str!("../../assets/moonfin/admin-home-rows.js");
const PAGE_CSS: &str = include_str!("../../assets/moonfin/admin.css");
/// The option schema behind the default settings forms. Derived from Moonbase
/// (see NOTICE); data only.
const DEFAULT_OPTIONS_JSON: &str =
    include_str!("../../assets/moonfin/default-options.json");

/// Scripts and styles are same-origin files, so no nonce or `unsafe-inline`
/// is needed. The page only talks to its own origin.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; \
base-uri 'none'; form-action 'none'";

fn with_headers(content_type: &'static str, body: &'static str, csp: bool) -> Response {
    let mut resp = (
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        body,
    )
        .into_response();
    let h = resp.headers_mut();
    if csp {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        );
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[get("/moonfin/admin")]
pub async fn moonfin_admin_page() -> Response {
    with_headers("text/html; charset=utf-8", PAGE_HTML, true)
}

#[get("/moonfin/admin/admin.js")]
pub async fn moonfin_admin_script() -> Response {
    with_headers("text/javascript; charset=utf-8", PAGE_JS, false)
}

#[get("/moonfin/admin/admin-defaults.js")]
pub async fn moonfin_admin_defaults_script() -> Response {
    with_headers("text/javascript; charset=utf-8", PAGE_DEFAULTS_JS, false)
}

#[get("/moonfin/admin/admin-home-rows.js")]
pub async fn moonfin_admin_home_rows_script() -> Response {
    with_headers("text/javascript; charset=utf-8", PAGE_HOME_ROWS_JS, false)
}

#[get("/moonfin/admin/admin-server-settings.js")]
pub async fn moonfin_admin_server_settings_script() -> Response {
    with_headers(
        "text/javascript; charset=utf-8",
        PAGE_SERVER_SETTINGS_JS,
        false,
    )
}

#[get("/moonfin/admin/admin.css")]
pub async fn moonfin_admin_style() -> Response {
    with_headers("text/css; charset=utf-8", PAGE_CSS, false)
}

#[get("/moonfin/admin/default-options.json")]
pub async fn moonfin_admin_default_options() -> Response {
    with_headers(
        "application/json; charset=utf-8",
        DEFAULT_OPTIONS_JSON,
        false,
    )
}

#[cfg(test)]
mod tests {
    use crate::api::moonfin_test_support::Harness;

    fn hdr(resp: &axum_test::TestResponse, name: &str) -> String {
        resp.headers()
            .get(name)
            .map(|v| {
                v.to_str()
                    .unwrap()
                    .to_string()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn page_is_served_anonymously_with_strict_headers() {
        let h = Harness::new().await;
        for path in ["/Moonfin/Admin", "/Moonfin/Admin/", "/moonfin/admin"] {
            let resp = h
                .server
                .get(path)
                .await;
            resp.assert_status_ok();
            assert_eq!(hdr(&resp, "content-type"), "text/html; charset=utf-8");
            let csp = hdr(&resp, "content-security-policy");
            for want in [
                "default-src 'none'",
                "script-src 'self'",
                "style-src 'self'",
                "connect-src 'self'",
                "img-src 'self' data:",
                "frame-ancestors 'none'",
                "base-uri 'none'",
                "form-action 'none'",
            ] {
                assert!(csp.contains(want), "{path}: CSP lacks {want}: {csp}");
            }
            assert!(!csp.contains("unsafe-inline"));
            assert_eq!(hdr(&resp, "x-content-type-options"), "nosniff");
            assert_eq!(hdr(&resp, "referrer-policy"), "no-referrer");
            assert_eq!(hdr(&resp, "cache-control"), "no-store");
            assert!(
                resp.text()
                    .contains("Moonfin")
            );
        }
    }

    #[tokio::test]
    async fn assets_are_served_with_their_types() {
        let h = Harness::new().await;
        let js = h
            .server
            .get("/Moonfin/Admin/admin.js")
            .await;
        js.assert_status_ok();
        assert_eq!(hdr(&js, "content-type"), "text/javascript; charset=utf-8");
        assert_eq!(hdr(&js, "x-content-type-options"), "nosniff");
        let defaults_js = h
            .server
            .get("/Moonfin/Admin/admin-defaults.js")
            .await;
        defaults_js.assert_status_ok();
        assert_eq!(
            hdr(&defaults_js, "content-type"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(hdr(&defaults_js, "x-content-type-options"), "nosniff");
        let home_rows_js = h
            .server
            .get("/Moonfin/Admin/admin-home-rows.js")
            .await;
        home_rows_js.assert_status_ok();
        assert_eq!(
            hdr(&home_rows_js, "content-type"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(hdr(&home_rows_js, "x-content-type-options"), "nosniff");
        assert!(
            home_rows_js
                .text()
                .contains("MoonfinHomeRows")
        );
        let server_settings_js = h
            .server
            .get("/Moonfin/Admin/admin-server-settings.js")
            .await;
        server_settings_js.assert_status_ok();
        assert_eq!(
            hdr(&server_settings_js, "content-type"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            hdr(&server_settings_js, "x-content-type-options"),
            "nosniff"
        );
        assert!(
            server_settings_js
                .text()
                .contains("MoonfinAdminServerSettings")
        );
        let css = h
            .server
            .get("/Moonfin/Admin/admin.css")
            .await;
        css.assert_status_ok();
        assert_eq!(hdr(&css, "content-type"), "text/css; charset=utf-8");
    }

    #[tokio::test]
    async fn default_options_are_served_as_json_with_strict_headers() {
        let h = Harness::new().await;
        let resp = h
            .server
            .get("/Moonfin/Admin/default-options.json")
            .await;
        resp.assert_status_ok();
        assert_eq!(
            hdr(&resp, "content-type"),
            "application/json; charset=utf-8"
        );
        assert_eq!(hdr(&resp, "x-content-type-options"), "nosniff");
        assert_eq!(hdr(&resp, "cache-control"), "no-store");
        let body: serde_json::Value = resp.json();
        assert!(body["controls"].is_array() && body["excluded"].is_array());
    }

    /// True when `html` has an attribute named `on<letters>` on any tag.
    fn has_inline_handler(html: &str) -> bool {
        let lower = html.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let mut in_tag = false;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'<' => in_tag = true,
                b'>' => in_tag = false,
                b'o' if in_tag
                    && i + 2 < bytes.len()
                    && bytes[i + 1] == b'n'
                    && bytes[i - 1].is_ascii_whitespace() =>
                {
                    let rest = &lower[i + 2..];
                    let name_len = rest
                        .find(|c: char| !c.is_ascii_lowercase())
                        .unwrap_or(rest.len());
                    if name_len > 0
                        && rest[name_len..]
                            .trim_start()
                            .starts_with('=')
                    {
                        return true;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        false
    }

    #[test]
    fn handler_detector_detects() {
        assert!(has_inline_handler("<a onclick=\"x()\">"));
        assert!(has_inline_handler("<img src=x onerror = y>"));
        assert!(!has_inline_handler("<p>one = two, onion = 3</p>"));
        assert!(!has_inline_handler("<input name=\"opinion\">"));
    }

    #[test]
    fn page_has_no_inline_handlers_inline_code_or_external_urls() {
        let html = super::PAGE_HTML;
        assert!(!has_inline_handler(html), "inline event handler in HTML");
        assert!(!html.contains("style="), "inline style attribute in HTML");
        assert!(
            !html
                .to_ascii_lowercase()
                .contains("<script>"),
            "inline script in HTML"
        );
        // The one external link the page may carry: the Moonfin theme editor.
        // It is a plain navigation link (opens in a new tab with no opener or
        // referrer); the page itself still loads nothing from other origins.
        const THEME_EDITOR: &str = "https://moonfin.io/theme-editor";
        assert_eq!(
            html.matches("https://")
                .count(),
            1,
            "html may contain only the theme editor URL"
        );
        assert!(
            html.contains(&format!(
                "<a href=\"{THEME_EDITOR}\" target=\"_blank\" rel=\"noopener noreferrer\">"
            )),
            "the theme editor link must open in a new tab with rel=noopener noreferrer"
        );
        let html_without_link = html.replace(THEME_EDITOR, "");
        for (name, text) in [
            ("html", html_without_link.as_str()),
            ("js", super::PAGE_JS),
            ("defaults-js", super::PAGE_DEFAULTS_JS),
            ("home-rows-js", super::PAGE_HOME_ROWS_JS),
            ("server-settings-js", super::PAGE_SERVER_SETTINGS_JS),
            ("css", super::PAGE_CSS),
        ] {
            assert!(!text.contains("http://"), "{name} references http://");
            assert!(!text.contains("https://"), "{name} references https://");
            assert!(!text.contains("//cdn"), "{name} references a CDN");
        }
        for js in [
            super::PAGE_JS,
            super::PAGE_DEFAULTS_JS,
            super::PAGE_HOME_ROWS_JS,
            super::PAGE_SERVER_SETTINGS_JS,
        ] {
            assert!(
                !js.contains("innerHTML")
                    && !js.contains("insertAdjacentHTML")
                    && !js.contains("document.write")
                    && !js.contains("outerHTML"),
                "scripts must build the DOM with textContent only"
            );
        }
    }

    mod options {
        use serde::Deserialize;
        use std::collections::HashSet;

        use crate::api::moonfin_settings::ADMIN_BLOCKED_KEYS;

        #[derive(Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Kind {
            Bool,
            Enum,
            Number,
            String,
            Color,
            List,
            Json,
        }

        #[derive(Deserialize)]
        struct Choice {
            value: serde_json::Value,
            label: String,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Control {
            key: String,
            label: String,
            group: String,
            section: Option<String>,
            #[serde(rename = "type")]
            kind: Kind,
            help: Option<String>,
            default: Option<serde_json::Value>,
            options: Option<Vec<Choice>>,
            suggestions: Option<Vec<Choice>>,
            min: Option<f64>,
            max: Option<f64>,
            step: Option<f64>,
            max_items: Option<u32>,
            store_as: Option<String>,
            also_keys: Option<Vec<String>>,
        }

        #[derive(Deserialize)]
        struct Excluded {
            key: String,
            reason: String,
            group: String,
            label: String,
        }

        #[derive(Deserialize)]
        struct Schema {
            groups: Vec<String>,
            controls: Vec<Control>,
            excluded: Vec<Excluded>,
        }

        fn schema() -> Schema {
            serde_json::from_str(super::super::DEFAULT_OPTIONS_JSON)
                .expect("default-options.json parses against the schema")
        }

        #[test]
        fn keys_are_unique_and_never_blocked() {
            let s = schema();
            assert!(
                s.controls
                    .len()
                    > 100
            );
            let mut seen = HashSet::new();
            for c in &s.controls {
                let keys = std::iter::once(&c.key).chain(
                    c.also_keys
                        .iter()
                        .flatten(),
                );
                for k in keys {
                    assert!(seen.insert(k.clone()), "duplicate key {k}");
                    assert!(
                        !ADMIN_BLOCKED_KEYS.contains(&k.as_str()),
                        "blocked key {k} is offered as a control"
                    );
                }
            }
            for e in &s.excluded {
                assert!(
                    !seen.contains(&e.key),
                    "{} is both included and excluded",
                    e.key
                );
                assert!(
                    !e.reason
                        .is_empty()
                        && !e
                            .group
                            .is_empty()
                        && !e
                            .label
                            .is_empty()
                );
            }
            let ex: HashSet<&str> = s
                .excluded
                .iter()
                .map(|e| {
                    e.key
                        .as_str()
                })
                .collect();
            assert_eq!(
                ex.len(),
                s.excluded
                    .len(),
                "duplicate excluded key"
            );
            for k in ADMIN_BLOCKED_KEYS {
                assert!(ex.contains(k), "blocked key {k} must be listed as excluded");
            }
        }

        #[test]
        fn groups_and_types_are_consistent() {
            let s = schema();
            for c in &s.controls {
                assert!(
                    s.groups
                        .contains(&c.group),
                    "{}: unknown group {}",
                    c.key,
                    c.group
                );
                assert!(
                    !c.label
                        .is_empty(),
                    "{}: empty label",
                    c.key
                );
                let _ = (&c.section, &c.help);
                match c.kind {
                    Kind::Enum => {
                        let opts = c
                            .options
                            .as_ref()
                            .unwrap_or_else(|| {
                                panic!("{}: enum without options", c.key)
                            });
                        assert!(!opts.is_empty());
                        let mut vals = HashSet::new();
                        for o in opts {
                            assert!(
                                !o.label
                                    .is_empty()
                            );
                            assert!(
                                vals.insert(
                                    o.value
                                        .to_string()
                                ),
                                "{}: duplicate option",
                                c.key
                            );
                        }
                        if let Some(d) = &c.default {
                            assert!(
                                opts.iter()
                                    .any(|o| &o.value == d),
                                "{}: default is not one of the options",
                                c.key
                            );
                        }
                    }
                    Kind::Number => {
                        let (min, max) = (
                            c.min
                                .unwrap(),
                            c.max
                                .unwrap(),
                        );
                        assert!(min <= max, "{}: min > max", c.key);
                        assert!(
                            c.step
                                .unwrap_or(1.0)
                                > 0.0
                        );
                        if let Some(d) = c
                            .default
                            .as_ref()
                            .and_then(|d| d.as_f64())
                        {
                            assert!(
                                (min..=max).contains(&d),
                                "{}: default out of range",
                                c.key
                            );
                        }
                        if let Some(t) = &c.store_as {
                            assert_eq!(t, "string");
                        }
                    }
                    Kind::List => {
                        for o in c
                            .suggestions
                            .iter()
                            .flatten()
                        {
                            assert!(
                                o.value
                                    .is_string(),
                                "{}: suggestion not a string",
                                c.key
                            );
                        }
                        assert!(
                            c.max_items
                                .is_none_or(|n| n > 0)
                        );
                    }
                    Kind::Bool | Kind::String | Kind::Color | Kind::Json => {}
                }
                if !matches!(c.kind, Kind::Enum) {
                    assert!(
                        c.options
                            .is_none(),
                        "{}: options only on enums",
                        c.key
                    );
                }
            }
        }

        #[test]
        fn home_layout_pair_is_present() {
            let s = schema();
            let has = |k: &str| {
                s.controls
                    .iter()
                    .any(|c| c.key == k)
            };
            assert!(has("homeSections") && has("homeRowOrder"));
        }
    }

    #[tokio::test]
    async fn admin_apis_still_need_an_admin() {
        let h = Harness::new().await;
        h.server
            .get("/Moonfin/Admin/Status")
            .await
            .assert_status_unauthorized();
        let other = h
            .second_user()
            .await;
        h.server
            .get("/Moonfin/Admin/Status")
            .add_header(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(
                    &crate::integration_test::auth_header_with_token(&other),
                )
                .unwrap(),
            )
            .await
            .assert_status_forbidden();
    }
}
