//! Opt-in game descriptions and ratings from the LaunchBox Games Database.
//!
//! `Metadata.zip` (~108 MB, ~515 MB of XML inside) is downloaded in the
//! background to a temporary file, its `Metadata.xml` is pull-parsed as a
//! stream (never held in memory) keeping only the systems the library has
//! games for, and the result is reduced to one compact JSON file per core
//! under `<data_dir>/games/meta/launchbox/gen-<id>/`. A small `stamp.json`
//! names the live generation, the cores it covers and the HTTP validators, so
//! a restart reads a few megabytes of JSON instead of reparsing the archive,
//! and a rebuild swaps in atomically (the old generation keeps serving until
//! the stamp is replaced).
//!
//! Nothing here runs on the request path beyond a short wait for an already
//! cached core file: building happens on the blocking pool, single-flight,
//! failures are logged and backed off, and a detail without a description
//! simply lacks the field until the data is ready.
//!
//! The data is LaunchBox's (<https://gamesdb.launchbox-app.com>); no terms are
//! published for the download, so this is off by default, nothing is bundled
//! or redistributed, and operators using it are responsible for LaunchBox's
//! terms.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::{
    FutureExt, StreamExt,
    future::{BoxFuture, Shared},
};
use quick_xml::{Reader, events::Event};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};
use uuid::Uuid;

use super::{
    rdb::{GameMeta, Rating},
    resolver::Core,
};
use crate::Config;

/// Entry inside the archive that holds the games.
const XML_ENTRY: &str = "Metadata.xml";
/// Largest download accepted.
const MAX_ZIP_BYTES: u64 = 1024 * 1024 * 1024;
/// Largest inflated `Metadata.xml` accepted (the real one is ~515 MB).
const MAX_XML_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Whole-download timeout (the source is slow and big).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Longest pause between attempts after repeated failures.
const MAX_BACKOFF: Duration = Duration::from_secs(24 * 3600);
const MAX_OVERVIEW_BYTES: usize = 16 * 1024;
const MAX_FIELD_BYTES: usize = 1024;
const MIN_PREFIX_LEN: usize = 5;
const MAX_PREFIX_GAP: usize = 20;
const STAMP_FILE: &str = "stamp.json";
const STAMP_VERSION: u32 = 1;

/// LaunchBox platform names that fold into each core (Moonbase's table).
/// Cores without an entry (arcade) have no LaunchBox data.
fn platform_names(core: Core) -> &'static [&'static str] {
    match core {
        Core::Nes => &["Nintendo Entertainment System"],
        Core::Snes => &["Super Nintendo Entertainment System"],
        Core::Gb => &["Nintendo Game Boy", "Nintendo Game Boy Color"],
        Core::Gba => &["Nintendo Game Boy Advance"],
        Core::N64 => &["Nintendo 64"],
        Core::Nds => &["Nintendo DS"],
        Core::Vb => &["Nintendo Virtual Boy"],
        Core::SegaMd => &["Sega Genesis"],
        Core::SegaMs => &["Sega Master System"],
        Core::SegaGg => &["Sega Game Gear"],
        Core::Atari2600 => &["Atari 2600"],
        Core::Atari7800 => &["Atari 7800"],
        Core::Lynx => &["Atari Lynx"],
        Core::Ws => &["WonderSwan", "WonderSwan Color"],
        Core::Ngp => &["SNK Neo Geo Pocket", "SNK Neo Geo Pocket Color"],
        Core::Pce => &["NEC TurboGrafx-16"],
        Core::Psx => &["Sony Playstation"],
        Core::Psp => &["Sony PSP"],
        Core::Mame | Core::Arcade => &[],
    }
}

/// Whether LaunchBox has anything for this core.
pub fn supports(core: Core) -> bool {
    !platform_names(core).is_empty()
}

fn core_for_platform(wanted: &HashSet<Core>, platform: &str) -> Option<Core> {
    wanted
        .iter()
        .copied()
        .find(|c| platform_names(*c).contains(&platform))
}

// ---------------------------------------------------------------------------
// Records and name keys
// ---------------------------------------------------------------------------

/// What is kept per game.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LbRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub players: Option<u8>,
    /// Hundredths on the 0 to 5 scale; only set with at least one vote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u16>,
}

impl LbRecord {
    fn to_meta(&self) -> GameMeta {
        GameMeta {
            genre: self
                .genre
                .clone(),
            developer: self
                .developer
                .clone(),
            publisher: self
                .publisher
                .clone(),
            year: self.year,
            players: self.players,
            overview: self
                .overview
                .clone(),
            rating: self
                .rating
                .and_then(Rating::from_hundredths),
            ..Default::default()
        }
    }
}

/// One core's games keyed by [`normalize_name`].
pub type LbMap = HashMap<String, LbRecord>;

/// Matching key: letters and digits only, lowercase, accents dropped and
/// anything inside `()` or `[]` removed, so a No-Intro file name and a clean
/// LaunchBox title collapse to the same key.
pub fn normalize_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut depth = 0usize;
    for ch in value
        .nfd()
        .filter(|c| !is_combining_mark(*c))
    {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            c if depth == 0 && c.is_alphanumeric() => {
                out.extend(c.to_lowercase());
            }
            _ => {}
        }
    }
    out
}

/// Match keys for a game, best first and de-duplicated: the database's own
/// canonical name (when the other database matched), the catalog title, then
/// the file stems.
pub fn name_candidates<'a>(
    canonical: Option<&str>,
    title: &str,
    stems: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let stems = stems
        .into_iter()
        .map(|s| {
            Path::new(s)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(s)
        });
    let mut out: Vec<String> = Vec::new();
    for raw in canonical
        .into_iter()
        .chain(std::iter::once(title))
        .chain(stems)
    {
        let n = normalize_name(raw);
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// Exact key first (over every candidate), then the unambiguous closest
/// prefix relative of a key (at least 5 characters, length gap at most 20).
pub fn lookup<'a>(map: &'a LbMap, names: &[String]) -> Option<&'a LbRecord> {
    if let Some(r) = names
        .iter()
        .find_map(|n| map.get(n))
    {
        return Some(r);
    }
    names
        .iter()
        .find_map(|n| prefix_match(map, n))
}

fn prefix_match<'a>(map: &'a LbMap, name: &str) -> Option<&'a LbRecord> {
    if name.len() < MIN_PREFIX_LEN {
        return None;
    }
    let mut best: Option<(usize, &String, &LbRecord)> = None;
    let mut tie = false;
    for (key, rec) in map {
        if !(key.starts_with(name) || name.starts_with(key.as_str())) {
            continue;
        }
        let gap = key
            .len()
            .abs_diff(name.len());
        if gap > MAX_PREFIX_GAP {
            continue;
        }
        match best {
            Some((g, _, _)) if gap > g => {}
            Some((g, k, _)) if gap == g => {
                if k != key {
                    tie = true;
                }
            }
            _ => {
                best = Some((gap, key, rec));
                tie = false;
            }
        }
    }
    best.filter(|_| !tie)
        .map(|(_, _, r)| r)
}

// ---------------------------------------------------------------------------
// Streaming build
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("xml: {0}")]
    Xml(String),
    #[error("`{XML_ENTRY}` is larger than the allowed size")]
    TooLarge,
    #[error("`{XML_ENTRY}` ends inside an element")]
    Truncated,
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Size limits for [`build`] (parameterised so tests can use tiny ones).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_xml_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_xml_bytes: MAX_XML_BYTES,
        }
    }
}

/// Fails with `TooLarge` once more than `remaining` bytes have been read.
struct Capped<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let ask = usize::try_from(
            self.remaining
                .saturating_add(1),
        )
        .unwrap_or(usize::MAX)
        .min(buf.len());
        let n = self
            .inner
            .read(&mut buf[..ask])?;
        if n as u64 > self.remaining {
            return Err(std::io::Error::other("inflated size over the limit"));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Platform,
    Name,
    Overview,
    Genres,
    Developer,
    Publisher,
    ReleaseYear,
    ReleaseDate,
    MaxPlayers,
    Rating,
    RatingCount,
}

impl Field {
    fn of(tag: &[u8]) -> Option<Self> {
        Some(match tag {
            b"Platform" => Self::Platform,
            b"Name" => Self::Name,
            b"Overview" => Self::Overview,
            b"Genres" => Self::Genres,
            b"Developer" => Self::Developer,
            b"Publisher" => Self::Publisher,
            b"ReleaseYear" => Self::ReleaseYear,
            b"ReleaseDate" => Self::ReleaseDate,
            b"MaxPlayers" => Self::MaxPlayers,
            b"CommunityRating" => Self::Rating,
            b"CommunityRatingCount" => Self::RatingCount,
            _ => return None,
        })
    }

    fn cap(self) -> usize {
        match self {
            Self::Overview => MAX_OVERVIEW_BYTES,
            _ => MAX_FIELD_BYTES,
        }
    }
}

/// The fields of one `<Game>` as raw text.
#[derive(Default)]
struct Raw {
    platform: String,
    name: String,
    overview: String,
    genres: String,
    developer: String,
    publisher: String,
    release_year: String,
    release_date: String,
    max_players: String,
    rating: String,
    rating_count: String,
}

impl Raw {
    fn slot(&mut self, f: Field) -> &mut String {
        match f {
            Field::Platform => &mut self.platform,
            Field::Name => &mut self.name,
            Field::Overview => &mut self.overview,
            Field::Genres => &mut self.genres,
            Field::Developer => &mut self.developer,
            Field::Publisher => &mut self.publisher,
            Field::ReleaseYear => &mut self.release_year,
            Field::ReleaseDate => &mut self.release_date,
            Field::MaxPlayers => &mut self.max_players,
            Field::Rating => &mut self.rating,
            Field::RatingCount => &mut self.rating_count,
        }
    }

    fn votes(&self) -> i64 {
        self.rating_count
            .trim()
            .parse()
            .unwrap_or(0)
    }

    fn record(&self) -> LbRecord {
        fn text(s: &str) -> Option<String> {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        let year = self
            .release_year
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|y| (1900..=2100).contains(y))
            .or_else(|| {
                self.release_date
                    .trim()
                    .get(..4)
                    .and_then(|y| {
                        y.parse::<u16>()
                            .ok()
                    })
                    .filter(|y| (1900..=2100).contains(y))
            });
        LbRecord {
            overview: text(&self.overview),
            genre: text(&self.genres),
            developer: text(&self.developer),
            publisher: text(&self.publisher),
            year,
            players: self
                .max_players
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|p| *p > 0),
            rating: (self.votes() > 0)
                .then(|| {
                    self.rating
                        .trim()
                        .parse::<f64>()
                        .ok()
                })
                .flatten()
                .and_then(Rating::parse)
                .map(Rating::hundredths),
        }
    }
}

fn push_capped(dst: &mut String, s: &str, cap: usize) {
    let Some(room) = cap.checked_sub(dst.len()) else {
        return;
    };
    if s.len() <= room {
        dst.push_str(s);
        return;
    }
    let mut end = room;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    dst.push_str(&s[..end]);
}

/// Pull-parses `<Game>` elements from `src`, keeping those of the `wanted`
/// cores. Per core and name key the entry with the most votes wins, so the
/// canonical release beats hacks and re-releases (ties keep the first).
pub fn parse_games<R: std::io::BufRead>(
    src: R,
    wanted: &HashSet<Core>,
) -> Result<HashMap<Core, LbMap>, BuildError> {
    let mut reader = Reader::from_reader(src);
    let mut buf = Vec::new();
    let mut out: HashMap<Core, HashMap<String, (LbRecord, i64)>> = wanted
        .iter()
        .map(|c| (*c, HashMap::new()))
        .collect();

    let mut depth = 0usize;
    let mut game: Option<Raw> = None;
    // Depth inside the current game (0 = directly in `<Game>`) and the field
    // being read, if any.
    let mut inner = 0usize;
    let mut field: Option<Field> = None;

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| BuildError::Xml(e.to_string()))?;
        match event {
            Event::Start(e) => {
                depth += 1;
                match game.as_mut() {
                    None if e
                        .name()
                        .as_ref()
                        == b"Game" =>
                    {
                        game = Some(Raw::default());
                        inner = 0;
                        field = None;
                    }
                    Some(_) => {
                        if inner == 0 {
                            field = Field::of(
                                e.name()
                                    .as_ref(),
                            );
                        }
                        inner += 1;
                    }
                    None => {}
                }
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if game.is_some() {
                    if inner == 0 {
                        // `</Game>`
                        let raw = game
                            .take()
                            .expect("game is set");
                        keep(&mut out, wanted, &raw);
                    } else {
                        inner -= 1;
                        if inner == 0 {
                            field = None;
                        }
                    }
                }
            }
            Event::Text(t) => {
                if let (Some(raw), Some(f), 1) = (game.as_mut(), field, inner) {
                    match t.unescape() {
                        Ok(s) => push_capped(raw.slot(f), &s, f.cap()),
                        // An entity XML does not define (`&nbsp;`): keep the
                        // text as written rather than lose the description.
                        Err(_) => push_capped(
                            raw.slot(f),
                            &String::from_utf8_lossy(&t),
                            f.cap(),
                        ),
                    }
                }
            }
            Event::CData(c) => {
                if let (Some(raw), Some(f), 1) = (game.as_mut(), field, inner) {
                    push_capped(raw.slot(f), &String::from_utf8_lossy(&c), f.cap());
                }
            }
            Event::Eof => {
                if depth != 0 {
                    return Err(BuildError::Truncated);
                }
                break;
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out
        .into_iter()
        .map(|(c, m)| {
            (
                c,
                m.into_iter()
                    .map(|(k, (r, _))| (k, r))
                    .collect(),
            )
        })
        .collect())
}

fn keep(
    out: &mut HashMap<Core, HashMap<String, (LbRecord, i64)>>,
    wanted: &HashSet<Core>,
    raw: &Raw,
) {
    let Some(core) = core_for_platform(
        wanted,
        raw.platform
            .trim(),
    ) else {
        return;
    };
    let key = normalize_name(&raw.name);
    if key.is_empty() {
        return;
    }
    let votes = raw.votes();
    let map = out
        .entry(core)
        .or_default();
    if map
        .get(&key)
        .is_some_and(|(_, v)| *v >= votes)
    {
        return;
    }
    map.insert(key, (raw.record(), votes));
}

/// Reads `Metadata.xml` out of the zip at `zip_path` as a stream and writes
/// one `<core>.json` per wanted core (possibly `{}`) into `out_dir`.
pub fn build(
    zip_path: &Path,
    out_dir: &Path,
    wanted: &HashSet<Core>,
    limits: Limits,
) -> Result<usize, BuildError> {
    let mut archive = zip::ZipArchive::new(BufReader::new(File::open(zip_path)?))?;
    let entry = archive.by_name(XML_ENTRY)?;
    if entry.size() > limits.max_xml_bytes {
        return Err(BuildError::TooLarge);
    }
    let capped = Capped {
        inner: entry,
        remaining: limits.max_xml_bytes,
    };
    let maps = parse_games(BufReader::with_capacity(256 * 1024, capped), wanted)
        .map_err(|e| match e {
            BuildError::Io(io)
                if io
                    .to_string()
                    .contains("over the limit") =>
            {
                BuildError::TooLarge
            }
            BuildError::Xml(m) if m.contains("over the limit") => BuildError::TooLarge,
            other => other,
        })?;
    std::fs::create_dir_all(out_dir)?;
    let mut total = 0;
    for (core, map) in &maps {
        total += map.len();
        let path = out_dir.join(format!("{core}.json"));
        let mut w = std::io::BufWriter::new(File::create(path)?);
        serde_json::to_writer(&mut w, map)?;
        std::io::Write::flush(&mut w)?;
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Stamp and service
// ---------------------------------------------------------------------------

/// Marks a complete, usable generation on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stamp {
    version: u32,
    generation: String,
    cores: Vec<String>,
    /// Where it was built from (a changed setting forces a rebuild).
    source: String,
    /// `ETag` for downloads, size and mtime for a local file.
    etag: Option<String>,
    last_modified: Option<String>,
    /// Last time the source was confirmed current (unix seconds).
    checked_at: u64,
}

impl Stamp {
    fn covers(&self, core: Core) -> bool {
        let key = core.to_string();
        self.cores
            .iter()
            .any(|c| *c == key)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
struct Settings {
    url: String,
    dir: PathBuf,
    refresh: Duration,
    retry: Duration,
}

impl Settings {
    fn new(config: &Config) -> Self {
        Self {
            url: config
                .games_launchbox_url
                .trim()
                .to_string(),
            dir: config
                .data_dir
                .join("games")
                .join("meta")
                .join("launchbox"),
            refresh: Duration::from_secs(
                u64::from(
                    config
                        .games_launchbox_refresh_days
                        .max(1),
                ) * 24
                    * 3600,
            ),
            retry: Duration::from_secs(
                config
                    .games_metadata_retry_secs
                    .max(1),
            ),
        }
    }

    fn is_remote(&self) -> bool {
        let u = self
            .url
            .to_ascii_lowercase();
        u.starts_with("http://") || u.starts_with("https://")
    }

    fn local_path(&self) -> PathBuf {
        PathBuf::from(
            self.url
                .strip_prefix("file://")
                .unwrap_or(&self.url),
        )
    }
}

enum StampSlot {
    Unread,
    Known(Option<Arc<Stamp>>),
}

#[derive(Default)]
struct Schedule {
    running: bool,
    /// Do not look at the source again before this (success path).
    next_check: Option<Instant>,
    /// Do not retry before this (failure path).
    retry_at: Option<Instant>,
    failures: u32,
}

type SharedMap = Shared<BoxFuture<'static, Option<Arc<LbMap>>>>;

enum MapSlot {
    Loading {
        generation: String,
        fut: SharedMap,
    },
    Ready {
        generation: String,
        map: Arc<LbMap>,
    },
    /// The generation's file is unusable; do not retry until it changes.
    Missing {
        generation: String,
    },
}

struct Inner {
    stamp: Mutex<StampSlot>,
    schedule: Mutex<Schedule>,
    maps: Mutex<HashMap<Core, MapSlot>>,
}

/// LaunchBox lookups and the background build. Cheap to clone.
#[derive(Clone)]
pub struct LaunchBox {
    inner: Arc<Inner>,
}

impl Default for LaunchBox {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                stamp: Mutex::new(StampSlot::Unread),
                schedule: Mutex::new(Schedule::default()),
                maps: Mutex::new(HashMap::new()),
            }),
        }
    }
}

impl LaunchBox {
    /// Makes sure the data for `cores` is (being) built or refreshed. Cheap
    /// and non-blocking; the work runs in a single background task.
    pub fn prewarm(
        &self,
        client: &reqwest::Client,
        config: &Config,
        cores: &HashSet<Core>,
    ) {
        if !config.games_launchbox_enabled {
            return;
        }
        let st = Settings::new(config);
        if st
            .url
            .is_empty()
        {
            warn!(
                "games: games_launchbox_enabled is set but games_launchbox_url is empty"
            );
            return;
        }
        let wanted: HashSet<Core> = cores
            .iter()
            .copied()
            .filter(|c| supports(*c))
            .collect();
        if wanted.is_empty() {
            return;
        }
        {
            let mut s = lock(
                &self
                    .inner
                    .schedule,
            );
            let now = Instant::now();
            if s.running
                || s.retry_at
                    .is_some_and(|t| now < t)
            {
                return;
            }
            if s.next_check
                .is_some_and(|t| now < t)
                && self.covers_all(&wanted)
            {
                return;
            }
            s.running = true;
        }
        let me = self.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let result = maintain(&me.inner, &client, &st, &wanted).await;
            let mut s = lock(
                &me.inner
                    .schedule,
            );
            s.running = false;
            let now = Instant::now();
            match result {
                Ok(next) => {
                    s.failures = 0;
                    s.retry_at = None;
                    s.next_check = Some(now + next);
                }
                Err(e) => {
                    s.failures = s
                        .failures
                        .saturating_add(1);
                    let factor = 1u32
                        << s.failures
                            .saturating_sub(1)
                            .min(16);
                    let wait = st
                        .retry
                        .saturating_mul(factor)
                        .min(MAX_BACKOFF);
                    s.retry_at = Some(now + wait);
                    warn!(error = %e, retry_in_secs = wait.as_secs(), "games: LaunchBox metadata update failed");
                }
            }
        });
    }

    fn covers_all(&self, wanted: &HashSet<Core>) -> bool {
        match &*lock(
            &self
                .inner
                .stamp,
        ) {
            StampSlot::Known(Some(s)) => wanted
                .iter()
                .all(|c| s.covers(*c)),
            _ => false,
        }
    }

    /// The LaunchBox record for a game, when its core's data is ready within
    /// `deadline`. Never starts a download.
    pub async fn lookup(
        &self,
        config: &Config,
        core: Core,
        names: &[String],
        deadline: tokio::time::Instant,
    ) -> Option<GameMeta> {
        if !config.games_launchbox_enabled || !supports(core) {
            return None;
        }
        let st = Settings::new(config);
        let map = self
            .map_for(&st, core, deadline)
            .await?;
        lookup(&map, names).map(LbRecord::to_meta)
    }

    async fn stamp(&self, st: &Settings) -> Option<Arc<Stamp>> {
        if let StampSlot::Known(s) = &*lock(
            &self
                .inner
                .stamp,
        ) {
            return s.clone();
        }
        let read = read_stamp(&st.dir)
            .await
            .map(Arc::new);
        let mut slot = lock(
            &self
                .inner
                .stamp,
        );
        match &*slot {
            StampSlot::Known(s) => s.clone(),
            StampSlot::Unread => {
                *slot = StampSlot::Known(read.clone());
                read
            }
        }
    }

    async fn map_for(
        &self,
        st: &Settings,
        core: Core,
        deadline: tokio::time::Instant,
    ) -> Option<Arc<LbMap>> {
        let stamp = tokio::time::timeout_at(deadline, self.stamp(st))
            .await
            .ok()??;
        if !stamp.covers(core) {
            return None;
        }
        let fut = {
            let mut maps = lock(
                &self
                    .inner
                    .maps,
            );
            match maps.get(&core) {
                Some(MapSlot::Ready { generation, map })
                    if *generation == stamp.generation =>
                {
                    return Some(map.clone());
                }
                Some(MapSlot::Missing { generation })
                    if *generation == stamp.generation =>
                {
                    return None;
                }
                Some(MapSlot::Loading { generation, fut })
                    if *generation == stamp.generation =>
                {
                    fut.clone()
                }
                _ => {
                    let inner = self
                        .inner
                        .clone();
                    let path = st
                        .dir
                        .join(format!("gen-{}", stamp.generation))
                        .join(format!("{core}.json"));
                    let generation = stamp
                        .generation
                        .clone();
                    let g2 = generation.clone();
                    let handle = tokio::spawn(async move {
                        let loaded = load_map(&path).await;
                        let slot = match &loaded {
                            Some(map) => MapSlot::Ready {
                                generation: g2.clone(),
                                map: map.clone(),
                            },
                            None => MapSlot::Missing {
                                generation: g2.clone(),
                            },
                        };
                        let mut maps = lock(&inner.maps);
                        // Only the load that is still current may publish.
                        if matches!(
                            maps.get(&core),
                            Some(MapSlot::Loading { generation, .. }) if *generation == g2
                        ) {
                            maps.insert(core, slot);
                        }
                        loaded
                    });
                    let fut: SharedMap = async move {
                        handle
                            .await
                            .ok()
                            .flatten()
                    }
                    .boxed()
                    .shared();
                    maps.insert(
                        core,
                        MapSlot::Loading {
                            generation,
                            fut: fut.clone(),
                        },
                    );
                    fut
                }
            }
        };
        tokio::time::timeout_at(deadline, fut)
            .await
            .ok()
            .flatten()
    }
}

async fn load_map(path: &Path) -> Option<Arc<LbMap>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = File::open(&path).ok()?;
        match serde_json::from_reader::<_, LbMap>(BufReader::new(file)) {
            Ok(m) => Some(Arc::new(m)),
            Err(e) => {
                warn!(error = %e, path = %path.display(), "games: unreadable LaunchBox cache file");
                None
            }
        }
    })
    .await
    .ok()
    .flatten()
}

async fn read_stamp(dir: &Path) -> Option<Stamp> {
    let bytes = tokio::fs::read(dir.join(STAMP_FILE))
        .await
        .ok()?;
    let stamp: Stamp = serde_json::from_slice(&bytes).ok()?;
    if stamp.version != STAMP_VERSION {
        return None;
    }
    // A stamp whose generation directory vanished is not usable.
    tokio::fs::metadata(dir.join(format!("gen-{}", stamp.generation)))
        .await
        .ok()?;
    Some(stamp)
}

async fn write_stamp(dir: &Path, stamp: &Stamp) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let tmp = dir.join(format!("{STAMP_FILE}.{}.tmp", Uuid::new_v4().simple()));
    let body = serde_json::to_vec(stamp).map_err(std::io::Error::other)?;
    tokio::fs::write(&tmp, body).await?;
    if let Err(e) = tokio::fs::rename(&tmp, dir.join(STAMP_FILE)).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

/// Removes generations other than `keep` and leftover partial downloads.
async fn clean_up(dir: &Path, keep: &str) {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let keep_name = format!("gen-{keep}");
    while let Ok(Some(e)) = rd
        .next_entry()
        .await
    {
        let name = e
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.starts_with("gen-") && name != keep_name {
            let _ = tokio::fs::remove_dir_all(e.path()).await;
        } else if name.starts_with("download-") {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum UpdateError {
    #[error("download: {0}")]
    Download(String),
    #[error("build: {0}")]
    Build(#[from] BuildError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("build task failed")]
    Join,
}

/// Brings the on-disk data up to date for `wanted`; returns when to look
/// again.
async fn maintain(
    inner: &Inner,
    client: &reqwest::Client,
    st: &Settings,
    wanted: &HashSet<Core>,
) -> Result<Duration, UpdateError> {
    // A stamp built from another source (the setting changed) is not reused.
    let previous = read_stamp(&st.dir)
        .await
        .filter(|s| s.source == st.url);
    let missing = previous
        .as_ref()
        .is_none_or(|s| {
            wanted
                .iter()
                .any(|c| !s.covers(*c))
        });
    // A rebuild covers everything wanted plus what is already there.
    let mut cores: HashSet<Core> = wanted.clone();
    if let Some(p) = &previous {
        cores.extend(
            p.cores
                .iter()
                .filter_map(|c| {
                    c.parse::<Core>()
                        .ok()
                }),
        );
    }

    let stamp = if st.is_remote() {
        update_remote(client, st, previous.as_ref(), missing, &cores).await?
    } else {
        update_local(st, previous.as_ref(), missing, &cores).await?
    };
    let age = Duration::from_secs(unix_now().saturating_sub(stamp.checked_at));
    *lock(&inner.stamp) = StampSlot::Known(Some(Arc::new(stamp)));
    Ok(st
        .refresh
        .saturating_sub(age)
        .max(Duration::from_secs(60)))
}

async fn update_remote(
    client: &reqwest::Client,
    st: &Settings,
    previous: Option<&Stamp>,
    missing: bool,
    cores: &HashSet<Core>,
) -> Result<Stamp, UpdateError> {
    // Only data that already covers everything can be revalidated.
    let current = previous.filter(|_| !missing);
    if let Some(p) = current
        && Duration::from_secs(unix_now().saturating_sub(p.checked_at)) < st.refresh
    {
        return Ok(p.clone());
    }
    let mut req = client
        .get(&st.url)
        .timeout(DOWNLOAD_TIMEOUT);
    if let Some(p) = current {
        if let Some(etag) = &p.etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(lm) = &p.last_modified {
            req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
        }
    }
    let resp = req
        .send()
        .await
        .map_err(|e| UpdateError::Download(e.to_string()))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED
        && let Some(p) = current
    {
        let mut stamp = p.clone();
        stamp.checked_at = unix_now();
        write_stamp(&st.dir, &stamp).await?;
        debug!("games: LaunchBox metadata unchanged upstream");
        return Ok(stamp);
    }
    if !status.is_success() {
        return Err(UpdateError::Download(format!("upstream status {status}")));
    }
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_ZIP_BYTES)
    {
        return Err(UpdateError::Download("download exceeds size cap".into()));
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
    let etag = header(reqwest::header::ETAG);
    let last_modified = header(reqwest::header::LAST_MODIFIED);

    tokio::fs::create_dir_all(&st.dir).await?;
    let part = st
        .dir
        .join(format!("download-{}.zip.part", Uuid::new_v4().simple()));
    let result = async {
        stream_to_file(resp, &part, MAX_ZIP_BYTES).await?;
        build_generation(st, &part, cores, etag, last_modified).await
    }
    .await;
    let _ = tokio::fs::remove_file(&part).await;
    result
}

async fn stream_to_file(
    resp: reqwest::Response,
    path: &Path,
    max: u64,
) -> Result<(), UpdateError> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut total = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream
        .next()
        .await
    {
        let chunk = chunk.map_err(|e| UpdateError::Download(e.to_string()))?;
        total += chunk.len() as u64;
        if total > max {
            return Err(UpdateError::Download("download exceeds size cap".into()));
        }
        file.write_all(&chunk)
            .await?;
    }
    file.flush()
        .await?;
    Ok(())
}

async fn update_local(
    st: &Settings,
    previous: Option<&Stamp>,
    missing: bool,
    cores: &HashSet<Core>,
) -> Result<Stamp, UpdateError> {
    let path = st.local_path();
    let md = tokio::fs::metadata(&path)
        .await
        .map_err(|e| UpdateError::Download(format!("{}: {e}", path.display())))?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|m| {
            m.duration_since(UNIX_EPOCH)
                .ok()
        })
        .map_or(0, |d| d.as_secs());
    let signature = format!("local:{}:{mtime}", md.len());
    if let Some(p) = previous
        && !missing
        && p.etag
            .as_deref()
            == Some(signature.as_str())
    {
        return Ok(p.clone());
    }
    build_generation(st, &path, cores, Some(signature), None).await
}

/// Builds a new generation next to the live one and only then points the
/// stamp at it, so readers never see a half-built index.
async fn build_generation(
    st: &Settings,
    zip: &Path,
    cores: &HashSet<Core>,
    etag: Option<String>,
    last_modified: Option<String>,
) -> Result<Stamp, UpdateError> {
    let generation = Uuid::new_v4()
        .simple()
        .to_string();
    let gen_dir = st
        .dir
        .join(format!("gen-{generation}"));
    let started = Instant::now();
    let built = {
        let (zip, out, cores) = (zip.to_path_buf(), gen_dir.clone(), cores.clone());
        tokio::task::spawn_blocking(move || {
            build(&zip, &out, &cores, Limits::default())
        })
        .await
        .map_err(|_| UpdateError::Join)?
    };
    let games = match built {
        Ok(n) => n,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&gen_dir).await;
            return Err(e.into());
        }
    };
    let mut names: Vec<String> = cores
        .iter()
        .map(ToString::to_string)
        .collect();
    names.sort();
    let stamp = Stamp {
        version: STAMP_VERSION,
        generation: generation.clone(),
        cores: names,
        source: st
            .url
            .clone(),
        etag,
        last_modified,
        checked_at: unix_now(),
    };
    if let Err(e) = write_stamp(&st.dir, &stamp).await {
        let _ = tokio::fs::remove_dir_all(&gen_dir).await;
        return Err(e.into());
    }
    clean_up(&st.dir, &generation).await;
    info!(
        games,
        secs = started
            .elapsed()
            .as_secs(),
        "games: LaunchBox metadata index built"
    );
    Ok(stamp)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn game(platform: &str, name: &str, extra: &str) -> String {
        format!(
            "<Game><Name>{name}</Name><Platform>{platform}</Platform>{extra}</Game>"
        )
    }

    fn doc(games: &[String]) -> String {
        format!(
            "<?xml version=\"1.0\"?><LaunchBox>{}</LaunchBox>",
            games.concat()
        )
    }

    fn wanted(cores: &[Core]) -> HashSet<Core> {
        cores
            .iter()
            .copied()
            .collect()
    }

    fn parse(xml: &str, cores: &[Core]) -> Result<HashMap<Core, LbMap>, BuildError> {
        parse_games(xml.as_bytes(), &wanted(cores))
    }

    fn write_zip(path: &Path, entry: &str, body: &[u8]) {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        w.start_file(
            entry,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        w.write_all(body)
            .unwrap();
        w.finish()
            .unwrap();
    }

    #[test]
    fn keys_drop_tags_accents_and_punctuation() {
        assert_eq!(normalize_name("Tetris (USA) (Rev 1)"), "tetris");
        assert_eq!(normalize_name("Tetris [!]"), "tetris");
        assert_eq!(normalize_name("Pokémon: Red Version"), "pokemonredversion");
        assert_eq!(normalize_name("(unbalanced"), "");
        assert_eq!(normalize_name("a) b"), "ab");
    }

    #[test]
    fn candidates_are_ordered_and_deduplicated() {
        let c = name_candidates(
            Some("Tetris (USA)"),
            "Tetris",
            ["Tetris (USA) (Rev 1).nes", "other.zip"],
        );
        assert_eq!(c, ["tetris", "other"]);
        assert!(name_candidates(None, "(x)", ["[y].nes"]).is_empty());
    }

    #[test]
    fn parses_cdata_entities_and_filters_platforms() {
        let xml = doc(&[
            game(
                "Nintendo Entertainment System",
                "Tetris",
                "<Overview><![CDATA[Blocks <fall> & stack]]> &amp; &lt;clear&gt; &#233;&#x41;</Overview>\
                 <Genres>Puzzle</Genres><ReleaseYear>1989</ReleaseYear>\
                 <ReleaseDate>2001-01-01T00:00:00</ReleaseDate><MaxPlayers>2</MaxPlayers>\
                 <CommunityRating>3.5</CommunityRating><CommunityRatingCount>10</CommunityRatingCount>",
            ),
            game("Nintendo Game Boy Color", "Wario", ""),
            game("Super Nintendo Entertainment System", "Other", ""),
            game("Nintendo Entertainment System", "(nameless)", ""),
            "<GameImage><Name>x</Name></GameImage>".to_string(),
        ]);
        let out = parse(&xml, &[Core::Nes, Core::Gb]).unwrap();
        assert!(!out.contains_key(&Core::Snes));
        let nes = &out[&Core::Nes];
        assert_eq!(nes.len(), 1);
        let t = &nes["tetris"];
        assert_eq!(
            t.overview
                .as_deref(),
            Some("Blocks <fall> & stack & <clear> éA")
        );
        assert_eq!(
            t.genre
                .as_deref(),
            Some("Puzzle")
        );
        // ReleaseYear wins over the date; rating needs votes.
        assert_eq!(t.year, Some(1989));
        assert_eq!(t.players, Some(2));
        assert_eq!(t.rating, Some(350));
        assert!(out[&Core::Gb].contains_key("wario"));
    }

    #[test]
    fn year_falls_back_to_the_release_date_and_junk_is_dropped() {
        let xml = doc(&[game(
            "Nintendo 64",
            "A",
            "<ReleaseYear>0</ReleaseYear><ReleaseDate>1997-06-23</ReleaseDate>\
             <MaxPlayers>0</MaxPlayers><CommunityRating>9.9</CommunityRating>\
             <CommunityRatingCount>5</CommunityRatingCount><Overview>  </Overview>",
        )]);
        let r = &parse(&xml, &[Core::N64]).unwrap()[&Core::N64]["a"];
        assert_eq!(r.year, Some(1997));
        assert_eq!(r.players, None);
        // Out of the 0 to 5 scale: not a rating. Blank text: not an overview.
        assert_eq!(r.rating, None);
        assert_eq!(r.overview, None);
    }

    #[test]
    fn undefined_entities_keep_the_text() {
        let xml = doc(&[game(
            "Nintendo 64",
            "A",
            "<Overview>Fast&nbsp;car</Overview>",
        )]);
        let r = &parse(&xml, &[Core::N64]).unwrap()[&Core::N64]["a"];
        assert!(
            r.overview
                .as_deref()
                .unwrap()
                .contains("Fast")
        );
    }

    #[test]
    fn most_voted_duplicate_wins_in_either_order_and_ties_keep_the_first() {
        let low = game(
            "Nintendo 64",
            "Mario Kart 64",
            "<Overview>hack</Overview><CommunityRatingCount>2</CommunityRatingCount>",
        );
        let high = game(
            "Nintendo 64",
            "Mario Kart 64 (USA)",
            "<Overview>real</Overview><CommunityRatingCount>50</CommunityRatingCount>",
        );
        for order in [[low.clone(), high.clone()], [high.clone(), low.clone()]] {
            let out = parse(&doc(&order), &[Core::N64]).unwrap();
            assert_eq!(
                out[&Core::N64]["mariokart64"]
                    .overview
                    .as_deref(),
                Some("real")
            );
        }
        let a = game("Nintendo 64", "Same", "<Overview>first</Overview>");
        let b = game("Nintendo 64", "Same", "<Overview>second</Overview>");
        let out = parse(&doc(&[a, b]), &[Core::N64]).unwrap();
        assert_eq!(
            out[&Core::N64]["same"]
                .overview
                .as_deref(),
            Some("first")
        );
    }

    #[test]
    fn field_text_is_capped_on_a_char_boundary() {
        let long = "é".repeat(MAX_OVERVIEW_BYTES); // 2 bytes each
        let xml = doc(&[game(
            "Nintendo 64",
            "A",
            &format!("<Overview>{long}</Overview>"),
        )]);
        let r = &parse(&xml, &[Core::N64]).unwrap()[&Core::N64]["a"];
        let o = r
            .overview
            .as_deref()
            .unwrap();
        assert!(o.len() <= MAX_OVERVIEW_BYTES);
        assert!(o.len() >= MAX_OVERVIEW_BYTES - 1);
    }

    #[test]
    fn malformed_and_truncated_xml_are_errors_not_partial_data() {
        assert!(
            parse("<LaunchBox><Game><Name>x</Game></LaunchBox>", &[Core::Nes]).is_err()
        );
        assert!(matches!(
            parse(
                "<LaunchBox><Game><Name>x</Name><Platform>Nintendo 64</Platform></Game>\
                 <Game><Name>y",
                &[Core::N64]
            ),
            Err(BuildError::Truncated)
        ));
        assert!(parse("", &[Core::Nes]).is_ok());
    }

    #[test]
    fn lookup_prefers_exact_then_unambiguous_prefix() {
        let rec = |o: &str| LbRecord {
            overview: Some(o.into()),
            ..Default::default()
        };
        let mut map = LbMap::new();
        map.insert("supermariobros".into(), rec("smb"));
        map.insert("tetris".into(), rec("tetris"));
        map.insert("tetrisattack".into(), rec("attack"));
        map.insert("zeldaone".into(), rec("z1"));
        let names = |s: &[&str]| -> Vec<String> {
            s.iter()
                .map(|s| s.to_string())
                .collect()
        };
        fn over(r: Option<&LbRecord>) -> Option<&str> {
            r.and_then(|r| {
                r.overview
                    .as_deref()
            })
        }
        // Exact beats any prefix relative, whatever the candidate order.
        assert_eq!(
            over(lookup(&map, &names(&["tetrisx", "tetris"]))),
            Some("tetris")
        );
        // Unambiguous prefix (>= 5 chars) within the gap.
        assert_eq!(over(lookup(&map, &names(&["supermario"]))), Some("smb"));
        assert_eq!(over(lookup(&map, &names(&["zelda"]))), Some("z1"));
        // Too short, ambiguous, or too far apart.
        assert_eq!(over(lookup(&map, &names(&["tetr"]))), None);
        // Closest key wins; a tie between different games is no match.
        assert_eq!(over(lookup(&map, &names(&["tetrisa"]))), Some("tetris"));
        map.insert("marioland".into(), rec("land"));
        map.insert("mariokart".into(), rec("kart"));
        assert_eq!(over(lookup(&map, &names(&["mario"]))), None);
        assert_eq!(
            over(lookup(
                &map,
                &names(&["supermariobrosdeluxeeditionwithbonusgames2000"])
            )),
            None
        );
    }

    #[test]
    fn build_writes_one_file_per_wanted_core_and_enforces_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir
            .path()
            .join("m.zip");
        let xml = doc(&[game("Nintendo 64", "A", "<Overview>text</Overview>")]);
        write_zip(&zip, XML_ENTRY, xml.as_bytes());

        let out = dir
            .path()
            .join("out");
        let n = build(
            &zip,
            &out,
            &wanted(&[Core::N64, Core::Nes]),
            Limits::default(),
        )
        .unwrap();
        assert_eq!(n, 1);
        let n64: LbMap =
            serde_json::from_slice(&std::fs::read(out.join("n64.json")).unwrap())
                .unwrap();
        assert_eq!(
            n64["a"]
                .overview
                .as_deref(),
            Some("text")
        );
        // A wanted core with no games still gets a (empty) file.
        let nes: LbMap =
            serde_json::from_slice(&std::fs::read(out.join("nes.json")).unwrap())
                .unwrap();
        assert!(nes.is_empty());

        // Declared size over the cap.
        let tiny = Limits { max_xml_bytes: 10 };
        assert!(matches!(
            build(
                &zip,
                &dir.path()
                    .join("o2"),
                &wanted(&[Core::N64]),
                tiny
            ),
            Err(BuildError::TooLarge)
        ));
    }

    #[test]
    fn build_rejects_missing_entry_and_corrupt_zip() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir
            .path()
            .join("m.zip");
        write_zip(&zip, "Other.xml", b"<LaunchBox/>");
        let out = dir
            .path()
            .join("out");
        let w = wanted(&[Core::Nes]);
        assert!(build(&zip, &out, &w, Limits::default()).is_err());
        std::fs::write(&zip, b"not a zip at all").unwrap();
        assert!(build(&zip, &out, &w, Limits::default()).is_err());
        assert!(
            build(
                &dir.path()
                    .join("absent.zip"),
                &out,
                &w,
                Limits::default()
            )
            .is_err()
        );
        // Nothing usable was written.
        assert!(
            !out.join("nes.json")
                .exists()
        );
    }

    #[test]
    fn inflation_past_the_cap_is_caught_even_if_the_header_lies() {
        // `Capped` is what guards against an entry that inflates beyond its
        // declared size.
        let mut r = Capped {
            inner: std::io::repeat(b'x'),
            remaining: 100,
        };
        let mut sink = Vec::new();
        let err = std::io::copy(&mut r, &mut sink).unwrap_err();
        assert!(
            err.to_string()
                .contains("over the limit")
        );
        let mut ok = Capped {
            inner: &b"12345"[..],
            remaining: 5,
        };
        let mut s = String::new();
        ok.read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "12345");
    }
}
