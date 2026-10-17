//! Moonfin EmulatorJS web player (`/moonfin/emulatorjs/...`).
//!
//! Mirrors Moonbase's `EmulatorController`: the player shell and bridge are
//! static, anonymous documents (the webview sends no auth headers; the ROM,
//! BIOS and save URLs it fetches carry their own `ApiKey`). EmulatorJS itself
//! is loaded from the self-hosted `<data_dir>/games/emulatorjs/data/` folder
//! when present, otherwise from the EmulatorJS CDN. Like Moonbase, these
//! routes are not gated on the games feature being enabled.

use std::{
    path::{Component, Path as FsPath, PathBuf},
    str::FromStr,
};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::Engine;
use rand::RngCore;
use remux_macros::get;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{
    AppState,
    games::{self, resolver::Core},
};

const PLAYER_HTML: &str = include_str!("../../assets/moonfin/player.html");
const BRIDGE_JS: &str = include_str!("../../assets/moonfin/moonfin-bridge.js");

/// Placeholder in `player.html` replaced with the EmulatorJS data URL.
const PATHTODATA_PLACEHOLDER: &str = "__EJS_PATHTODATA__";
/// Placeholder in `player.html` replaced with a per-response CSP nonce.
const NONCE_PLACEHOLDER: &str = "__CSP_NONCE__";
/// EmulatorJS release served from the CDN. Moonbase tracks the moving "stable"
/// channel; we pin the release (4.2.3 is what "stable" resolves to and what
/// Moonfin's bridge was verified against) so the CDN cannot change the scripts
/// executed in this origin. To bump: pick a stable release at least two weeks
/// old, confirm `https://cdn.emulatorjs.org/<version>/data/loader.js` exists,
/// and re-verify `moonfin-bridge.js` (it reaches into EmulatorJS internals).
pub(crate) const EMULATORJS_VERSION: &str = "4.2.3";
const CDN_ORIGIN: &str = "https://cdn.emulatorjs.org";
/// Self-hosted data URL, relative to `/moonfin/emulatorjs/player.html`.
const SELF_HOSTED_DATA_URL: &str = "./data/";
/// Cores whose WASM build needs SharedArrayBuffer (a cross-origin isolated document).
const THREAD_REQUIRED_CORES: &[Core] = &[Core::Psp];

#[derive(serde::Deserialize)]
struct PlayerQuery {
    #[serde(default)]
    core: Option<String>,
}

fn data_root(state: &AppState) -> PathBuf {
    games::cores::data_dir(
        &state
            .ctx
            .config,
    )
}

/// Joins `requested` under `root`, rejecting anything but plain path
/// components (no `..`, absolute paths, drive prefixes or backslashes).
pub(crate) fn contained_path(root: &FsPath, requested: &str) -> Option<PathBuf> {
    if requested.contains('\\') || requested.contains('\0') {
        return None;
    }
    let mut out = root.to_path_buf();
    for comp in FsPath::new(requested.trim_start_matches('/')).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn content_type_for(path: &FsPath) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript",
        Some("mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        Some("png") => "image/png",
        Some("jpg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ttf") => "font/ttf",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// CSP for the player document. It is same-origin with jellyfin-web (and its
/// stored tokens), so scripts are limited to this origin, the EmulatorJS data
/// origin and the nonce'd inline scripts. ROM/BIOS/save URLs are built by the
/// client against this server, hence `connect-src 'self'`.
/// `blob:` and `'unsafe-eval'` are required by EmulatorJS 4.2.3 itself: the
/// core JS is injected as a blob script and its 7z extractor worker uses eval.
fn content_security_policy(data_origin: &str, nonce: &str) -> String {
    format!(
        "default-src 'none'; \
         script-src 'self' 'nonce-{nonce}' 'wasm-unsafe-eval' 'unsafe-eval' blob: {data_origin}; \
         worker-src blob: {data_origin}; \
         connect-src 'self' blob: data: {data_origin}; \
         img-src 'self' data: blob: {data_origin}; \
         media-src 'self' data: blob: {data_origin}; \
         style-src 'self' 'unsafe-inline' {data_origin}; \
         font-src 'self' data: {data_origin}; \
         object-src 'none'; base-uri 'none'; form-action 'none'; \
         frame-ancestors 'self'"
    )
}

#[get("/moonfin/emulatorjs/player.html")]
pub async fn emulator_player(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<PlayerQuery>,
) -> Response {
    // Parse, don't validate: an unknown core never reaches EmulatorJS. A missing
    // core is allowed (player.html shows its own "missing parameters" message).
    let core = match q
        .core
        .as_deref()
        .filter(|c| !c.is_empty())
    {
        None => None,
        Some(c) => match Core::from_str(c) {
            Ok(core) => Some(core),
            Err(_) => return (StatusCode::BAD_REQUEST, "unknown core").into_response(),
        },
    };
    // A half-extracted or stale folder must not flip the player to a broken
    // self-hosted mode: it needs the required files and a matching version.
    let self_hosted = games::cores::data_installed(&data_root(&state)).await;
    let (data_url, script_origin) = if self_hosted {
        (SELF_HOSTED_DATA_URL.to_string(), "'self'")
    } else {
        (
            format!("{CDN_ORIGIN}/{EMULATORJS_VERSION}/data/"),
            CDN_ORIGIN,
        )
    };
    let mut nonce_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = base64::engine::general_purpose::STANDARD.encode(nonce_bytes);
    let html = PLAYER_HTML
        .replace(PATHTODATA_PLACEHOLDER, &data_url)
        .replace(NONCE_PLACEHOLDER, &nonce);
    let csp = content_security_policy(script_origin, &nonce);
    let mut resp = (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        html,
    )
        .into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // Only cores needing SharedArrayBuffer get isolation headers, so the other
    // cores keep loading normally (including from the CDN).
    if core.is_some_and(|c| THREAD_REQUIRED_CORES.contains(&c)) {
        h.insert(
            "cross-origin-opener-policy",
            HeaderValue::from_static("same-origin"),
        );
        h.insert(
            "cross-origin-embedder-policy",
            HeaderValue::from_static("credentialless"),
        );
    }
    resp
}

#[get("/moonfin/emulatorjs/moonfin-bridge.js")]
pub async fn emulator_bridge() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/javascript"),
        )],
        BRIDGE_JS,
    )
        .into_response()
}

async fn serve_data(state: &AppState, requested: &str, req: Request<Body>) -> Response {
    let root = data_root(state);
    if !games::cores::data_installed(&root).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let requested = if requested
        .trim_matches('/')
        .is_empty()
    {
        "loader.js"
    } else {
        requested
    };
    let Some(full) = contained_path(&root, requested) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !tokio::fs::metadata(&full)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
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
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    }
    resp
}

#[get("/moonfin/emulatorjs/data/{*path}")]
pub async fn emulator_data(
    State(state): State<AppState>,
    Path(path): Path<String>,
    req: Request<Body>,
) -> Response {
    serve_data(&state, &path, req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Config, init_app_with_ctx, integration_test::TestGuard, rewrite_request_uri,
    };
    use axum::Router;
    use axum_test::TestServer;
    use tower::Layer;
    use tower::util::MapRequestLayer;

    async fn server(data: &tempfile::TempDir) -> (TestServer, TestGuard) {
        let (router, ctx) = init_app_with_ctx(Config {
            database_url: Some("sqlite::memory:".into()),
            torrent_http_port: None,
            torrent_peer_port: None,
            disable_dht: true,
            data_dir: data
                .path()
                .to_path_buf(),
            ..Default::default()
        })
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

    fn hdr(resp: &axum_test::TestResponse, name: &str) -> Option<String> {
        resp.headers()
            .get(name)
            .map(|v| {
                v.to_str()
                    .unwrap()
                    .to_string()
            })
    }

    #[tokio::test]
    async fn player_served_anonymously_with_cdn_default() {
        let data = tempfile::tempdir().unwrap();
        let (s, _g) = server(&data).await;
        let resp = s
            .get("/Moonfin/EmulatorJS/player.html?core=nes&rom=http%3A%2F%2Fx%2From")
            .await;
        resp.assert_status_ok();
        assert!(
            hdr(&resp, "content-type")
                .unwrap()
                .starts_with("text/html")
        );
        assert_eq!(hdr(&resp, "cross-origin-opener-policy"), None);
        assert_eq!(hdr(&resp, "cross-origin-embedder-policy"), None);
        let body = resp.text();
        assert!(body.contains("https://cdn.emulatorjs.org/4.2.3/data/"));
        assert!(!body.contains("/stable/"));
        assert!(!body.contains(PATHTODATA_PLACEHOLDER));
        assert!(!body.contains(NONCE_PLACEHOLDER));
        assert_eq!(
            hdr(&resp, "x-content-type-options").as_deref(),
            Some("nosniff")
        );
        assert_eq!(
            hdr(&resp, "referrer-policy").as_deref(),
            Some("no-referrer")
        );
        let csp = hdr(&resp, "content-security-policy").unwrap();
        assert!(csp.contains("script-src 'self' 'nonce-"));
        assert!(csp.contains("https://cdn.emulatorjs.org"));
        assert!(csp.contains("frame-ancestors 'self'"));
        assert!(!csp.contains("script-src 'unsafe-inline'"));
        // The nonce in the CSP matches the inline scripts in the body.
        let nonce = csp
            .split("'nonce-")
            .nth(1)
            .unwrap()
            .split('\'')
            .next()
            .unwrap();
        assert!(body.contains(&format!("<script nonce=\"{nonce}\">")));
    }

    #[tokio::test]
    async fn player_unknown_core_rejected_and_known_cores_accepted() {
        let data = tempfile::tempdir().unwrap();
        let (s, _g) = server(&data).await;
        for bad in ["evil", "nes%22%3E", "..%2Fx", "NES", "psp%20"] {
            let r = s
                .get(&format!("/Moonfin/EmulatorJS/player.html?core={bad}&rom=x"))
                .await;
            assert_eq!(r.status_code(), StatusCode::BAD_REQUEST, "{bad}");
        }
        for good in [
            "nes",
            "snes",
            "gb",
            "gba",
            "segaMD",
            "segaMS",
            "segaGG",
            "n64",
            "nds",
            "vb",
            "atari2600",
            "atari7800",
            "lynx",
            "ws",
            "ngp",
            "pce",
            "psx",
            "psp",
            "mame",
            "arcade",
        ] {
            let r = s
                .get(&format!(
                    "/Moonfin/EmulatorJS/player.html?core={good}&rom=x"
                ))
                .await;
            r.assert_status_ok();
        }
        // Missing core: same as Moonbase, the page renders its own error.
        s.get("/Moonfin/EmulatorJS/player.html")
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn player_psp_gets_isolation_headers() {
        let data = tempfile::tempdir().unwrap();
        let (s, _g) = server(&data).await;
        let resp = s
            .get("/Moonfin/EmulatorJS/player.html?core=psp&rom=x")
            .await;
        resp.assert_status_ok();
        assert_eq!(
            hdr(&resp, "cross-origin-opener-policy").as_deref(),
            Some("same-origin")
        );
        assert_eq!(
            hdr(&resp, "cross-origin-embedder-policy").as_deref(),
            Some("credentialless")
        );
    }

    #[tokio::test]
    async fn player_uses_self_hosted_data_when_installed() {
        let data = tempfile::tempdir().unwrap();
        let root = data
            .path()
            .join("games/emulatorjs/data");
        std::fs::create_dir_all(root.join("cores")).unwrap();
        for f in games::cores::REQUIRED_FILES {
            std::fs::write(root.join(f), "// bundle").unwrap();
        }
        std::fs::write(root.join("cores/x.wasm"), b"\0asm").unwrap();
        let (s, _g) = server(&data).await;
        let resp = s
            .get("/Moonfin/EmulatorJS/player.html?core=nes&rom=x")
            .await;
        assert!(
            !hdr(&resp, "content-security-policy")
                .unwrap()
                .contains("cdn.emulatorjs.org")
        );
        let body = resp.text();
        assert!(body.contains("window.EJS_pathtodata = './data/'"));
        let r = s
            .get("/Moonfin/EmulatorJS/data/loader.js")
            .await;
        r.assert_status_ok();
        assert_eq!(
            hdr(&r, "content-type").as_deref(),
            Some("application/javascript")
        );
        let r = s
            .get("/Moonfin/EmulatorJS/data/cores/x.wasm")
            .await;
        r.assert_status_ok();
        assert_eq!(hdr(&r, "content-type").as_deref(), Some("application/wasm"));
    }

    #[tokio::test]
    async fn partial_or_stale_install_keeps_the_cdn() {
        let data = tempfile::tempdir().unwrap();
        let root = data
            .path()
            .join("games/emulatorjs/data");
        std::fs::create_dir_all(&root).unwrap();
        // Only two of the three required files.
        std::fs::write(root.join("loader.js"), "x").unwrap();
        std::fs::write(root.join("emulator.min.js"), "x").unwrap();
        let (s, _g) = server(&data).await;
        let page = "/Moonfin/EmulatorJS/player.html?core=nes&rom=x";
        let body = s
            .get(page)
            .await
            .text();
        assert!(body.contains("https://cdn.emulatorjs.org/4.2.3/data/"));
        s.get("/Moonfin/EmulatorJS/data/loader.js")
            .await
            .assert_status_not_found();

        // Complete, but marked as another EmulatorJS version: still stale.
        std::fs::write(root.join("emulator.min.css"), "x").unwrap();
        std::fs::write(root.join(games::cores::VERSION_MARKER), "4.0.0").unwrap();
        let body = s
            .get(page)
            .await
            .text();
        assert!(body.contains("https://cdn.emulatorjs.org/4.2.3/data/"));

        // Matching marker: self-hosted.
        std::fs::write(root.join(games::cores::VERSION_MARKER), EMULATORJS_VERSION)
            .unwrap();
        let body = s
            .get(page)
            .await
            .text();
        assert!(body.contains("window.EJS_pathtodata = './data/'"));
    }

    #[tokio::test]
    async fn bridge_js_served_anonymously() {
        let data = tempfile::tempdir().unwrap();
        let (s, _g) = server(&data).await;
        let resp = s
            .get("/Moonfin/EmulatorJS/moonfin-bridge.js")
            .await;
        resp.assert_status_ok();
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/javascript")
        );
        assert!(
            resp.text()
                .contains("moonfinGetState")
        );
    }

    #[tokio::test]
    async fn data_404_when_not_installed_and_traversal_rejected() {
        let data = tempfile::tempdir().unwrap();
        let (s, _g) = server(&data).await;
        s.get("/Moonfin/EmulatorJS/data/loader.js")
            .await
            .assert_status_not_found();

        // Installed: traversal toward a real sibling file must still 404.
        let root = data
            .path()
            .join("games/emulatorjs/data");
        std::fs::create_dir_all(&root).unwrap();
        for f in games::cores::REQUIRED_FILES {
            std::fs::write(root.join(f), "x").unwrap();
        }
        std::fs::write(
            data.path()
                .join("games/secret.txt"),
            "secret",
        )
        .unwrap();
        for p in [
            "/Moonfin/EmulatorJS/data/..%2Fsecret.txt",
            "/Moonfin/EmulatorJS/data/..%2F..%2Fetc%2Fpasswd",
            "/Moonfin/EmulatorJS/data/%2Fetc%2Fpasswd",
            "/Moonfin/EmulatorJS/data/a%5C..%5C..%5Csecret.txt",
        ] {
            let r = s
                .get(p)
                .await;
            assert_eq!(r.status_code(), StatusCode::NOT_FOUND, "{p}");
        }
    }

    #[test]
    fn contained_path_rejects_parent_and_absolute() {
        let root = FsPath::new("/r");
        assert!(contained_path(root, "../x").is_none());
        assert!(contained_path(root, "a/../../x").is_none());
        assert_eq!(
            contained_path(root, "a/b.js"),
            Some(PathBuf::from("/r/a/b.js"))
        );
    }
}
