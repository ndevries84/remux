//! Shared test harness for the Moonfin endpoint modules: the full router
//! behind the production URI rewrite layer, so tests hit mixed-case
//! Moonfin client URLs exactly as the client sends them.

use crate::{
    AppContext, Config, init_app_with_ctx,
    integration_test::{AUTH_HEADER, TestGuard, auth_header_with_token},
    rewrite_request_uri,
};
use axum::Router;
use axum_test::{TestResponse, TestServer};
use http::header;
use serde_json::{Value, json};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tower::{Layer, util::MapRequestLayer};

pub(crate) struct Harness {
    pub server: TestServer,
    pub ctx: AppContext,
    _guard: TestGuard,
    /// Admin ("test") access token.
    pub token: String,
    /// The same router the mock `server` drives, for real-socket tests.
    pub app: Router,
    _data: tempfile::TempDir,
}

impl Harness {
    pub async fn new() -> Self {
        Self::new_with(|_| {}).await
    }

    /// [`Harness::new`] with a chance to adjust the `Config` first.
    pub async fn new_with(tweak: impl FnOnce(&mut Config)) -> Self {
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
        tweak(&mut config);
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
            ctx: ctx.clone(),
            _guard: TestGuard(ctx),
            token: String::new(),
            app,
            _data: data,
        };
        h.token = h
            .login("test", "test")
            .await;
        h
    }

    fn auth_for(token: &str) -> http::HeaderValue {
        http::HeaderValue::from_str(&auth_header_with_token(token)).unwrap()
    }

    pub async fn login(&self, name: &str, pw: &str) -> String {
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
    pub async fn second_user(&self) -> String {
        self.server
            .post("/Users/New")
            .add_header(header::AUTHORIZATION, Self::auth_for(&self.token))
            .json(&json!({ "Name": "second", "Password": "pw" }))
            .await;
        self.login("second", "pw")
            .await
    }

    pub async fn get(&self, token: &str, path: &str) -> TestResponse {
        self.server
            .get(path)
            .add_header(header::AUTHORIZATION, Self::auth_for(token))
            .await
    }

    pub async fn post(&self, token: &str, path: &str, body: &Value) -> TestResponse {
        self.server
            .post(path)
            .add_header(header::AUTHORIZATION, Self::auth_for(token))
            .json(body)
            .await
    }

    pub async fn post_empty(&self, token: &str, path: &str) -> TestResponse {
        self.server
            .post(path)
            .add_header(header::AUTHORIZATION, Self::auth_for(token))
            .await
    }

    pub async fn delete(&self, token: &str, path: &str) -> TestResponse {
        self.server
            .delete(path)
            .add_header(header::AUTHORIZATION, Self::auth_for(token))
            .await
    }
}

/// Saves a movie with a unique TMDB id (the canonical-id check rejects
/// media without one) and returns its id in the dashless form `BaseItemDto.Id` serialises as.
pub(crate) async fn seed_movie(ctx: &AppContext, title: &str, tmdb: i64) -> String {
    seed_movie_full(ctx, title, tmdb, &[], None).await
}

/// [`seed_movie`] with genres and a certification age (parental rating).
pub(crate) async fn seed_movie_full(
    ctx: &AppContext,
    title: &str,
    tmdb: i64,
    genres: &[&str],
    certification_age: Option<i32>,
) -> String {
    use crate::db;
    let external_ids = db::ExternalIds {
        tmdb: Some(tmdb),
        ..Default::default()
    };
    let mut media = db::Media {
        id: uuid::Uuid::from(&db::MediaIdRaw {
            kind: db::MediaKind::Movie,
            external_ids: external_ids.clone(),
            season: None,
            episode: None,
        }),
        title: title.into(),
        kind: db::MediaKind::Movie,
        external_ids,
        certification_age,
        certification: certification_age.map(|a| format!("age-{a}")),
        ..Default::default()
    };
    media
        .save(&ctx.db)
        .await
        .unwrap();
    if !genres.is_empty() {
        let names: Vec<String> = genres
            .iter()
            .map(|g| g.to_string())
            .collect();
        let pairs = db::build_genre_relations_from_names(
            media.id,
            &names,
            db::MediaKind::Genre,
        );
        let genre_rows: Vec<_> = pairs
            .iter()
            .map(|(_, m)| m.clone())
            .collect();
        let relations: Vec<_> = pairs
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        db::Media::upsert(&ctx.db, &genre_rows)
            .await
            .unwrap();
        db::MediaRelation::upsert(&ctx.db, &relations)
            .await
            .unwrap();
    }
    media
        .id
        .simple()
        .to_string()
}

/// Minimal raw-HTTP SSE reader over a real socket (the mock transport
/// buffers whole bodies, which an open stream never finishes).
pub(crate) struct SseConn {
    stream: TcpStream,
    pub seen: String,
}

impl SseConn {
    pub async fn open(app: &Router, token: &str) -> SseConn {
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

    pub async fn wait_for(&mut self, needle: &str) {
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
    pub async fn appears_within(&mut self, needle: &str, wait: Duration) -> bool {
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
