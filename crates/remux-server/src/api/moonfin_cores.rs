//! Server-installed EmulatorJS cores (`/moonfin/games/cores/...`).
//!
//! Mirrors Moonbase's `GamesController` cores routes: `Status` for any
//! signed-in user, `Install` and `Upload` for admins (both answer `202` with
//! the current status; progress is polled through `Status`). Like Moonbase
//! these are not gated on the games feature flag. The install pipeline lives
//! in [`crate::games::cores`].
//!
//! * Non-admins get `403` (not the `401` of the shared admin extractor, which
//!   a client may treat as an expired session).
//! * `Status` adds `bytesDownloaded`/`totalBytes` (only while downloading) to
//!   Moonbase's five keys.

use axum::{
    Json,
    body::Body,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use remux_macros::{get, post};
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::{
    AppState,
    db::auth::AuthSession,
    games::cores::{self, ArchiveFormat},
};

/// Moonbase's error body shape for the cores routes.
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "Error": message }))).into_response()
}

fn forbidden(session: &AuthSession) -> Option<Response> {
    (!session
        .user
        .is_admin)
        .then(|| error(StatusCode::FORBIDDEN, "Admin access required"))
}

async fn accepted(state: &AppState) -> Response {
    let status = state
        .ctx
        .cores
        .status(
            &state
                .ctx
                .config,
        )
        .await;
    (StatusCode::ACCEPTED, Json(status)).into_response()
}

#[get("/moonfin/games/cores/status")]
pub async fn games_cores_status(
    State(state): State<AppState>,
    _session: AuthSession,
) -> Response {
    Json(
        state
            .ctx
            .cores
            .status(
                &state
                    .ctx
                    .config,
            )
            .await,
    )
    .into_response()
}

#[post("/moonfin/games/cores/install")]
pub async fn games_cores_install(
    State(state): State<AppState>,
    session: AuthSession,
) -> Response {
    if let Some(denied) = forbidden(&session) {
        return denied;
    }
    state
        .ctx
        .cores
        .start_install(
            &state
                .ctx
                .config,
        );
    accepted(&state).await
}

#[post("/moonfin/games/cores/upload")]
pub async fn games_cores_upload(
    State(state): State<AppState>,
    session: AuthSession,
    body: Body,
) -> Response {
    if let Some(denied) = forbidden(&session) {
        return denied;
    }
    let config = &state
        .ctx
        .config;
    // An install already running keeps the slot; the upload is not read.
    let Some(claim) = state
        .ctx
        .cores
        .try_begin()
    else {
        return accepted(&state).await;
    };
    let max = cores::max_upload_bytes(config);
    let path = match cores::upload_temp_path(config).await {
        Ok(p) => p,
        Err(e) => {
            claim.fail(cores::CoresError::Io(e));
            return accepted(&state).await;
        }
    };
    let streamed = stream_to_file(&claim, body, &path, max).await;
    match streamed {
        Ok(0) => {
            let _ = tokio::fs::remove_file(&path).await;
            claim.release();
            error(StatusCode::BAD_REQUEST, "Empty upload.")
        }
        Ok(_) => {
            state
                .ctx
                .cores
                .start_extract(claim, config, path);
            accepted(&state).await
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&path).await;
            let status = match e {
                cores::CoresError::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
                cores::CoresError::UnsupportedArchive => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            let message = e.to_string();
            claim.fail(e);
            error(status, &message)
        }
    }
}

/// Writes the request body to `path`, refusing more than `max` bytes or
/// anything that is not a 7z/zip. Returns the byte count (0 = empty body).
async fn stream_to_file(
    claim: &cores::Claim,
    body: Body,
    path: &std::path::Path,
    max: u64,
) -> Result<u64, cores::CoresError> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    let mut head = Vec::with_capacity(8);
    while let Some(chunk) = stream
        .next()
        .await
    {
        let chunk =
            chunk.map_err(|_| cores::CoresError::Http("upload interrupted".into()))?;
        total += chunk.len() as u64;
        if total > max {
            return Err(cores::CoresError::TooLarge { max });
        }
        if head.len() < 6 {
            head.extend_from_slice(
                &chunk[..chunk
                    .len()
                    .min(6 - head.len())],
            );
        }
        file.write_all(&chunk)
            .await?;
        claim.set_downloaded(total);
    }
    file.flush()
        .await?;
    if total > 0 && ArchiveFormat::sniff(&head).is_none() {
        return Err(cores::CoresError::UnsupportedArchive);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::moonfin_test_support::Harness,
        games::cores::{VERSION_MARKER, base_dir, data_dir},
        integration_test::auth_header_with_token,
    };
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::{io::Write as _, path::PathBuf, time::Duration};

    const STATUS: &str = "/Moonfin/Games/Cores/Status";
    const INSTALL: &str = "/Moonfin/Games/Cores/Install";
    const UPLOAD: &str = "/Moonfin/Games/Cores/Upload";
    const PLAYER: &str = "/Moonfin/EmulatorJS/player.html?core=nes&rom=x";

    fn bundle(version_json: &str) -> Vec<(String, Vec<u8>)> {
        vec![
            ("CHANGES.md".into(), b"repo file outside data".to_vec()),
            ("data/loader.js".into(), b"// loader".to_vec()),
            ("data/emulator.min.js".into(), b"// min js".to_vec()),
            ("data/emulator.min.css".into(), b"/* css */".to_vec()),
            (
                "data/version.json".into(),
                version_json
                    .as_bytes()
                    .to_vec(),
            ),
        ]
    }

    fn good_bundle() -> Vec<(String, Vec<u8>)> {
        bundle(r#"{"version":"4.2.3","current_version":999999}"#)
    }

    fn seven_z(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("a.7z");
        let mut w = sevenz_rust2::ArchiveWriter::create(&path).unwrap();
        for (name, data) in entries {
            w.push_archive_entry(
                sevenz_rust2::ArchiveEntry::new_file(name),
                Some(data.as_slice()),
            )
            .unwrap();
        }
        w.finish()
            .unwrap();
        std::fs::read(path).unwrap()
    }

    fn zip_bytes(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        for (name, data) in entries {
            w.start_file(name.as_str(), zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data)
                .unwrap();
        }
        w.finish()
            .unwrap();
        buf.into_inner()
    }

    fn sha_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    async fn mount<'a>(
        server: &'a httpmock::MockServer,
        archive: &[u8],
        delay: Duration,
    ) -> httpmock::Mock<'a> {
        let body = archive.to_vec();
        server
            .mock_async(move |when, then| {
                when.method(httpmock::Method::GET)
                    .path("/cores.7z");
                then.status(200)
                    .body(body)
                    .delay(delay);
            })
            .await
    }

    async fn harness_for(url: &str, sha: &str, size: Option<u64>) -> Harness {
        let (url, sha) = (url.to_string(), sha.to_string());
        Harness::new_with(move |c| {
            c.games_cores_url = Some(url);
            c.games_cores_sha256 = Some(sha);
            c.games_cores_size = size;
        })
        .await
    }

    async fn status(h: &Harness) -> Value {
        h.get(&h.token, STATUS)
            .await
            .json()
    }

    async fn wait_settled(h: &Harness) -> Value {
        for _ in 0..200 {
            let s = status(h).await;
            if s["state"] != "downloading" {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("install never finished");
    }

    async fn upload(
        h: &Harness,
        token: &str,
        body: Vec<u8>,
    ) -> axum_test::TestResponse {
        h.server
            .post(UPLOAD)
            .add_header(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(&auth_header_with_token(token)).unwrap(),
            )
            .bytes(body.into())
            .await
    }

    /// True when the player is not self-hosted. Since the CDN is opt-in, a
    /// server without installed cores serves the "not installed" page (no CDN
    /// in the body or the CSP) instead of the CDN player.
    async fn player_not_installed(h: &Harness) -> bool {
        let r = h
            .server
            .get(PLAYER)
            .await;
        let csp = r
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(!csp.contains("cdn.emulatorjs.org"));
        let body = r.text();
        assert!(!body.contains("https://cdn.emulatorjs.org"));
        let self_hosted = body.contains("window.EJS_pathtodata = './data/'");
        // A stale install now says "out of date" rather than "aren't installed".
        assert_eq!(
            !self_hosted,
            body.contains("Emulator files aren't installed on this server")
                || body.contains("Emulator files on this server are out of date")
        );
        !self_hosted
    }

    fn leftovers(h: &Harness) -> Vec<String> {
        let base = base_dir(
            &h.ctx
                .config,
        );
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let name = e
                    .file_name()
                    .to_string_lossy()
                    .into_owned();
                if name == "tmp" {
                    out.extend(
                        std::fs::read_dir(e.path())
                            .unwrap()
                            .flatten()
                            .map(|f| {
                                format!(
                                    "tmp/{}",
                                    f.file_name()
                                        .to_string_lossy()
                                )
                            }),
                    );
                } else if name != "data" {
                    out.push(name);
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn status_is_idle_initially_and_needs_auth() {
        let h = Harness::new().await;
        let idle = serde_json::json!({
            "installed": false, "downloading": false, "state": "idle",
            "error": null, "filesInstalled": 0,
        });
        assert_eq!(status(&h).await, idle);
        let second = h
            .second_user()
            .await;
        assert_eq!(
            h.get(&second, STATUS)
                .await
                .json::<Value>(),
            idle
        );
        h.server
            .get(STATUS)
            .expect_failure()
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        h.server
            .post(INSTALL)
            .expect_failure()
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_admin_cannot_install_or_upload() {
        let h = Harness::new().await;
        let second = h
            .second_user()
            .await;
        h.post_empty(&second, INSTALL)
            .await
            .assert_status(StatusCode::FORBIDDEN);
        upload(&h, &second, seven_z(&good_bundle()))
            .await
            .assert_status(StatusCode::FORBIDDEN);
        assert_eq!(status(&h).await["state"], "idle");
        assert!(
            !data_dir(
                &h.ctx
                    .config
            )
            .exists()
        );
    }

    #[tokio::test]
    async fn install_downloads_verifies_and_switches_the_player() {
        let archive = seven_z(&good_bundle());
        let server = httpmock::MockServer::start_async().await;
        let mock = mount(&server, &archive, Duration::from_millis(300)).await;
        let url = server.url("/cores.7z");
        let h = harness_for(&url, &sha_hex(&archive), Some(archive.len() as u64)).await;
        assert!(player_not_installed(&h).await);

        let resp = h
            .post_empty(&h.token, INSTALL)
            .await;
        resp.assert_status(StatusCode::ACCEPTED);
        let v: Value = resp.json();
        assert_eq!(v["state"], "downloading");
        assert_eq!(v["downloading"], true);
        assert_eq!(v["installed"], false);
        assert!(
            v.get("bytesDownloaded")
                .is_some()
        );

        let done = wait_settled(&h).await;
        assert_eq!(
            done,
            serde_json::json!({
                "installed": true, "downloading": false, "state": "installed",
                "error": null, "filesInstalled": 4,
            })
        );
        mock.assert_hits_async(1)
            .await;
        let data = data_dir(
            &h.ctx
                .config,
        );
        assert_eq!(
            std::fs::read_to_string(data.join("loader.js")).unwrap(),
            "// loader"
        );
        assert_eq!(
            std::fs::read_to_string(data.join(VERSION_MARKER)).unwrap(),
            "4.2.3"
        );
        assert!(
            !data
                .join("CHANGES.md")
                .exists()
        );
        assert!(leftovers(&h).is_empty(), "{:?}", leftovers(&h));

        assert!(!player_not_installed(&h).await);
        assert!(
            h.server
                .get(PLAYER)
                .await
                .text()
                .contains("window.EJS_pathtodata = './data/'")
        );
        h.server
            .get("/Moonfin/EmulatorJS/data/loader.js")
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn checksum_mismatch_fails_and_installs_nothing() {
        let archive = seven_z(&good_bundle());
        let server = httpmock::MockServer::start_async().await;
        let _mock = mount(&server, &archive, Duration::ZERO).await;
        let url = server.url("/cores.7z");
        let h = harness_for(&url, &"00".repeat(32), None).await;
        h.post_empty(&h.token, INSTALL)
            .await
            .assert_status(StatusCode::ACCEPTED);
        let s = wait_settled(&h).await;
        assert_eq!(s["state"], "failed");
        assert_eq!(s["installed"], false);
        assert!(
            s["error"]
                .as_str()
                .unwrap()
                .contains("checksum mismatch")
        );
        assert!(
            !data_dir(
                &h.ctx
                    .config
            )
            .exists()
        );
        assert!(leftovers(&h).is_empty(), "{:?}", leftovers(&h));
        assert!(player_not_installed(&h).await);
    }

    #[tokio::test]
    async fn size_mismatch_fails_and_keeps_the_previous_install() {
        let archive = seven_z(&good_bundle());
        let server = httpmock::MockServer::start_async().await;
        let _mock = mount(&server, &archive, Duration::ZERO).await;
        let url = server.url("/cores.7z");
        let h =
            harness_for(&url, &sha_hex(&archive), Some(archive.len() as u64 + 1)).await;
        // A complete earlier install must survive a failed attempt.
        let data = data_dir(
            &h.ctx
                .config,
        );
        std::fs::create_dir_all(&data).unwrap();
        for f in cores::REQUIRED_FILES {
            std::fs::write(data.join(f), "previous").unwrap();
        }
        h.post_empty(&h.token, INSTALL)
            .await;
        let s = wait_settled(&h).await;
        assert_eq!(s["state"], "failed");
        assert_eq!(s["installed"], true);
        assert!(
            s["error"]
                .as_str()
                .unwrap()
                .contains("size mismatch")
        );
        assert_eq!(
            std::fs::read_to_string(data.join("loader.js")).unwrap(),
            "previous"
        );
        assert!(leftovers(&h).is_empty(), "{:?}", leftovers(&h));
    }

    #[tokio::test]
    async fn download_over_the_cap_fails() {
        let archive = seven_z(&good_bundle());
        let server = httpmock::MockServer::start_async().await;
        let _mock = mount(&server, &archive, Duration::ZERO).await;
        let url = server.url("/cores.7z");
        let (u, sha) = (url, sha_hex(&archive));
        let h = Harness::new_with(move |c| {
            c.games_cores_url = Some(u);
            c.games_cores_sha256 = Some(sha);
            c.games_cores_max_download_bytes = 16;
        })
        .await;
        h.post_empty(&h.token, INSTALL)
            .await;
        let s = wait_settled(&h).await;
        assert_eq!(s["state"], "failed");
        assert!(
            s["error"]
                .as_str()
                .unwrap()
                .contains("larger than")
        );
        assert!(leftovers(&h).is_empty());
    }

    #[tokio::test]
    async fn override_without_checksum_is_refused() {
        let h = Harness::new_with(|c| {
            c.games_cores_url = Some("http://127.0.0.1:1/x.7z".into());
        })
        .await;
        h.post_empty(&h.token, INSTALL)
            .await
            .assert_status(StatusCode::ACCEPTED);
        let s = wait_settled(&h).await;
        assert_eq!(s["state"], "failed");
        assert!(
            s["error"]
                .as_str()
                .unwrap()
                .contains("requires games_cores_sha256")
        );
    }

    #[tokio::test]
    async fn concurrent_install_is_single_flight() {
        let archive = seven_z(&good_bundle());
        let server = httpmock::MockServer::start_async().await;
        let mock = mount(&server, &archive, Duration::from_millis(500)).await;
        let url = server.url("/cores.7z");
        let h = harness_for(&url, &sha_hex(&archive), None).await;
        let (a, b) = tokio::join!(
            h.post_empty(&h.token, INSTALL),
            h.post_empty(&h.token, INSTALL)
        );
        a.assert_status(StatusCode::ACCEPTED);
        b.assert_status(StatusCode::ACCEPTED);
        assert_eq!(a.json::<Value>()["state"], "downloading");
        assert_eq!(b.json::<Value>()["state"], "downloading");
        // An upload while busy does not displace the running install.
        upload(
            &h,
            &h.token
                .clone(),
            seven_z(&good_bundle()),
        )
        .await
        .assert_status(StatusCode::ACCEPTED);
        assert_eq!(wait_settled(&h).await["state"], "installed");
        mock.assert_hits_async(1)
            .await;
    }

    #[tokio::test]
    async fn upload_7z_and_zip_without_data_prefix() {
        let h = Harness::new().await;
        let r = upload(
            &h,
            &h.token
                .clone(),
            seven_z(&good_bundle()),
        )
        .await;
        r.assert_status(StatusCode::ACCEPTED);
        let s = wait_settled(&h).await;
        assert_eq!(s["state"], "installed");
        assert_eq!(s["filesInstalled"], 4);
        assert!(!player_not_installed(&h).await);

        // A zip whose root is the data folder itself replaces the install.
        let flat: Vec<(String, Vec<u8>)> = vec![
            ("loader.js".into(), b"// second".to_vec()),
            ("emulator.min.js".into(), b"x".to_vec()),
            ("emulator.min.css".into(), b"x".to_vec()),
            ("version.json".into(), br#"{"version":"4.2.3"}"#.to_vec()),
            ("cores/nes.data".into(), b"core".to_vec()),
        ];
        upload(
            &h,
            &h.token
                .clone(),
            zip_bytes(&flat),
        )
        .await
        .assert_status(StatusCode::ACCEPTED);
        let s = wait_settled(&h).await;
        assert_eq!(s["state"], "installed");
        assert_eq!(s["filesInstalled"], 5);
        let data = data_dir(
            &h.ctx
                .config,
        );
        assert_eq!(
            std::fs::read_to_string(data.join("loader.js")).unwrap(),
            "// second"
        );
        assert!(
            data.join("cores/nes.data")
                .is_file()
        );
        assert!(leftovers(&h).is_empty(), "{:?}", leftovers(&h));
    }

    #[tokio::test]
    async fn upload_empty_and_garbage_are_rejected() {
        let h = Harness::new().await;
        let r = upload(
            &h,
            &h.token
                .clone(),
            Vec::new(),
        )
        .await;
        r.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            r.json::<Value>(),
            serde_json::json!({ "Error": "Empty upload." })
        );
        assert_eq!(status(&h).await["state"], "idle");

        let r = upload(
            &h,
            &h.token
                .clone(),
            b"definitely not an archive".to_vec(),
        )
        .await;
        r.assert_status(StatusCode::BAD_REQUEST);
        let s = status(&h).await;
        assert_eq!(s["state"], "failed");
        assert!(leftovers(&h).is_empty(), "{:?}", leftovers(&h));
    }

    #[tokio::test]
    async fn upload_over_the_cap_is_rejected() {
        let h = Harness::new_with(|c| c.games_cores_max_download_bytes = 64).await;
        let r = upload(
            &h,
            &h.token
                .clone(),
            seven_z(&good_bundle()),
        )
        .await;
        r.assert_status(StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(status(&h).await["state"], "failed");
        assert!(leftovers(&h).is_empty(), "{:?}", leftovers(&h));
    }

    #[tokio::test]
    async fn zip_slip_entries_are_rejected() {
        let mut entries = good_bundle();
        entries.push(("data/../../escape.txt".into(), b"pwned".to_vec()));
        for archive in [seven_z(&entries), zip_bytes(&entries)] {
            let h = Harness::new().await;
            upload(
                &h,
                &h.token
                    .clone(),
                archive,
            )
            .await
            .assert_status(StatusCode::ACCEPTED);
            let s = wait_settled(&h).await;
            assert_eq!(s["state"], "failed");
            assert!(
                s["error"]
                    .as_str()
                    .unwrap()
                    .contains("unsafe path")
            );
            let games: PathBuf = h
                .ctx
                .config
                .data_dir
                .join("games");
            assert!(
                !games
                    .join("escape.txt")
                    .exists()
            );
            assert!(
                !base_dir(
                    &h.ctx
                        .config
                )
                .join("escape.txt")
                .exists()
            );
            assert!(
                !data_dir(
                    &h.ctx
                        .config
                )
                .exists()
            );
            assert!(leftovers(&h).is_empty(), "{:?}", leftovers(&h));
        }
    }

    #[tokio::test]
    async fn incomplete_or_wrong_version_bundles_fail() {
        let h = Harness::new().await;
        let mut missing = good_bundle();
        missing.retain(|(n, _)| n != "data/emulator.min.css");
        upload(
            &h,
            &h.token
                .clone(),
            seven_z(&missing),
        )
        .await
        .assert_status(StatusCode::ACCEPTED);
        let s = wait_settled(&h).await;
        assert_eq!(s["state"], "failed");
        assert!(
            s["error"]
                .as_str()
                .unwrap()
                .contains("emulator.min.css")
        );
        assert!(player_not_installed(&h).await);

        upload(
            &h,
            &h.token
                .clone(),
            seven_z(&bundle(r#"{"version":"4.0.0"}"#)),
        )
        .await
        .assert_status(StatusCode::ACCEPTED);
        let s = wait_settled(&h).await;
        assert_eq!(s["state"], "failed");
        assert!(
            s["error"]
                .as_str()
                .unwrap()
                .contains("expected 4.2.3")
        );
        assert!(
            !data_dir(
                &h.ctx
                    .config
            )
            .exists()
        );
        assert!(player_not_installed(&h).await);
        // A later success clears the error.
        upload(
            &h,
            &h.token
                .clone(),
            seven_z(&good_bundle()),
        )
        .await;
        let s = wait_settled(&h).await;
        assert_eq!(s["state"], "installed");
        assert_eq!(s["error"], Value::Null);
    }

    #[tokio::test]
    async fn extraction_over_the_cap_fails() {
        let h = Harness::new_with(|c| c.games_cores_max_extract_bytes = 10).await;
        upload(
            &h,
            &h.token
                .clone(),
            seven_z(&good_bundle()),
        )
        .await;
        let s = wait_settled(&h).await;
        assert_eq!(s["state"], "failed");
        assert!(
            !data_dir(
                &h.ctx
                    .config
            )
            .exists()
        );
        assert!(leftovers(&h).is_empty(), "{:?}", leftovers(&h));
    }

    #[tokio::test]
    async fn disk_state_survives_restart_and_stale_marker_is_not_installed() {
        let h = Harness::new().await;
        let data = data_dir(
            &h.ctx
                .config,
        );
        std::fs::create_dir_all(data.join("cores")).unwrap();
        for f in cores::REQUIRED_FILES {
            std::fs::write(data.join(f), "x").unwrap();
        }
        std::fs::write(data.join("cores/a.data"), "x").unwrap();
        std::fs::write(data.join(VERSION_MARKER), "4.2.3").unwrap();
        // Fresh in-memory state, bundle on disk: installed but idle.
        assert_eq!(
            status(&h).await,
            serde_json::json!({
                "installed": true, "downloading": false, "state": "idle",
                "error": null, "filesInstalled": 4,
            })
        );
        std::fs::write(data.join(VERSION_MARKER), "4.1.0").unwrap();
        let s = status(&h).await;
        assert_eq!(s["installed"], false);
        assert_eq!(s["filesInstalled"], 0);
        assert!(player_not_installed(&h).await);
    }

    #[tokio::test]
    async fn cleanup_removes_staging_and_restores_a_half_swapped_install() {
        let h = Harness::new().await;
        let base = base_dir(
            &h.ctx
                .config,
        );
        std::fs::create_dir_all(base.join("data.staging-x")).unwrap();
        std::fs::create_dir_all(base.join("tmp")).unwrap();
        std::fs::write(base.join("tmp/a.download"), "x").unwrap();
        std::fs::create_dir_all(base.join("data.old-y")).unwrap();
        std::fs::write(base.join("data.old-y/loader.js"), "kept").unwrap();
        h.ctx
            .cores
            .cleanup_stale(
                &h.ctx
                    .config,
            )
            .await;
        assert_eq!(
            std::fs::read_to_string(base.join("data/loader.js")).unwrap(),
            "kept"
        );
        assert!(leftovers(&h).is_empty(), "{:?}", leftovers(&h));
    }
}
