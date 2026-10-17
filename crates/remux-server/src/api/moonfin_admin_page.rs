//! The standalone Moonfin admin page (`/Moonfin/Admin`).
//!
//! A small static shell (HTML + one script + one stylesheet, all embedded in
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
const PAGE_CSS: &str = include_str!("../../assets/moonfin/admin.css");

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

#[get("/moonfin/admin/admin.css")]
pub async fn moonfin_admin_style() -> Response {
    with_headers("text/css; charset=utf-8", PAGE_CSS, false)
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
        let css = h
            .server
            .get("/Moonfin/Admin/admin.css")
            .await;
        css.assert_status_ok();
        assert_eq!(hdr(&css, "content-type"), "text/css; charset=utf-8");
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
        for (name, text) in [
            ("html", html),
            ("js", super::PAGE_JS),
            ("css", super::PAGE_CSS),
        ] {
            assert!(!text.contains("http://"), "{name} references http://");
            assert!(!text.contains("https://"), "{name} references https://");
            assert!(!text.contains("//cdn"), "{name} references a CDN");
        }
        assert!(
            !super::PAGE_JS.contains("innerHTML")
                && !super::PAGE_JS.contains("insertAdjacentHTML")
                && !super::PAGE_JS.contains("document.write"),
            "script must build the DOM with textContent only"
        );
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
