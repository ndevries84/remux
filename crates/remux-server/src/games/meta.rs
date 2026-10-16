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
    rdb::{self, GameMeta, MAX_RDB_BYTES, RdbIndex},
    resolver::Core,
    thumbs,
};
use crate::Config;

/// How long a request waits for work that is already in flight (a cached
/// database being parsed, a ROM being hashed). Anything slower finishes in
/// the background and shows up on the next request.
pub const REQUEST_WAIT: Duration = Duration::from_millis(250);
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
            }),
        }
    }
}

impl MetaService {
    /// Starts loading the databases of every system in `snap` (once per
    /// scan) so the first game opened already finds them ready. Never blocks.
    pub fn prewarm(&self, config: &Config, snap: &GamesSnapshot) {
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

    /// Whatever is known about `game` right now. Work that is not finished
    /// within [`REQUEST_WAIT`] completes in the background; fields that are
    /// not resolved are simply absent.
    pub async fn get(&self, config: &Config, game: &Game) -> GameMeta {
        if !config.games_metadata_enabled {
            return GameMeta::default();
        }
        let platforms = thumbs::platforms(game.core, game.served_file_name());
        if platforms.is_empty() {
            return GameMeta::default();
        }
        let st = Settings::new(config);
        let deadline = tokio::time::Instant::now() + REQUEST_WAIT;

        // Start everything first so the pieces run concurrently.
        let crcs = self.crc_state(&st, game);
        let indexes: Vec<IndexState> = platforms
            .iter()
            .map(|p| self.ensure_index(&st, p))
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
                    // Still hashing: answer with nothing rather than guess
                    // by name while an exact match may be on its way.
                    Err(_) => return GameMeta::default(),
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
            if let Some(m) = index.lookup(&crcs, &names, crcs.is_empty()) {
                return m;
            }
        }
        GameMeta::default()
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
            let out = load_platform(&inner, &st, platform).await;
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

    #[test]
    fn db_url_escapes_platform_names() {
        assert_eq!(
            db_url("https://x/rdb/", "Sega - Mega Drive - Genesis"),
            "https://x/rdb/Sega%20-%20Mega%20Drive%20-%20Genesis.rdb"
        );
    }
}
