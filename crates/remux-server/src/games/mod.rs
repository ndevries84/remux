//! Retro-games foundation (Moonfin/Moonbase compatible): system/core
//! resolution, on-disk discovery, an in-memory id index and the synthetic
//! "Games" user view. HTTP endpoints live elsewhere.

pub mod resolver;
pub mod scanner;

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{Config, api};
pub use scanner::{Bios, Game, GamesSnapshot};

/// How long a scan stays fresh before the next access triggers a rescan.
pub const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

/// Whether the games feature is active: enabled and at least one root.
pub fn is_active(config: &Config) -> bool {
    config.games_enabled
        && config
            .games_roots
            .iter()
            .any(|r| {
                !r.trim()
                    .is_empty()
            })
}

/// Id of the synthetic Games user view (deterministic, v5 UUID).
pub fn games_view_id() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, b"remux-games-view")
}

/// The Games view id as clients see it: Jellyfin's dashless "simple" form,
/// exactly what `/userviews` emits for the view's `Id`. Clients compare ids
/// as exact strings, so everything that advertises the id must use this.
pub fn games_view_id_string() -> String {
    games_view_id()
        .simple()
        .to_string()
}

/// Synthetic Games view for `/userviews`.
///
/// `CollectionType` is `boxsets`: Moonfin's home screen skips
/// playlists/boxsets/livetv when requesting `/Items/Latest?ParentId=`, and of
/// those, jellyfin-web shows no "latest" section for boxsets and falls back to
/// a plain (here empty) item list on click, unlike `livetv`, which would
/// route to the Live TV page. Moonfin routes game libraries by id (from the
/// games API) before looking at the collection type.
pub fn games_view_item(name: &str) -> api::BaseItemDto {
    api::BaseItemDto {
        id: games_view_id(),
        name: Some(name.to_string()),
        server_id: crate::common::server_id(),
        type_: api::MediaType::UserView,
        collection_type: Some(api::CollectionType::Boxsets),
        is_folder: true,
        ..Default::default()
    }
}

/// Lazily-built, TTL-bound in-memory index of the games on disk. Cheap to
/// clone (shared state). Ids resolve through the snapshot only; no filesystem
/// path is ever derived from client input.
#[derive(Clone)]
pub struct GamesIndex {
    ttl: Duration,
    state: Arc<RwLock<Option<Arc<GamesSnapshot>>>>,
}

impl Default for GamesIndex {
    fn default() -> Self {
        Self::with_ttl(DEFAULT_TTL)
    }
}

impl GamesIndex {
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            state: Arc::new(RwLock::new(None)),
        }
    }

    fn roots(config: &Config) -> Vec<PathBuf> {
        config
            .games_roots
            .iter()
            .map(|r| r.trim())
            .filter(|r| !r.is_empty())
            .map(PathBuf::from)
            .collect()
    }

    fn fresh(&self, snap: &GamesSnapshot) -> bool {
        snap.scanned_at
            .elapsed()
            < self.ttl
    }

    /// Current snapshot, scanning on first access or once the TTL expired.
    /// Concurrent callers share a single scan.
    pub async fn snapshot(&self, config: &Config) -> Arc<GamesSnapshot> {
        if let Some(s) = self
            .state
            .read()
            .await
            .as_ref()
            .filter(|s| self.fresh(s))
        {
            return s.clone();
        }
        let mut guard = self
            .state
            .write()
            .await;
        if let Some(s) = guard
            .as_ref()
            .filter(|s| self.fresh(s))
        {
            return s.clone();
        }
        let snap = Self::scan(config).await;
        *guard = Some(snap.clone());
        snap
    }

    /// Forces a rescan regardless of TTL.
    pub async fn rescan(&self, config: &Config) -> Arc<GamesSnapshot> {
        let mut guard = self
            .state
            .write()
            .await;
        let snap = Self::scan(config).await;
        *guard = Some(snap.clone());
        snap
    }

    async fn scan(config: &Config) -> Arc<GamesSnapshot> {
        let roots = Self::roots(config);
        let started = Instant::now();
        let snap = tokio::task::spawn_blocking(move || scanner::scan(&roots))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "games scan task failed");
                GamesSnapshot::empty()
            });
        tracing::info!(
            games = snap
                .games
                .len(),
            elapsed_ms = started
                .elapsed()
                .as_millis() as u64,
            "games scan complete"
        );
        Arc::new(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(root: &std::path::Path) -> Config {
        Config {
            games_enabled: true,
            games_roots: vec![
                root.to_string_lossy()
                    .into_owned(),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn view_id_is_deterministic_lowercase_hyphenated() {
        let s = games_view_id().to_string();
        assert_eq!(s, games_view_id().to_string());
        assert_eq!(s, s.to_lowercase());
        assert_eq!(
            s.matches('-')
                .count(),
            4
        );
        assert_ne!(games_view_id(), crate::api::shows::livetv_view_id());
    }

    #[test]
    fn view_item_shape() {
        let item = games_view_item("My Games");
        assert_eq!(item.id, games_view_id());
        assert_eq!(
            item.name
                .as_deref(),
            Some("My Games")
        );
        assert_eq!(item.type_, api::MediaType::UserView);
        assert_eq!(item.collection_type, Some(api::CollectionType::Boxsets));
        assert!(item.is_folder);
    }

    #[test]
    fn is_active_requires_flag_and_a_root() {
        let mut c = Config::default();
        assert!(!is_active(&c));
        c.games_enabled = true;
        assert!(!is_active(&c));
        c.games_roots = vec!["  ".into()];
        assert!(!is_active(&c));
        c.games_roots = vec!["/x".into()];
        assert!(is_active(&c));
        c.games_enabled = false;
        assert!(!is_active(&c));
    }

    #[tokio::test]
    async fn index_scans_lazily_caches_and_rescans() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(
            dir.path()
                .join("NES"),
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("NES/a.nes"),
            b"a",
        )
        .unwrap();
        let cfg = config_with(dir.path());

        let index = GamesIndex::default();
        let first = index
            .snapshot(&cfg)
            .await;
        assert_eq!(
            first
                .games
                .len(),
            1
        );

        std::fs::write(
            dir.path()
                .join("NES/b.nes"),
            b"b",
        )
        .unwrap();
        // Within TTL: cached.
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            1
        );
        // Explicit rescan picks it up.
        let rescanned = index
            .rescan(&cfg)
            .await;
        assert_eq!(
            rescanned
                .games
                .len(),
            2
        );
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn expired_ttl_triggers_rescan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(
            dir.path()
                .join("NES"),
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("NES/a.nes"),
            b"a",
        )
        .unwrap();
        let cfg = config_with(dir.path());

        let index = GamesIndex::with_ttl(Duration::ZERO);
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            1
        );
        std::fs::write(
            dir.path()
                .join("NES/b.nes"),
            b"b",
        )
        .unwrap();
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            2
        );
    }

    async fn server_with(
        games_enabled: bool,
        roots: Vec<String>,
    ) -> (
        axum_test::TestServer,
        crate::integration_test::TestGuard,
        String,
    ) {
        use crate::integration_test::*;
        let (server, guard) = new_test_server_with_config(Config {
            database_url: Some("sqlite::memory:".into()),
            torrent_http_port: None,
            torrent_peer_port: None,
            disable_dht: true,
            games_enabled,
            games_roots: roots,
            ..Default::default()
        })
        .await
        .unwrap();
        let resp = server
            .post("/users/authenticatebyname")
            .add_header(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_static(AUTH_HEADER),
            )
            .json(&serde_json::json!({ "Username": "test", "Pw": "test" }))
            .await;
        let token = resp.json::<serde_json::Value>()["AccessToken"]
            .as_str()
            .unwrap()
            .to_string();
        (server, guard, auth_header_with_token(&token))
    }

    fn view_ids(body: &serde_json::Value) -> Vec<Uuid> {
        body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| {
                v["Id"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
            })
            .collect()
    }

    #[tokio::test]
    async fn userviews_include_games_view_only_when_active() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .to_string_lossy()
            .into_owned();
        let get = |server: &axum_test::TestServer, auth: &str| {
            server
                .get("/userviews")
                .add_header(
                    http::header::AUTHORIZATION,
                    http::HeaderValue::from_str(auth).unwrap(),
                )
                .into_future()
        };

        // Enabled but no roots, and roots but disabled: absent.
        for (enabled, roots) in [(true, vec![]), (false, vec![root.clone()])] {
            let (server, _g, auth) = server_with(enabled, roots).await;
            let body = get(&server, &auth)
                .await
                .json::<serde_json::Value>();
            assert!(!view_ids(&body).contains(&games_view_id()));
        }

        let (server, _g, auth) = server_with(true, vec![root]).await;
        let resp = get(&server, &auth).await;
        let body = resp.json::<serde_json::Value>();
        assert!(view_ids(&body).contains(&games_view_id()));
        let view = body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| {
                v["Id"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    == Some(games_view_id())
            })
            .unwrap();
        assert_eq!(view["Name"], "Games");
        assert_eq!(view["Type"], "UserView");
        assert_eq!(view["CollectionType"], "boxsets");
    }

    #[tokio::test]
    async fn games_view_short_circuits_item_and_listing_requests() {
        let dir = tempfile::tempdir().unwrap();
        let (server, _g, auth) = server_with(
            true,
            vec![
                dir.path()
                    .to_string_lossy()
                    .into_owned(),
            ],
        )
        .await;
        let id = games_view_id();
        let hdr = || http::HeaderValue::from_str(&auth).unwrap();

        let me = server
            .get("/users/me")
            .add_header(http::header::AUTHORIZATION, hdr())
            .await
            .json::<serde_json::Value>();
        let user_id = me["Id"]
            .as_str()
            .unwrap()
            .to_string();

        for path in [
            format!("/items/{id}"),
            format!("/users/{user_id}/items/{id}"),
        ] {
            let resp = server
                .get(&path)
                .add_header(http::header::AUTHORIZATION, hdr())
                .await;
            let body = resp.json::<serde_json::Value>();
            assert_eq!(body["Name"], "Games", "{path}");
            assert_eq!(body["Type"], "UserView", "{path}");
        }
        for path in ["/items".to_string(), format!("/users/{user_id}/items")] {
            let resp = server
                .get(&path)
                .add_query_params(&[("parentId", id.to_string())])
                .add_header(http::header::AUTHORIZATION, hdr())
                .await;
            let body = resp.json::<serde_json::Value>();
            assert_eq!(
                body["Items"]
                    .as_array()
                    .unwrap()
                    .len(),
                0,
                "{path}"
            );
            assert_eq!(body["TotalRecordCount"], 0, "{path}");
        }
        for path in [
            "/items/latest".to_string(),
            format!("/users/{user_id}/items/latest"),
        ] {
            let resp = server
                .get(&path)
                .add_query_params(&[("parentId", id.to_string())])
                .add_header(http::header::AUTHORIZATION, hdr())
                .await;
            assert_eq!(
                resp.json::<serde_json::Value>(),
                serde_json::json!([]),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn games_view_item_is_not_served_when_disabled() {
        let (server, _g, auth) = server_with(false, vec![]).await;
        let resp = server
            .get(&format!("/items/{}", games_view_id()))
            .add_header(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(&auth).unwrap(),
            )
            .expect_failure()
            .await;
        assert_eq!(resp.status_code(), http::StatusCode::NOT_FOUND);
    }
}
