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
use tower::{Layer, util::MapRequestLayer};

pub(crate) struct Harness {
    pub server: TestServer,
    pub ctx: AppContext,
    _guard: TestGuard,
    /// Admin ("test") access token.
    pub token: String,
    _data: tempfile::TempDir,
}

impl Harness {
    pub async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let config = Config {
            database_url: Some("sqlite::memory:".into()),
            torrent_http_port: None,
            torrent_peer_port: None,
            disable_dht: true,
            data_dir: data
                .path()
                .to_path_buf(),
            ..Default::default()
        };
        let (router, ctx) = init_app_with_ctx(config)
            .await
            .unwrap();
        let app = Router::new()
            .fallback_service(MapRequestLayer::new(rewrite_request_uri).layer(router));
        let server = TestServer::builder()
            .mock_transport()
            .build(app)
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
        ..Default::default()
    };
    media
        .save(&ctx.db)
        .await
        .unwrap();
    media
        .id
        .simple()
        .to_string()
}
