//! Game metadata (genre, developer, publisher, franchise, region, year,
//! players) from the libretro game databases.
//!
//! The per-system `.rdb` files are downloaded lazily and in the background
//! (never on the request path), cached under
//! `<data_dir>/games/meta/rdb/<Platform>.rdb` and revalidated with
//! `ETag`/`Last-Modified` once they are older than
//! `games_metadata_refresh_days`. Failures are logged and backed off; they
//! never fail a request, a detail just has no metadata fields yet.
//!
//! Matching is CRC-first: a plain ROM is hashed once (cached by path, size and
//! modification time, on the blocking pool), an archived single ROM uses the
//! CRC the archive's own index already stores, and only then does it fall back
//! to the normalized name (see [`super::rdb`]).
//!
//! Data source: libretro-database (<https://github.com/libretro/libretro-database>,
//! CC-BY-SA-4.0), fetched at runtime and not redistributed.

use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime},
};

use dashmap::{DashMap, mapref::entry::Entry};
use futures_util::{
    FutureExt, StreamExt,
    future::{BoxFuture, Shared},
};
use tokio::sync::Semaphore;
use tracing::{debug, warn};
use uuid::Uuid;

use super::{
    Game, GamesSnapshot,
    launchbox::{self, LaunchBox},
    rdb::{self, GameMeta, MAX_RDB_BYTES, RdbIndex},
    resolver::Core,
    thumbs,
};
use crate::Config;

/// How long a request waits for work that is already in flight (a cached
/// database being parsed, a ROM being hashed). Anything slower finishes in
/// the background and shows up on the next request.
pub const REQUEST_WAIT: Duration = Duration::from_millis(250);
/// How long the startup warm-up waits on one game (a database still being
/// downloaded or parsed, a big ROM being hashed).
const WARM_WAIT: Duration = Duration::from_secs(300);
/// Games resolved at once by the warm-up (hashing is capped separately).
const WARM_CONCURRENCY: usize = 8;
/// Simultaneous database downloads (be polite to the CDN).
const MAX_CONCURRENT_DOWNLOADS: usize = 2;
/// Simultaneous ROM hashing jobs.
const MAX_CONCURRENT_HASHES: usize = 2;
/// Entries kept in the CRC cache before it is reset.
const MAX_CRC_CACHE: usize = 65_536;
const HASH_CHUNK: usize = 64 * 1024;

type SharedIndex = Shared<BoxFuture<'static, Option<Arc<RdbIndex>>>>;
type SharedCrcs = Shared<BoxFuture<'static, Option<Arc<Vec<u32>>>>>;

/// Per-call copy of the relevant configuration.
#[derive(Clone)]
struct Settings {
    base: String,
    dir: PathBuf,
    refresh: Duration,
    retry: Duration,
    max_hash_bytes: u64,
    max_rdb_bytes: usize,
}

impl Settings {
    fn new(config: &Config) -> Self {
        Self {
            base: config
                .games_metadata_rdb_base_url
                .trim()
                .to_string(),
            dir: config
                .data_dir
                .join("games")
                .join("meta")
                .join("rdb"),
            refresh: Duration::from_secs(
                u64::from(
                    config
                        .games_metadata_refresh_days
                        .max(1),
                ) * 24
                    * 3600,
            ),
            retry: Duration::from_secs(
                config
                    .games_metadata_retry_secs
                    .max(1),
            ),
            max_hash_bytes: config.games_metadata_max_hash_bytes,
            max_rdb_bytes: MAX_RDB_BYTES,
        }
    }

    fn is_remote(&self) -> bool {
        let b = self
            .base
            .to_ascii_lowercase();
        b.starts_with("http://") || b.starts_with("https://")
    }
}

enum Slot {
    Loading(SharedIndex),
    Ready {
        index: Arc<RdbIndex>,
        /// When the cache should be revalidated.
        next_check: Instant,
    },
    Failed {
        until: Instant,
    },
}

enum IndexState {
    Ready(Arc<RdbIndex>),
    Pending(SharedIndex),
    Unavailable,
}

/// A CRC is still being computed.
struct Pending;

enum CrcState {
    /// Candidates known without any I/O.
    Known(Vec<u32>),
    /// A CRC cannot be had for this game (too big, archive without one).
    Unavailable,
    Pending(SharedCrcs),
}

struct Inner {
    client: reqwest::Client,
    download_permits: Semaphore,
    hash_permits: Semaphore,
    slots: Mutex<HashMap<&'static str, Slot>>,
    crc_cache: Mutex<HashMap<PathBuf, (u64, SystemTime, Arc<Vec<u32>>)>>,
    crc_inflight: DashMap<PathBuf, SharedCrcs>,
    /// `scanned_at` of the snapshot last used to pre-load databases.
    prewarmed: Mutex<Option<Instant>>,
    launchbox: LaunchBox,
    /// Test hook: the next database load panics.
    #[cfg(test)]
    panic_next_load: std::sync::atomic::AtomicBool,
}

/// Resets a database slot if its load task unwinds, so the slot does not stay
/// `Loading` (and every caller stuck on it) forever. A refresh keeps serving
/// the index it already had.
struct LoadGuard {
    inner: Arc<Inner>,
    platform: &'static str,
    keep: Option<Arc<RdbIndex>>,
    retry: Duration,
    armed: bool,
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let until = Instant::now() + self.retry;
        let slot = match self
            .keep
            .take()
        {
            Some(index) => Slot::Ready {
                index,
                next_check: until,
            },
            None => Slot::Failed { until },
        };
        warn!(
            platform = self.platform,
            "games: metadata database load panicked"
        );
        lock(
            &self
                .inner
                .slots,
        )
        .insert(self.platform, slot);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Metadata lookups for game details. Cheap to clone.
#[derive(Clone)]
pub struct MetaService {
    inner: Arc<Inner>,
}

impl Default for MetaService {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "Remux/",
                env!("CARGO_PKG_VERSION"),
                " (game metadata)"
            ))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .expect("failed to build metadata client");
        Self {
            inner: Arc::new(Inner {
                client,
                download_permits: Semaphore::new(MAX_CONCURRENT_DOWNLOADS),
                hash_permits: Semaphore::new(MAX_CONCURRENT_HASHES),
                slots: Mutex::new(HashMap::new()),
                crc_cache: Mutex::new(HashMap::new()),
                crc_inflight: DashMap::new(),
                prewarmed: Mutex::new(None),
                launchbox: LaunchBox::default(),
                #[cfg(test)]
                panic_next_load: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }
}

impl MetaService {
    /// State of the LaunchBox index (meaningful when LaunchBox is enabled).
    pub fn launchbox_status(&self) -> launchbox::LaunchBoxStatus {
        self.inner
            .launchbox
            .status()
    }

    /// Starts loading the databases of every system in `snap` (once per
    /// scan) so the first game opened already finds them ready. Never blocks.
    pub fn prewarm(&self, config: &Config, snap: &GamesSnapshot) {
        if config.games_launchbox_enabled {
            // Cheap and self-throttling: builds or refreshes the LaunchBox
            // index in one background task, whatever the scan cadence.
            let cores: HashSet<Core> = snap
                .games
                .iter()
                .map(|g| g.core)
                .collect();
            self.inner
                .launchbox
                .prewarm(
                    &self
                        .inner
                        .client,
                    config,
                    &cores,
                );
        }
        if !config.games_metadata_enabled {
            return;
        }
        {
            let mut last = lock(
                &self
                    .inner
                    .prewarmed,
            );
            if *last == Some(snap.scanned_at) {
                return;
            }
            *last = Some(snap.scanned_at);
        }
        let st = Settings::new(config);
        let platforms: HashSet<&'static str> = snap
            .games
            .iter()
            .flat_map(|g| thumbs::platforms(g.core, g.served_file_name()))
            .copied()
            .collect();
        for platform in platforms {
            let _ = self.ensure_index(&st, platform);
        }
    }

    /// Brings everything a detail needs into memory ahead of the first
    /// request: starts the LaunchBox update and the database loads
    /// ([`prewarm`](Self::prewarm)), waits for them (downloads included, as
    /// the usual rules allow), then resolves every game once so ROM hashes,
    /// parsed databases and LaunchBox indexes are all cached. Meant for a
    /// background task at startup; requests never wait for it.
    pub async fn warm(&self, config: &Config, snap: &GamesSnapshot) {
        if !config.games_metadata_enabled && !config.games_launchbox_enabled {
            return;
        }
        self.prewarm(config, snap);
        if config.games_launchbox_enabled {
            self.inner
                .launchbox
                .settled()
                .await;
        }
        let started = Instant::now();
        let deadline = || tokio::time::Instant::now() + WARM_WAIT;
        futures_util::stream::iter(
            snap.games
                .iter(),
        )
        .for_each_concurrent(WARM_CONCURRENCY, |game| async move {
            let _ = self
                .get_until(config, game, deadline())
                .await;
        })
        .await;
        debug!(
            games = snap
                .games
                .len(),
            secs = started
                .elapsed()
                .as_secs(),
            "games: metadata warmed"
        );
    }

    /// Whatever is known about `game` right now. Work that is not finished
    /// within [`REQUEST_WAIT`] completes in the background; fields that are
    /// not resolved are simply absent.
    ///
    /// Precedence: a libretro database match supplies its fields as they are
    /// (CRC matches are exact); LaunchBox only fills what is missing, and is
    /// the sole source of `overview` and `rating`.
    pub async fn get(&self, config: &Config, game: &Game) -> GameMeta {
        self.get_until(config, game, tokio::time::Instant::now() + REQUEST_WAIT)
            .await
    }

    /// [`get`](Self::get) with an explicit deadline.
    async fn get_until(
        &self,
        config: &Config,
        game: &Game,
        deadline: tokio::time::Instant,
    ) -> GameMeta {
        let rdb_on = config.games_metadata_enabled;
        let lb_on = config.games_launchbox_enabled;
        if !rdb_on && !lb_on {
            return GameMeta::default();
        }
        let platforms = thumbs::platforms(game.core, game.served_file_name());
        if platforms.is_empty() {
            return GameMeta::default();
        }
        let st = Settings::new(config);

        let (mut meta, canonical) = if rdb_on {
            match self
                .rdb_lookup(&st, game, platforms, deadline)
                .await
            {
                Ok(found) => found.unwrap_or_default(),
                // Still hashing: answer with nothing rather than guess by
                // name while an exact match may be on its way.
                Err(Pending) => return GameMeta::default(),
            }
        } else {
            Default::default()
        };

        if lb_on {
            let names = launchbox::name_candidates(
                canonical.as_deref(),
                &game.title,
                [
                    game.served_file_name(),
                    game.file_name
                        .as_str(),
                ],
            );
            if let Some(lb) = self
                .inner
                .launchbox
                .lookup(config, game.core, &names, deadline)
                .await
            {
                meta.fill_from(&lb);
            }
        }
        meta
    }

    /// The libretro match for a game and its canonical name; `Err` while a
    /// CRC that may still produce an exact match is being computed.
    async fn rdb_lookup(
        &self,
        st: &Settings,
        game: &Game,
        platforms: &'static [&'static str],
        deadline: tokio::time::Instant,
    ) -> Result<Option<(GameMeta, Option<String>)>, Pending> {
        // Start everything first so the pieces run concurrently.
        let crcs = self.crc_state(st, game);
        let indexes: Vec<IndexState> = platforms
            .iter()
            .map(|p| self.ensure_index(st, p))
            .collect();

        let crcs: Vec<u32> = match crcs {
            CrcState::Known(v) => v,
            CrcState::Unavailable => Vec::new(),
            CrcState::Pending(fut) => {
                match tokio::time::timeout_at(deadline, fut).await {
                    Ok(Some(v)) => v
                        .as_ref()
                        .clone(),
                    Ok(None) => Vec::new(),
                    Err(_) => return Err(Pending),
                }
            }
        };
        let names = rdb::name_candidates(
            [
                game.served_file_name(),
                game.file_name
                    .as_str(),
            ],
            &game.title,
        );
        for state in indexes {
            let index = match state {
                IndexState::Ready(i) => i,
                IndexState::Pending(fut) => {
                    match tokio::time::timeout_at(deadline, fut).await {
                        Ok(Some(i)) => i,
                        _ => continue,
                    }
                }
                IndexState::Unavailable => continue,
            };
            // Prefix guessing is only for games with no CRC to go by.
            if let Some(m) = index.lookup_named(&crcs, &names, crcs.is_empty()) {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    // -- databases ---------------------------------------------------------

    fn ensure_index(&self, st: &Settings, platform: &'static str) -> IndexState {
        let now = Instant::now();
        let mut slots = lock(
            &self
                .inner
                .slots,
        );
        match slots.get_mut(platform) {
            Some(Slot::Ready { index, next_check }) => {
                let index = index.clone();
                if now >= *next_check {
                    // Revalidate in the background; keep serving this index.
                    *next_check = now + st.retry;
                    drop(slots);
                    let _ = self.spawn_load(st.clone(), platform, Some(index.clone()));
                }
                IndexState::Ready(index)
            }
            Some(Slot::Loading(fut)) => IndexState::Pending(fut.clone()),
            Some(Slot::Failed { until }) if now < *until => IndexState::Unavailable,
            _ => {
                let fut =
                    self.spawn_load_locked(&mut slots, st.clone(), platform, None);
                IndexState::Pending(fut)
            }
        }
    }

    fn spawn_load(
        &self,
        st: Settings,
        platform: &'static str,
        previous: Option<Arc<RdbIndex>>,
    ) -> SharedIndex {
        let mut slots = lock(
            &self
                .inner
                .slots,
        );
        self.spawn_load_locked(&mut slots, st, platform, previous)
    }

    /// Starts the load task and records it as the slot's state. A refresh
    /// (`previous` set) leaves the old index in place until the new one is
    /// ready and keeps it if the refresh fails outright.
    fn spawn_load_locked(
        &self,
        slots: &mut HashMap<&'static str, Slot>,
        st: Settings,
        platform: &'static str,
        previous: Option<Arc<RdbIndex>>,
    ) -> SharedIndex {
        let inner = self
            .inner
            .clone();
        let keep = previous.clone();
        let handle = tokio::spawn(async move {
            let mut guard = LoadGuard {
                inner: inner.clone(),
                platform,
                keep: keep.clone(),
                retry: st.retry,
                armed: true,
            };
            let out = load_platform(&inner, &st, platform).await;
            guard.armed = false;
            let now = Instant::now();
            let slot = match out
                .index
                .clone()
                .or(keep)
            {
                Some(index) => Slot::Ready {
                    index,
                    next_check: now + out.next_check_in,
                },
                None => Slot::Failed {
                    until: now + out.next_check_in,
                },
            };
            lock(&inner.slots).insert(platform, slot);
            out.index
        });
        let shared = async move {
            handle
                .await
                .ok()
                .flatten()
        }
        .boxed()
        .shared();
        if previous.is_none() {
            slots.insert(platform, Slot::Loading(shared.clone()));
        }
        shared
    }

    // -- CRCs --------------------------------------------------------------

    fn crc_state(&self, st: &Settings, game: &Game) -> CrcState {
        if let Some(rom) = &game.inner_rom {
            // The archive index already holds the ROM's CRC: no extraction.
            return match rom.crc32 {
                Some(c) => CrcState::Known(vec![c]),
                None => CrcState::Unavailable,
            };
        }
        let is_archive = game
            .path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                e.eq_ignore_ascii_case("zip") || e.eq_ignore_ascii_case("7z")
            });
        if is_archive || game.size_bytes > st.max_hash_bytes {
            return CrcState::Unavailable;
        }

        // Already hashed (the warm-up did it): answer now. Going through the
        // hash task even for a cache hit would make a warmed request wait on
        // a task switch and, with no time budget, come back empty.
        if let Some(crcs) = cached_crcs(&self.inner, &game.path) {
            return CrcState::Known(
                crcs.as_ref()
                    .clone(),
            );
        }

        let path = game
            .path
            .clone();
        let fut = match self
            .inner
            .crc_inflight
            .entry(path.clone())
        {
            Entry::Occupied(e) => e
                .get()
                .clone(),
            Entry::Vacant(v) => {
                let inner = self
                    .inner
                    .clone();
                let core = game.core;
                let max = st.max_hash_bytes;
                let key = path.clone();
                let handle = tokio::spawn(async move {
                    let out = hash_cached(&inner, &key, core, max).await;
                    inner
                        .crc_inflight
                        .remove(&key);
                    out
                });
                let shared = async move {
                    handle
                        .await
                        .ok()
                        .flatten()
                }
                .boxed()
                .shared();
                v.insert(shared.clone());
                shared
            }
        };
        CrcState::Pending(fut)
    }
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

/// The cached CRCs for `path`, if the file is unchanged since they were
/// computed. One `stat`, no task switch.
fn cached_crcs(inner: &Inner, path: &Path) -> Option<Arc<Vec<u32>>> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()?;
    let (size, cached_mtime, crcs) = lock(&inner.crc_cache)
        .get(path)
        .cloned()?;
    (size == md.len() && cached_mtime == mtime).then_some(crcs)
}

async fn hash_cached(
    inner: &Inner,
    path: &Path,
    core: Core,
    max: u64,
) -> Option<Arc<Vec<u32>>> {
    let _permit = inner
        .hash_permits
        .acquire()
        .await
        .ok()?;
    let md = tokio::fs::metadata(path)
        .await
        .ok()?;
    let (size, mtime) = (
        md.len(),
        md.modified()
            .ok()?,
    );
    if size > max {
        return None;
    }
    if let Some((s, m, crcs)) = lock(&inner.crc_cache)
        .get(path)
        .cloned()
        && s == size
        && m == mtime
    {
        return Some(crcs);
    }
    let p = path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || hash_file(&p, core))
        .await
        .ok()?;
    match result {
        Ok(crcs) => {
            let crcs = Arc::new(crcs);
            let mut cache = lock(&inner.crc_cache);
            if cache.len() >= MAX_CRC_CACHE {
                cache.clear();
            }
            cache.insert(path.to_path_buf(), (size, mtime, crcs.clone()));
            Some(crcs)
        }
        Err(e) => {
            debug!(error = %e, path = %path.display(), "games: hashing ROM failed");
            None
        }
    }
}

/// How many leading bytes a console-specific copier/dump header takes, when
/// the file has one. Databases hash the bare ROM for these systems.
fn header_len(core: Core, size: u64, head: &[u8]) -> Option<u64> {
    match core {
        Core::Nes if size > 16 && head.starts_with(b"NES\x1A") => Some(16),
        Core::Snes if size > 512 && size % 1024 == 512 => Some(512),
        _ => None,
    }
}

/// CRC-32 candidates of a file, best first: the whole file (what the NES
/// database records, header included), then the headerless ROM for systems
/// that carry a removable header.
fn hash_file(path: &Path, core: Core) -> std::io::Result<Vec<u32>> {
    let mut file = std::fs::File::open(path)?;
    let size = file
        .metadata()?
        .len();
    let mut full = crc32fast::Hasher::new();
    let mut bare: Option<(u64, crc32fast::Hasher)> = None;
    let mut buf = vec![0u8; HASH_CHUNK];
    let mut pos = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if pos == 0 {
            bare = header_len(core, size, chunk)
                .map(|skip| (skip, crc32fast::Hasher::new()));
        }
        full.update(chunk);
        if let Some((skip, hasher)) = bare.as_mut() {
            let start = usize::try_from(skip.saturating_sub(pos)).unwrap_or(usize::MAX);
            if let Some(rest) = chunk.get(start..) {
                hasher.update(rest);
            }
        }
        pos += n as u64;
    }
    let mut out = vec![full.finalize()];
    if let Some((_, hasher)) = bare {
        out.push(hasher.finalize());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Database loading
// ---------------------------------------------------------------------------

struct Outcome {
    index: Option<Arc<RdbIndex>>,
    /// Until the next revalidation (success) or retry (failure).
    next_check_in: Duration,
}

fn db_url(base: &str, platform: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode as enc};
    const SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_');
    format!("{}/{}.rdb", base.trim_end_matches('/'), enc(platform, SET))
}

/// Parses and indexes on the blocking pool; `None` when the data is bad.
async fn build_index(bytes: Vec<u8>) -> Option<Arc<RdbIndex>> {
    tokio::task::spawn_blocking(move || match rdb::parse(&bytes) {
        Ok(records) => Some(Arc::new(RdbIndex::build(records))),
        Err(e) => {
            warn!(error = %e, "games: unusable metadata database");
            None
        }
    })
    .await
    .ok()
    .flatten()
}

async fn read_capped(path: &Path, max: usize) -> Option<Vec<u8>> {
    let md = tokio::fs::metadata(path)
        .await
        .ok()?;
    if !md.is_file() || md.len() > max as u64 {
        return None;
    }
    tokio::fs::read(path)
        .await
        .ok()
}

async fn load_file(path: &Path, max: usize) -> Option<Arc<RdbIndex>> {
    build_index(read_capped(path, max).await?).await
}

fn sidecar(path: &Path, ext: &str) -> PathBuf {
    let mut s = path
        .as_os_str()
        .to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

async fn load_platform(inner: &Inner, st: &Settings, platform: &str) -> Outcome {
    #[cfg(test)]
    if inner
        .panic_next_load
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        panic!("injected database load panic");
    }
    let failed = |index| Outcome {
        index,
        next_check_in: st.retry,
    };

    if !st.is_remote() {
        // A local directory of databases (air-gapped hosts): read in place.
        let path = Path::new(&st.base).join(format!("{platform}.rdb"));
        return match load_file(&path, st.max_rdb_bytes).await {
            Some(index) => Outcome {
                index: Some(index),
                next_check_in: st.refresh,
            },
            None => {
                debug!(path = %path.display(), "games: no local metadata database");
                failed(None)
            }
        };
    }

    let path = st
        .dir
        .join(format!("{platform}.rdb"));
    let modified = tokio::fs::metadata(&path)
        .await
        .ok()
        .filter(|m| m.is_file())
        .and_then(|m| {
            m.modified()
                .ok()
        });
    let age = modified.map(|m| {
        SystemTime::now()
            .duration_since(m)
            .unwrap_or_default()
    });

    if let Some(age) = age
        && age < st.refresh
    {
        if let Some(index) = load_file(&path, st.max_rdb_bytes).await {
            return Outcome {
                index: Some(index),
                next_check_in: st.refresh - age,
            };
        }
        warn!(path = %path.display(), "games: discarding unreadable cached metadata database");
        remove_cached(&path).await;
    }
    // A cached file we have not just served from: stale (revalidate it) or
    // unreadable (already removed above, so `age` is stale information).
    let have_cache = tokio::fs::metadata(&path)
        .await
        .is_ok();

    let Ok(_permit) = inner
        .download_permits
        .acquire()
        .await
    else {
        return failed(None);
    };
    let url = db_url(&st.base, platform);
    let validators = if have_cache {
        Validators::read(&path).await
    } else {
        Validators::default()
    };
    match download(&inner.client, &url, &validators, st.max_rdb_bytes).await {
        Download::Fresh(body) => {
            let Some(index) = build_index(
                body.bytes
                    .clone(),
            )
            .await
            else {
                // Upstream served garbage: keep whatever we already had.
                return failed(load_file(&path, st.max_rdb_bytes).await);
            };
            if let Err(e) = store(&path, &body).await {
                warn!(error = %e, path = %path.display(), "games: caching metadata database failed");
            }
            Outcome {
                index: Some(index),
                next_check_in: st.refresh,
            }
        }
        Download::NotModified if have_cache => {
            // Content unchanged: just restart the freshness clock.
            touch(&path).await;
            match load_file(&path, st.max_rdb_bytes).await {
                Some(index) => Outcome {
                    index: Some(index),
                    next_check_in: st.refresh,
                },
                None => {
                    remove_cached(&path).await;
                    failed(None)
                }
            }
        }
        Download::NotModified | Download::Failed => failed(if have_cache {
            // Upstream is down: a stale database beats none.
            load_file(&path, st.max_rdb_bytes).await
        } else {
            None
        }),
    }
}

#[derive(Default)]
struct Validators {
    etag: Option<String>,
    last_modified: Option<String>,
}

impl Validators {
    async fn read(path: &Path) -> Self {
        Self {
            etag: read_sidecar(path, "etag").await,
            last_modified: read_sidecar(path, "lastmod").await,
        }
    }
}

async fn read_sidecar(path: &Path, ext: &str) -> Option<String> {
    tokio::fs::read_to_string(sidecar(path, ext))
        .await
        .ok()
        .map(|s| {
            s.trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
}

struct Body {
    bytes: Vec<u8>,
    validators: Validators,
}

enum Download {
    Fresh(Body),
    NotModified,
    Failed,
}

async fn download(
    client: &reqwest::Client,
    url: &str,
    validators: &Validators,
    max: usize,
) -> Download {
    let mut req = client.get(url);
    if let Some(etag) = &validators.etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    if let Some(lm) = &validators.last_modified {
        req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
    }
    let resp = match req
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, url, "games: metadata database request failed");
            return Download::Failed;
        }
    };
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Download::NotModified;
    }
    if !status.is_success() {
        warn!(%status, url, "games: metadata database upstream error");
        return Download::Failed;
    }
    if resp
        .content_length()
        .is_some_and(|n| n > max as u64)
    {
        warn!(url, "games: metadata database exceeds size cap");
        return Download::Failed;
    }
    let header = |name| {
        resp.headers()
            .get(name)
            .and_then(|v| {
                v.to_str()
                    .ok()
            })
            .map(str::to_string)
    };
    let validators = Validators {
        etag: header(reqwest::header::ETAG),
        last_modified: header(reqwest::header::LAST_MODIFIED),
    };
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream
        .next()
        .await
    {
        let Ok(chunk) = chunk else {
            warn!(url, "games: metadata database download interrupted");
            return Download::Failed;
        };
        if bytes.len() + chunk.len() > max {
            warn!(url, "games: metadata database exceeds size cap");
            return Download::Failed;
        }
        bytes.extend_from_slice(&chunk);
    }
    Download::Fresh(Body { bytes, validators })
}

/// Writes beside the target and renames, so a reader never sees a torn file.
async fn store(path: &Path, body: &Body) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = path.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    tokio::fs::write(&tmp, &body.bytes).await?;
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    for (ext, value) in [
        (
            "etag",
            &body
                .validators
                .etag,
        ),
        (
            "lastmod",
            &body
                .validators
                .last_modified,
        ),
    ] {
        let side = sidecar(path, ext);
        match value {
            Some(v) => {
                let _ = tokio::fs::write(&side, v).await;
            }
            None => {
                let _ = tokio::fs::remove_file(&side).await;
            }
        }
    }
    Ok(())
}

async fn remove_cached(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
    let _ = tokio::fs::remove_file(sidecar(path, "etag")).await;
    let _ = tokio::fs::remove_file(sidecar(path, "lastmod")).await;
}

/// Sets the file's modification time to now without touching its content.
async fn touch(path: &Path) {
    let path = path.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(SystemTime::now()))
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn nes_hash_is_whole_file_then_headerless() {
        let dir = tempfile::tempdir().unwrap();
        let mut rom = b"NES\x1A".to_vec();
        rom.extend([0u8; 12]);
        rom.extend(vec![0xAB; 200_000]); // spans several read chunks
        let p = write(dir.path(), "a.nes", &rom);
        assert_eq!(
            hash_file(&p, Core::Nes).unwrap(),
            vec![crc32fast::hash(&rom), crc32fast::hash(&rom[16..])]
        );
        // No iNES magic: only the whole file; other cores never get a second.
        let p = write(dir.path(), "b.nes", b"plain data here, 20+ bytes");
        assert_eq!(
            hash_file(&p, Core::Nes).unwrap(),
            vec![crc32fast::hash(b"plain data here, 20+ bytes")]
        );
        let p = write(dir.path(), "c.gb", b"NES\x1A0123456789abcdef0123");
        assert_eq!(
            hash_file(&p, Core::Gb)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn snes_copier_header_gives_a_second_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let rom = vec![7u8; 1024 + 512];
        let p = write(dir.path(), "a.sfc", &rom);
        assert_eq!(
            hash_file(&p, Core::Snes).unwrap(),
            vec![crc32fast::hash(&rom), crc32fast::hash(&rom[512..])]
        );
        let plain = vec![7u8; 2048];
        let p = write(dir.path(), "b.sfc", &plain);
        assert_eq!(
            hash_file(&p, Core::Snes)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn download_enforces_the_size_cap() {
        let mock = httpmock::MockServer::start_async().await;
        mock.mock_async(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/big.rdb");
            then.status(200)
                .body(vec![0u8; 500]);
        })
        .await;
        let client = reqwest::Client::new();
        let url = mock.url("/big.rdb");
        assert!(matches!(
            download(&client, &url, &Validators::default(), 100).await,
            Download::Failed
        ));
        assert!(matches!(
            download(&client, &url, &Validators::default(), 1000).await,
            Download::Fresh(_)
        ));
    }

    /// A library with one NES ROM, its libretro database (by CRC) and a
    /// LaunchBox zip, all synthetic, under fresh temp dirs.
    struct WarmFixture {
        _dirs: (tempfile::TempDir, tempfile::TempDir),
        config: Config,
        snap: GamesSnapshot,
    }

    fn warm_fixture() -> WarmFixture {
        use std::io::Write as _;
        let roms = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir(
            roms.path()
                .join("NES"),
        )
        .unwrap();
        let mut rom = b"NES\x1A".to_vec();
        rom.extend_from_slice(&[7u8; 48]);
        write(
            &roms
                .path()
                .join("NES"),
            "Legend of Hero, The (USA).nes",
            &rom,
        );
        let crc = crc32fast::hash(&rom);

        // libretro: read in place from a local directory.
        let rdb_dir = data
            .path()
            .join("rdb");
        std::fs::create_dir(&rdb_dir).unwrap();
        write(
            &rdb_dir,
            "Nintendo - Nintendo Entertainment System.rdb",
            &rdb::test_support::build(&[rdb::test_support::Rec::named(
                "Legend of Hero, The (USA)",
            )
            .crc(crc)
            .genre("RPG")
            .developer("Hero Soft")]),
        );
        // LaunchBox: built from a local zip at warm-up.
        let zip = data
            .path()
            .join("Metadata.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&zip).unwrap());
        w.start_file("Metadata.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(
            b"<LaunchBox><Game><Name>The Legend of Hero</Name>\
              <Platform>Nintendo Entertainment System</Platform>\
              <Overview>A hero sets out.</Overview><MaxPlayers>2</MaxPlayers></Game></LaunchBox>",
        )
        .unwrap();
        w.finish()
            .unwrap();

        let config = Config {
            data_dir: data
                .path()
                .to_path_buf(),
            games_enabled: true,
            games_roots: vec![
                roms.path()
                    .to_string_lossy()
                    .into_owned(),
            ],
            games_metadata_enabled: true,
            games_metadata_rdb_base_url: rdb_dir
                .to_string_lossy()
                .into_owned(),
            games_launchbox_enabled: true,
            games_launchbox_url: zip
                .to_string_lossy()
                .into_owned(),
            ..Default::default()
        };
        let snap = crate::games::scanner::scan(&[roms
            .path()
            .to_path_buf()]);
        assert_eq!(
            snap.games
                .len(),
            1
        );
        WarmFixture {
            _dirs: (roms, data),
            config,
            snap,
        }
    }

    #[tokio::test]
    async fn warm_up_makes_the_first_detail_complete() {
        let fx = warm_fixture();
        let game = &fx
            .snap
            .games[0];

        // A fresh service that has been warmed: even a request with no time
        // to wait at all already finds the rdb fields and the description.
        let svc = MetaService::default();
        svc.warm(&fx.config, &fx.snap)
            .await;
        let meta = svc
            .get_until(&fx.config, game, tokio::time::Instant::now())
            .await;
        assert_eq!(
            meta.genre
                .as_deref(),
            Some("RPG")
        );
        assert_eq!(
            meta.developer
                .as_deref(),
            Some("Hero Soft")
        );
        assert_eq!(
            meta.overview
                .as_deref(),
            Some("A hero sets out.")
        );
        assert_eq!(meta.players, Some(2));
    }

    #[tokio::test]
    async fn without_a_warm_up_the_first_request_is_bare() {
        // The behaviour the warm-up exists to fix: nothing is loaded yet, so
        // a request with no budget gets nothing at all. The deadline is
        // already well past: tokio timers tick in whole milliseconds, so a
        // deadline of "now" can still be pending for up to a tick, and on a
        // loaded machine the local CRC and index loads finish inside it.
        let fx = warm_fixture();
        let svc = MetaService::default();
        let spent = tokio::time::Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("a minute before now is representable");
        let meta = svc
            .get_until(
                &fx.config,
                &fx.snap
                    .games[0],
                spent,
            )
            .await;
        assert!(meta.is_empty());
    }

    #[tokio::test]
    async fn warm_up_does_nothing_when_both_sources_are_off() {
        let mut fx = warm_fixture();
        fx.config
            .games_metadata_enabled = false;
        fx.config
            .games_launchbox_enabled = false;
        let svc = MetaService::default();
        svc.warm(&fx.config, &fx.snap)
            .await;
        assert!(
            lock(
                &svc.inner
                    .slots
            )
            .is_empty()
        );
    }

    #[test]
    fn db_url_escapes_platform_names() {
        assert_eq!(
            db_url("https://x/rdb/", "Sega - Mega Drive - Genesis"),
            "https://x/rdb/Sega%20-%20Mega%20Drive%20-%20Genesis.rdb"
        );
    }

    #[tokio::test]
    async fn a_panicking_database_load_recovers_on_the_next_call() {
        let mut fx = warm_fixture();
        fx.config
            .games_launchbox_enabled = false;
        let game = &fx
            .snap
            .games[0];
        let svc = MetaService::default();

        svc.inner
            .panic_next_load
            .store(true, std::sync::atomic::Ordering::SeqCst);
        svc.warm(&fx.config, &fx.snap)
            .await;
        {
            // The slot recorded a failure (backoff) instead of staying
            // `Loading`; let the backoff lapse without waiting for it.
            let mut slots = lock(
                &svc.inner
                    .slots,
            );
            assert!(!slots.is_empty());
            for slot in slots.values_mut() {
                assert!(
                    matches!(slot, Slot::Failed { .. }),
                    "a dead load must not leave the slot loading"
                );
                *slot = Slot::Failed {
                    until: Instant::now(),
                };
            }
        }

        // The next call loads the database for real.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let meta = svc
            .get_until(&fx.config, game, deadline)
            .await;
        assert_eq!(
            meta.genre
                .as_deref(),
            Some("RPG")
        );
    }
}
