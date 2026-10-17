//! Hosted Moonfin web app (`/Moonfin/Web/...`).
//!
//! Mirrors Moonbase's `MoonfinWebController`: the Flutter web bundle is served
//! anonymously from an admin-provided directory (`Config::moonfin_web_path`,
//! default `<data_dir>/moonfin-web`), plus a dynamic `config.json` that puts
//! the app in "plugin mode" so it auto-connects to this server. The bundle is
//! GPL-licensed and never shipped with Remux.
//!
//! The bundle's base href is baked in as `/Moonfin/Web/`, so it must be reached
//! at exactly that path. The URI rewrite layer lowercases purely alphanumeric
//! `/moonfin` segments (so the extensionless `assets/NOTICES` arrives as
//! `notices`) and trims trailing slashes, hence lookups here fall back to an
//! ASCII case-insensitive directory match, and directory hits redirect to their
//! `index.html`.

use std::path::{Component, Path as FsPath, PathBuf};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use remux_macros::get;
use serde_json::json;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::moonfin_emulator::contained_path;
use crate::AppState;

const MOUNT: &str = "/moonfin/web";

pub(crate) fn web_root(state: &AppState) -> PathBuf {
    match state
        .ctx
        .config
        .moonfin_web_path
        .as_deref()
        .filter(|p| {
            !p.trim()
                .is_empty()
        }) {
        Some(p) => PathBuf::from(p),
        None => state
            .ctx
            .config
            .data_dir
            .join("moonfin-web"),
    }
}

fn content_type_for(path: &FsPath) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("txt" | "frag") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        _ => "application/octet-stream",
    }
}

/// `Cache-Control` for a bundle file (path relative to the root, as found on
/// disk). The bundle's file names are not content-hashed, so anything that
/// changes between releases is revalidated through its ETag; large, static
/// resources get a day.
fn cache_control_for(rel: &str) -> Option<&'static str> {
    let lower = rel.to_ascii_lowercase();
    let name = lower
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if name == "index.html"
        || name == "version.json"
        || name == "flutter_bootstrap.js"
        || name == "flutter.js"
        || name == "drift_worker.js"
        || name.starts_with("main.dart.")
    {
        return Some("no-cache");
    }
    if lower.starts_with("canvaskit/")
        || [".otf", ".ttf", ".woff", ".woff2"]
            .iter()
            .any(|e| name.ends_with(e))
    {
        return Some("public, max-age=86400");
    }
    None
}

/// Resolves `requested` under `root` with an ASCII case-insensitive fallback
/// per component (exact matches win). Returns `None` for traversal attempts, a
/// miss, or a result that escapes `root` once symlinks are resolved.
async fn resolve_in_root(root: &FsPath, requested: &str) -> Option<PathBuf> {
    let joined = contained_path(root, requested)?;
    let rel = joined
        .strip_prefix(root)
        .ok()?
        .to_path_buf();
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        let Component::Normal(name) = comp else {
            return None;
        };
        let exact = cur.join(name);
        if tokio::fs::symlink_metadata(&exact)
            .await
            .is_ok()
        {
            cur = exact;
            continue;
        }
        let wanted = name.to_str()?;
        let mut dir = tokio::fs::read_dir(&cur)
            .await
            .ok()?;
        let mut found = None;
        while let Ok(Some(entry)) = dir
            .next_entry()
            .await
        {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(wanted))
            {
                found = Some(entry.path());
                break;
            }
        }
        cur = found?;
    }
    let canon_root = tokio::fs::canonicalize(root)
        .await
        .ok()?;
    let canon = tokio::fs::canonicalize(&cur)
        .await
        .ok()?;
    canon
        .starts_with(&canon_root)
        .then_some(cur)
}

fn not_found_hint() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        json!({
            "error": "Moonfin web bundle not found",
            "hint": "Extract the Moonfin web bundle into <data_dir>/moonfin-web \
                     or set moonfin_web_path"
        })
        .to_string(),
    )
        .into_response()
}

async fn serve_file(root: &FsPath, full: PathBuf, req: Request<Body>) -> Response {
    let rel = full
        .strip_prefix(root)
        .map(|p| {
            p.to_string_lossy()
                .replace('\\', "/")
        })
        .unwrap_or_default();
    let ct = content_type_for(&full);
    let mut resp = match ServeFile::new(full)
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
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
        if let Some(cc) = cache_control_for(&rel) {
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cc));
        }
    }
    resp
}

async fn serve_path(state: &AppState, requested: &str, req: Request<Body>) -> Response {
    let root = web_root(state);
    if !tokio::fs::metadata(&root)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        tracing::warn!(
            path = %root.display(),
            "Moonfin web bundle directory is missing; /Moonfin/Web will answer 404"
        );
        return not_found_hint();
    }
    let trimmed = requested.trim_matches('/');
    let target = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    match resolve_in_root(&root, target).await {
        Some(full) => {
            let meta = tokio::fs::metadata(&full)
                .await
                .ok();
            if meta
                .as_ref()
                .is_some_and(|m| m.is_dir())
            {
                // The rewrite layer strips trailing slashes, so relative URLs in
                // a directory's index.html would resolve one level too high.
                if full
                    .join("index.html")
                    .is_file()
                {
                    let rel = full
                        .strip_prefix(&root)
                        .map(|p| {
                            p.to_string_lossy()
                                .replace('\\', "/")
                        })
                        .unwrap_or_default();
                    let location = format!("{MOUNT}/{rel}/index.html");
                    return (
                        StatusCode::PERMANENT_REDIRECT,
                        [(header::LOCATION, location)],
                    )
                        .into_response();
                }
                return StatusCode::NOT_FOUND.into_response();
            }
            if meta.is_some_and(|m| m.is_file()) {
                return serve_file(&root, full, req).await;
            }
            StatusCode::NOT_FOUND.into_response()
        }
        None => {
            // Missing path with an extension is a real 404; without one it is a
            // client-side (SPA) route. Traversal attempts never fall back.
            let last = target
                .rsplit('/')
                .next()
                .unwrap_or_default();
            if last.contains('.') || contained_path(&root, target).is_none() {
                return StatusCode::NOT_FOUND.into_response();
            }
            match resolve_in_root(&root, "index.html").await {
                Some(index) => serve_file(&root, index, req).await,
                None => not_found_hint(),
            }
        }
    }
}

/// Strips a trailing slash, then a trailing `/web/index.html` or `/web`.
fn normalize_forced_url(url: &str) -> String {
    let mut s = url
        .trim()
        .trim_end_matches('/')
        .to_string();
    for suffix in ["/web/index.html", "/web"] {
        if s.to_ascii_lowercase()
            .ends_with(suffix)
        {
            s.truncate(s.len() - suffix.len());
            break;
        }
    }
    s.trim_end_matches('/')
        .to_string()
}

/// Request origin from `Host`; the scheme comes from `X-Forwarded-Proto` when a
/// reverse proxy sets it. Only used for the Add Server pre-fill, since the app
/// auto-connects to its own origin in plugin mode.
fn request_origin(headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get(header::HOST)?
        .to_str()
        .ok()?;
    let scheme = match headers
        .get("x-forwarded-proto")
        .and_then(|v| {
            v.to_str()
                .ok()
        })
        .map(|v| {
            v.split(',')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
        .as_deref()
    {
        Some("https") => "https",
        _ => "http",
    };
    Some(format!("{scheme}://{host}"))
}

#[get("/moonfin/web/config.json")]
pub async fn web_config(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let cfg = &state
        .ctx
        .config;
    let default_url = cfg
        .moonfin_web_default_server_url
        .as_deref()
        .map(|u| {
            u.trim()
                .trim_end_matches('/')
                .to_string()
        })
        .filter(|u| !u.is_empty())
        .or_else(|| request_origin(&headers));
    let forced = cfg
        .moonfin_web_forced_server_url
        .as_deref()
        .map(normalize_forced_url)
        .filter(|u| !u.is_empty());
    let body = json!({
        "schemaVersion": 1,
        "defaultServerUrl": default_url,
        "discoveryProxyUrl": null,
        "enableWebRtcScan": cfg.moonfin_web_enable_webrtc_scan,
        "brandingName": "Moonfin",
        "pluginMode": true,
        "forcedServerUrl": forced,
    });
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
            (header::PRAGMA, "no-cache"),
            (header::EXPIRES, "0"),
        ],
        body.to_string(),
    )
        .into_response()
}

#[get("/moonfin/web")]
pub async fn web_root_index(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response {
    serve_path(&state, "", req).await
}

#[get("/moonfin/web/{*path}")]
pub async fn web_asset(
    State(state): State<AppState>,
    Path(path): Path<String>,
    req: Request<Body>,
) -> Response {
    serve_path(&state, &path, req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Config, init_app_with_ctx, integration_test::TestGuard, rewrite_request_uri,
    };
    use axum::Router;
    use axum_test::{TestResponse, TestServer};
    use tower::Layer;
    use tower::util::MapRequestLayer;

    async fn server(
        data: &tempfile::TempDir,
        tweak: impl FnOnce(&mut Config),
    ) -> (TestServer, TestGuard) {
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
        (server, TestGuard(ctx))
    }

    fn hdr(resp: &TestResponse, name: &str) -> Option<String> {
        resp.headers()
            .get(name)
            .map(|v| {
                v.to_str()
                    .unwrap()
                    .to_string()
            })
    }

    /// Writes a tiny synthetic bundle under `<data>/moonfin-web`.
    fn write_bundle(data: &tempfile::TempDir) -> PathBuf {
        let root = data
            .path()
            .join("moonfin-web");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::create_dir_all(root.join("theme/css")).unwrap();
        std::fs::create_dir_all(root.join("canvaskit")).unwrap();
        std::fs::write(
            root.join("index.html"),
            "<html><head><base href=\"/Moonfin/Web/\"></head><body>moonfin</body></html>",
        )
        .unwrap();
        std::fs::write(root.join("config.json"), "{\"static\":true}").unwrap();
        std::fs::write(root.join("main.dart.wasm"), b"\0asm\x01\0\0\0").unwrap();
        std::fs::write(root.join("main.dart.mjs"), "export default 1;").unwrap();
        std::fs::write(root.join("assets/NOTICES"), "licences text").unwrap();
        std::fs::write(root.join("theme/index.html"), "theme page").unwrap();
        std::fs::write(root.join("theme/css/styles.css"), "body{}").unwrap();
        std::fs::write(root.join("canvaskit/canvaskit.wasm"), b"\0asm").unwrap();
        root
    }

    #[tokio::test]
    async fn index_is_served_anonymously() {
        let data = tempfile::tempdir().unwrap();
        write_bundle(&data);
        let (server, _g) = server(&data, |_| {}).await;
        for path in ["/Moonfin/Web/", "/Moonfin/Web", "/moonfin/web/index.html"] {
            let resp = server
                .get(path)
                .await;
            resp.assert_status_ok();
            assert!(
                hdr(&resp, "content-type")
                    .unwrap()
                    .starts_with("text/html"),
                "{path}"
            );
            assert!(
                resp.text()
                    .contains("<base href=\"/Moonfin/Web/\">")
            );
            assert_eq!(hdr(&resp, "cache-control").as_deref(), Some("no-cache"));
            assert!(hdr(&resp, "content-security-policy").is_none());
            assert!(hdr(&resp, "cross-origin-opener-policy").is_none());
        }
    }

    #[tokio::test]
    async fn config_json_is_dynamic_and_ignores_bundle_copy() {
        let data = tempfile::tempdir().unwrap();
        write_bundle(&data);
        let (server, _g) = server(&data, |_| {}).await;
        let resp = server
            .get("/Moonfin/Web/config.json")
            .add_header("host", "media.example:8096")
            .await;
        resp.assert_status_ok();
        assert_eq!(
            hdr(&resp, "cache-control").as_deref(),
            Some("no-store, no-cache, must-revalidate")
        );
        assert_eq!(
            resp.json::<serde_json::Value>(),
            json!({
                "schemaVersion": 1,
                "defaultServerUrl": "http://media.example:8096",
                "discoveryProxyUrl": null,
                "enableWebRtcScan": false,
                "brandingName": "Moonfin",
                "pluginMode": true,
                "forcedServerUrl": null,
            })
        );
        // Behind a TLS-terminating proxy.
        let resp = server
            .get("/moonfin/web/config.json")
            .add_header("host", "media.example")
            .add_header("x-forwarded-proto", "https")
            .await;
        assert_eq!(
            resp.json::<serde_json::Value>()["defaultServerUrl"],
            "https://media.example"
        );
    }

    #[tokio::test]
    async fn config_json_honours_configured_urls() {
        let data = tempfile::tempdir().unwrap();
        write_bundle(&data);
        let (server, _g) = server(&data, |c| {
            c.moonfin_web_default_server_url = Some("https://home.example/".into());
            c.moonfin_web_forced_server_url = Some("https://remux.example/web/".into());
            c.moonfin_web_enable_webrtc_scan = true;
        })
        .await;
        let v = server
            .get("/Moonfin/Web/config.json")
            .await
            .json::<serde_json::Value>();
        assert_eq!(v["defaultServerUrl"], "https://home.example");
        assert_eq!(v["forcedServerUrl"], "https://remux.example");
        assert_eq!(v["enableWebRtcScan"], true);
        assert_eq!(v["pluginMode"], true);
    }

    #[test]
    fn forced_url_normalisation() {
        assert_eq!(
            normalize_forced_url("http://a:1/web/index.html"),
            "http://a:1"
        );
        assert_eq!(normalize_forced_url("http://a:1/Web"), "http://a:1");
        assert_eq!(normalize_forced_url("http://a:1/"), "http://a:1");
        assert_eq!(normalize_forced_url("http://a:1/sub"), "http://a:1/sub");
    }

    #[tokio::test]
    async fn wasm_and_module_mime_types() {
        let data = tempfile::tempdir().unwrap();
        write_bundle(&data);
        let (server, _g) = server(&data, |_| {}).await;
        let resp = server
            .get("/Moonfin/Web/main.dart.wasm")
            .await;
        resp.assert_status_ok();
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/wasm")
        );
        assert_eq!(hdr(&resp, "cache-control").as_deref(), Some("no-cache"));
        let resp = server
            .get("/Moonfin/Web/main.dart.mjs")
            .await;
        assert!(
            hdr(&resp, "content-type")
                .unwrap()
                .starts_with("text/javascript")
        );
        let resp = server
            .get("/Moonfin/Web/canvaskit/canvaskit.wasm")
            .await;
        assert_eq!(
            hdr(&resp, "cache-control").as_deref(),
            Some("public, max-age=86400")
        );
    }

    #[tokio::test]
    async fn spa_fallback_and_missing_assets() {
        let data = tempfile::tempdir().unwrap();
        write_bundle(&data);
        let (server, _g) = server(&data, |_| {}).await;
        let resp = server
            .get("/Moonfin/Web/Some/Deep/Route")
            .await;
        resp.assert_status_ok();
        assert!(
            resp.text()
                .contains("moonfin</body>")
        );
        server
            .get("/Moonfin/Web/missing.js")
            .await
            .assert_status_not_found();
    }

    #[tokio::test]
    async fn extensionless_mixed_case_notices_is_served() {
        let data = tempfile::tempdir().unwrap();
        write_bundle(&data);
        let (server, _g) = server(&data, |_| {}).await;
        // The rewrite layer lowercases this to /moonfin/web/assets/notices.
        let resp = server
            .get("/Moonfin/Web/assets/NOTICES")
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.text(), "licences text");
    }

    #[tokio::test]
    async fn traversal_is_rejected() {
        let data = tempfile::tempdir().unwrap();
        write_bundle(&data);
        std::fs::write(
            data.path()
                .join("secret.txt"),
            "secret",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            data.path()
                .join("secret.txt"),
            data.path()
                .join("moonfin-web/leak.txt"),
        )
        .unwrap();
        let (server, _g) = server(&data, |_| {}).await;
        for path in [
            "/Moonfin/Web/..%2fsecret.txt",
            "/Moonfin/Web/..%2f..%2fsecret.txt",
            "/Moonfin/Web/%2e%2e/secret.txt",
            "/Moonfin/Web/..%5csecret.txt",
            "/Moonfin/Web/leak.txt",
        ] {
            let resp = server
                .get(path)
                .await;
            assert!(
                !resp
                    .text()
                    .contains("secret"),
                "{path} leaked"
            );
            assert_eq!(resp.status_code(), 404, "{path}");
        }
    }

    #[tokio::test]
    async fn encoded_absolute_path_never_reads_outside_root() {
        let data = tempfile::tempdir().unwrap();
        write_bundle(&data);
        let (server, _g) = server(&data, |_| {}).await;
        // Treated as a client-side route (index.html), never as /etc/passwd.
        let resp = server
            .get("/Moonfin/Web/%2fetc%2fpasswd")
            .await;
        assert!(
            !resp
                .text()
                .contains("root:")
        );
    }

    #[tokio::test]
    async fn directory_redirects_to_index() {
        let data = tempfile::tempdir().unwrap();
        write_bundle(&data);
        let (server, _g) = server(&data, |_| {}).await;
        let resp = server
            .get("/Moonfin/Web/theme/")
            .await;
        assert_eq!(resp.status_code(), 308);
        assert_eq!(
            hdr(&resp, "location").as_deref(),
            Some("/moonfin/web/theme/index.html")
        );
        server
            .get("/Moonfin/Web/theme/index.html")
            .await
            .assert_status_ok();
        server
            .get("/Moonfin/Web/theme/css/styles.css")
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn unconfigured_is_404_but_config_json_still_served() {
        let data = tempfile::tempdir().unwrap();
        let (server, _g) = server(&data, |_| {}).await;
        for path in [
            "/Moonfin/Web/",
            "/Moonfin/Web/main.dart.js",
            "/Moonfin/Web/x",
        ] {
            let resp = server
                .get(path)
                .await;
            resp.assert_status_not_found();
            assert!(
                resp.text()
                    .contains("hint")
            );
        }
        server
            .get("/Moonfin/Web/config.json")
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn configured_path_overrides_default() {
        let data = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(
            other
                .path()
                .join("index.html"),
            "elsewhere",
        )
        .unwrap();
        let p = other
            .path()
            .to_string_lossy()
            .into_owned();
        let (server, _g) = server(&data, |c| c.moonfin_web_path = Some(p)).await;
        assert_eq!(
            server
                .get("/Moonfin/Web/")
                .await
                .text(),
            "elsewhere"
        );
    }
}
