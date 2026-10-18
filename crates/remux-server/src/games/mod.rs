//! Retro-games foundation (Moonfin/Moonbase compatible): system/core
//! resolution, on-disk discovery, an in-memory id index and the synthetic
//! "Games" user view. HTTP endpoints live elsewhere.

pub mod archive;
pub mod cores;
pub mod launchbox;
pub mod meta;
pub mod names;
pub mod rdb;
pub mod resolver;
pub mod scanner;
pub mod thumbs;

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

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

/// How long to wait before retrying after a scan that failed, timed out or
/// found its roots missing (capped by the TTL).
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);

/// What a scan produced, plus whether every configured root was readable.
/// An empty snapshot from unreadable roots is a failure, not an empty library.
pub struct ScanOutput {
    pub snapshot: GamesSnapshot,
    pub roots_ok: bool,
}

type ScanFn = Arc<dyn Fn(&[PathBuf]) -> ScanOutput + Send + Sync>;

fn scan_roots(roots: &[PathBuf]) -> ScanOutput {
    let roots_ok = roots
        .iter()
        .all(|r| {
            r.canonicalize()
                .is_ok_and(|c| c.is_dir())
        });
    ScanOutput {
        snapshot: scanner::scan(roots),
        roots_ok,
    }
}

struct State {
    current: Option<Arc<GamesSnapshot>>,
    /// The last scan failed (or found its roots missing): `current` is
    /// stale regardless of the TTL and a retry is due at `next_attempt`.
    degraded: bool,
    next_attempt: Option<Instant>,
    /// A scan task (or its blocking thread) is alive. Single-flight.
    running: bool,
    /// Bumped every time a scan publishes an outcome.
    generation: u64,
}

struct Shared {
    ttl: Duration,
    scan: ScanFn,
    state: std::sync::Mutex<State>,
    published: tokio::sync::Notify,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn backoff(&self) -> Duration {
        self.ttl
            .min(FAILURE_BACKOFF)
    }

    /// Records a scan outcome. A failure never replaces a non-empty snapshot.
    fn publish(&self, outcome: Result<ScanOutput, &str>) {
        let mut st = self.lock();
        let old_has_games = st
            .current
            .as_ref()
            .is_some_and(|s| {
                !s.games
                    .is_empty()
            });
        let now = Instant::now();
        let failure = match &outcome {
            Ok(out) => (out
                .snapshot
                .games
                .is_empty()
                && !out.roots_ok)
                .then_some("games roots are not accessible"),
            Err(why) => Some(*why),
        };
        match (failure, outcome) {
            (Some(why), _) if old_has_games => {
                tracing::warn!(
                    reason = why,
                    "games scan failed; keeping the previous game list"
                );
                st.degraded = true;
                st.next_attempt = Some(now + self.backoff());
            }
            (Some(why), _) => {
                tracing::warn!(
                    reason = why,
                    "games scan failed; serving an empty list"
                );
                st.current = Some(Arc::new(GamesSnapshot::empty()));
                st.degraded = true;
                st.next_attempt = Some(now + self.backoff());
            }
            (None, Ok(out)) => {
                tracing::info!(
                    games = out
                        .snapshot
                        .games
                        .len(),
                    "games scan complete"
                );
                st.current = Some(Arc::new(out.snapshot));
                st.degraded = false;
                st.next_attempt = None;
            }
            (None, Err(_)) => unreachable!("an Err outcome is always a failure"),
        }
        st.generation += 1;
        drop(st);
        self.published
            .notify_waiters();
    }

    async fn run_scan(self: Arc<Self>, roots: Vec<PathBuf>, timeout: Duration) {
        let started = Instant::now();
        let scan = self
            .scan
            .clone();
        let mut handle = tokio::task::spawn_blocking(move || scan(&roots));
        match tokio::time::timeout(timeout, &mut handle).await {
            Ok(Ok(out)) => self.publish(Ok(out)),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "games scan task failed");
                self.publish(Err("scan task panicked"));
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = timeout.as_secs(),
                    "games scan timed out; serving the previous game list meanwhile"
                );
                self.publish(Err("scan timed out"));
                // The blocking thread cannot be cancelled: stay single-flight
                // until it ends, and accept its result if it does finish.
                if let Ok(out) = handle.await {
                    self.publish(Ok(out));
                }
            }
        }
        tracing::debug!(
            elapsed_ms = started
                .elapsed()
                .as_millis() as u64,
            "games scan finished"
        );
        self.lock()
            .running = false;
        // Waiters of an idle index (see `settled`) re-check `running`.
        self.published
            .notify_waiters();
    }

    /// Starts a scan task unless one is alive. Returns the generation to wait
    /// past to observe an outcome of the (new or already running) scan.
    fn start_scan(self: &Arc<Self>, config: &Config) -> u64 {
        let mut st = self.lock();
        let generation = st.generation;
        if !st.running {
            st.running = true;
            let timeout = Duration::from_secs(config.games_scan_timeout_secs);
            tokio::spawn(
                self.clone()
                    .run_scan(GamesIndex::roots(config), timeout),
            );
        }
        generation
    }

    async fn wait_published(&self, after: u64) {
        loop {
            let notified = self
                .published
                .notified();
            tokio::pin!(notified);
            notified
                .as_mut()
                .enable();
            if self
                .lock()
                .generation
                > after
            {
                return;
            }
            notified.await;
        }
    }
}

/// Lazily-built, TTL-bound in-memory index of the games on disk. Cheap to
/// clone (shared state). Ids resolve through the snapshot only; no filesystem
/// path is ever derived from client input.
///
/// Stale-while-revalidate: once a snapshot exists, requests return it without
/// waiting. An expired TTL starts a single background rescan, and a rescan
/// that fails, times out or finds the roots missing never replaces a non-empty
/// snapshot.
#[derive(Clone)]
pub struct GamesIndex {
    shared: Arc<Shared>,
}

impl Default for GamesIndex {
    fn default() -> Self {
        Self::with_ttl(DEFAULT_TTL)
    }
}

impl GamesIndex {
    pub fn with_ttl(ttl: Duration) -> Self {
        Self::build(ttl, Arc::new(scan_roots))
    }

    fn build(ttl: Duration, scan: ScanFn) -> Self {
        Self {
            shared: Arc::new(Shared {
                ttl,
                scan,
                state: std::sync::Mutex::new(State {
                    current: None,
                    degraded: false,
                    next_attempt: None,
                    running: false,
                    generation: 0,
                }),
                published: tokio::sync::Notify::new(),
            }),
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

    /// Current snapshot. Returns the existing one immediately (starting a
    /// background rescan when it is stale and one is due); only the very
    /// first call waits for a scan, bounded by `games_scan_timeout_secs`.
    pub async fn snapshot(&self, config: &Config) -> Arc<GamesSnapshot> {
        let after = {
            let st = self
                .shared
                .lock();
            match &st.current {
                Some(s) => {
                    let stale = st.degraded
                        || s.scanned_at
                            .elapsed()
                            >= self
                                .shared
                                .ttl;
                    let due = st
                        .next_attempt
                        .is_none_or(|t| Instant::now() >= t);
                    let current = s.clone();
                    drop(st);
                    if stale && due {
                        self.shared
                            .start_scan(config);
                    }
                    return current;
                }
                None => {
                    drop(st);
                    self.shared
                        .start_scan(config)
                }
            }
        };
        self.shared
            .wait_published(after)
            .await;
        self.current_or_empty()
    }

    /// Forces a rescan regardless of TTL (joining one already running) and
    /// waits for its outcome. A failed rescan leaves the previous snapshot.
    pub async fn rescan(&self, config: &Config) -> Arc<GamesSnapshot> {
        let after = self
            .shared
            .start_scan(config);
        self.shared
            .wait_published(after)
            .await;
        self.current_or_empty()
    }

    fn current_or_empty(&self) -> Arc<GamesSnapshot> {
        self.shared
            .lock()
            .current
            .clone()
            .unwrap_or_else(|| Arc::new(GamesSnapshot::empty()))
    }

    /// Resolves once no scan is running.
    #[cfg(test)]
    async fn settled(&self) {
        loop {
            let notified = self
                .shared
                .published
                .notified();
            tokio::pin!(notified);
            notified
                .as_mut()
                .enable();
            if !self
                .shared
                .lock()
                .running
            {
                return;
            }
            notified.await;
        }
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

    fn write_game(root: &std::path::Path, name: &str) {
        std::fs::create_dir_all(root.join("NES")).unwrap();
        std::fs::write(
            root.join("NES")
                .join(name),
            name,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn expired_ttl_serves_the_old_list_then_picks_up_the_rescan() {
        let dir = tempfile::tempdir().unwrap();
        write_game(dir.path(), "a.nes");
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
        write_game(dir.path(), "b.nes");
        // Expired: the old list comes back at once and a rescan starts.
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            1
        );
        index
            .settled()
            .await;
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            2
        );
    }

    /// A parked scan step for tests: the scanner blocks until released.
    struct Gate {
        released: std::sync::Mutex<bool>,
        cv: std::sync::Condvar,
    }

    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                released: std::sync::Mutex::new(false),
                cv: std::sync::Condvar::new(),
            })
        }
        fn release(&self) {
            *self
                .released
                .lock()
                .unwrap() = true;
            self.cv
                .notify_all();
        }
        fn wait(&self) {
            let mut r = self
                .released
                .lock()
                .unwrap();
            while !*r {
                r = self
                    .cv
                    .wait(r)
                    .unwrap();
            }
        }
    }

    enum Step {
        Real,
        Panic,
        Gated(Arc<Gate>),
    }

    /// An index whose scans follow a script (default: a real scan) and count
    /// how many times the scanner ran.
    fn scripted_index(
        ttl: Duration,
    ) -> (
        GamesIndex,
        Arc<std::sync::Mutex<std::collections::VecDeque<Step>>>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let steps = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scan: ScanFn = {
            let (steps, calls) = (steps.clone(), calls.clone());
            Arc::new(move |roots: &[PathBuf]| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let step = steps
                    .lock()
                    .unwrap()
                    .pop_front();
                match step.unwrap_or(Step::Real) {
                    Step::Real => {}
                    Step::Panic => panic!("scripted scan failure"),
                    Step::Gated(g) => g.wait(),
                }
                scan_roots(roots)
            })
        };
        (GamesIndex::build(ttl, scan), steps, calls)
    }

    fn zero_timeout(root: &std::path::Path) -> Config {
        Config {
            games_scan_timeout_secs: 0,
            ..config_with(root)
        }
    }

    async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
        // A hang guard, not a timing assertion: the awaited futures only
        // finish through explicit sync points.
        tokio::time::timeout(Duration::from_secs(60), f)
            .await
            .expect("hung")
    }

    #[tokio::test]
    async fn stale_snapshot_is_served_without_waiting_and_one_rescan_runs() {
        let dir = tempfile::tempdir().unwrap();
        write_game(dir.path(), "a.nes");
        let cfg = config_with(dir.path());
        let (index, steps, calls) = scripted_index(Duration::ZERO);
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            1
        );

        write_game(dir.path(), "b.nes");
        let gate = Gate::new();
        steps
            .lock()
            .unwrap()
            .push_back(Step::Gated(gate.clone()));
        // The rescan is parked, yet every concurrent request returns at once.
        for _ in 0..5 {
            assert_eq!(
                within(index.snapshot(&cfg))
                    .await
                    .games
                    .len(),
                1
            );
        }
        gate.release();
        within(index.settled()).await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "first scan plus a single shared rescan"
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
    async fn failed_scan_keeps_the_old_snapshot_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        write_game(dir.path(), "a.nes");
        let cfg = config_with(dir.path());
        let (index, steps, _) = scripted_index(Duration::ZERO);
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            1
        );

        write_game(dir.path(), "b.nes");
        steps
            .lock()
            .unwrap()
            .push_back(Step::Panic);
        assert_eq!(
            within(index.rescan(&cfg))
                .await
                .games
                .len(),
            1
        );
        // The failure is retried on a later request.
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            1
        );
        within(index.settled()).await;
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
    async fn first_scan_failure_is_not_cached_as_fresh() {
        let dir = tempfile::tempdir().unwrap();
        write_game(dir.path(), "a.nes");
        let cfg = config_with(dir.path());
        let (index, steps, _) = scripted_index(Duration::ZERO);
        steps
            .lock()
            .unwrap()
            .push_back(Step::Panic);
        assert_eq!(
            within(index.snapshot(&cfg))
                .await
                .games
                .len(),
            0
        );
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            0
        );
        within(index.settled()).await;
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn removed_root_keeps_the_old_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .join("roms");
        write_game(&root, "a.nes");
        let cfg = config_with(&root);
        let index = GamesIndex::default();
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            1
        );

        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            index
                .rescan(&cfg)
                .await
                .games
                .len(),
            1
        );

        // Once the mount is back the new content is picked up.
        write_game(&root, "a.nes");
        write_game(&root, "b.nes");
        assert_eq!(
            index
                .rescan(&cfg)
                .await
                .games
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn genuinely_empty_library_replaces_the_old_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        write_game(dir.path(), "a.nes");
        let cfg = config_with(dir.path());
        let index = GamesIndex::default();
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            1
        );

        std::fs::remove_file(
            dir.path()
                .join("NES/a.nes"),
        )
        .unwrap();
        assert_eq!(
            index
                .rescan(&cfg)
                .await
                .games
                .len(),
            0
        );
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn first_scan_wait_is_bounded_and_a_late_result_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        write_game(dir.path(), "a.nes");
        let (index, steps, _) = scripted_index(DEFAULT_TTL);
        let gate = Gate::new();
        steps
            .lock()
            .unwrap()
            .push_back(Step::Gated(gate.clone()));
        // Timeout of zero seconds: the parked scan cannot finish in time.
        let cfg = zero_timeout(dir.path());
        assert_eq!(
            within(index.snapshot(&cfg))
                .await
                .games
                .len(),
            0
        );

        gate.release();
        within(index.settled()).await;
        assert_eq!(
            index
                .snapshot(&cfg)
                .await
                .games
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn timed_out_rescan_keeps_the_old_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        write_game(dir.path(), "a.nes");
        let (index, steps, _) = scripted_index(Duration::ZERO);
        assert_eq!(
            index
                .snapshot(&config_with(dir.path()))
                .await
                .games
                .len(),
            1
        );

        write_game(dir.path(), "b.nes");
        let gate = Gate::new();
        steps
            .lock()
            .unwrap()
            .push_back(Step::Gated(gate.clone()));
        let cfg = zero_timeout(dir.path());
        assert_eq!(
            within(index.rescan(&cfg))
                .await
                .games
                .len(),
            1
        );

        gate.release();
        within(index.settled()).await;
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
