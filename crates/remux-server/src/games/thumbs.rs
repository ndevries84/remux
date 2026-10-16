//! Game box art from a libretro-style thumbnail server.
//!
//! Layout upstream: `<base>/<Platform>/Named_<Boxarts|Snaps|Titles>/<Name>.png`
//! (<https://docs.libretro.com/guides/roms-playlists-thumbnails/>), keyed on the
//! No-Intro game name with `&*/:`<>?\|"` replaced by `_`. Matching is the
//! subset of RetroArch's "flexible name matching" that makes sense without a
//! metadata database: ROM file stem, then catalog title, then the "short name"
//! (text before the first `(`/`[`), tried on each candidate platform.
//!
//! Results are cached under `<data_dir>/games/thumbs/<core>/<kind>/<game id>.png`.
//! A game with no art anywhere gets an empty `<game id>.miss` marker that
//! suppresses upstream probing for [`MISS_TTL`]. Transient upstream failures
//! (timeouts, 5xx, bad bodies) are never negative-cached on disk; they only
//! back off in memory for [`FAILURE_BACKOFF`] so a down upstream is not
//! hammered.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use dashmap::{DashMap, mapref::entry::Entry};
use futures_util::{
    FutureExt, StreamExt,
    future::{BoxFuture, Shared},
};
use strum_macros::{Display, EnumString};
use tokio::sync::Semaphore;
use tracing::{debug, warn};
use uuid::Uuid;

use super::{Game, resolver::Core};
use crate::Config;

/// How long a confirmed "no art upstream" answer is trusted.
pub const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// How long to stop asking an upstream that just failed transiently.
pub const FAILURE_BACKOFF: Duration = Duration::from_secs(60);
/// How long a request may block on a fetch before answering 503.
pub const REQUEST_BUDGET: Duration = Duration::from_secs(8);
/// Suggested client retry delay for a still-running fetch.
pub const RETRY_AFTER_PENDING: Duration = Duration::from_secs(5);
/// Simultaneous upstream requests across all games (be polite to libretro).
const MAX_CONCURRENT_FETCHES: usize = 4;
/// Largest thumbnail accepted; real ones are well under 2 MB.
const MAX_THUMB_BYTES: usize = 16 * 1024 * 1024;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Which libretro folder to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum ThumbKind {
    Boxart,
    Snap,
    Title,
}

impl ThumbKind {
    /// Lenient parse matching Moonbase: anything unknown means box art.
    pub fn parse_lenient(s: Option<&str>) -> Self {
        s.and_then(|s| {
            s.trim()
                .parse()
                .ok()
        })
        .unwrap_or(Self::Boxart)
    }

    fn folder(self) -> &'static str {
        match self {
            Self::Boxart => "Named_Boxarts",
            Self::Snap => "Named_Snaps",
            Self::Title => "Named_Titles",
        }
    }
}

/// Outcome of a thumbnail lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbResult {
    /// Cached PNG on disk.
    Found(PathBuf),
    /// Confirmed absent upstream (negative-cached).
    Missing,
    /// Not decided yet or upstream trouble; ask again after the delay.
    Retry(Duration),
}

/// libretro platform (= thumbnail folder) names to try for a game, best first.
/// Empty when libretro has no usable folder (arcade sets are keyed on long
/// titles, not the short ROM names found on disk).
pub fn platforms(core: Core, file_name: &str) -> &'static [&'static str] {
    let ext = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match (core, ext.as_deref()) {
        (Core::Gb, Some("gbc")) => {
            &["Nintendo - Game Boy Color", "Nintendo - Game Boy"]
        }
        (Core::SegaMs, Some("sg")) => {
            &["Sega - SG-1000", "Sega - Master System - Mark III"]
        }
        (Core::Nes, _) => &["Nintendo - Nintendo Entertainment System"],
        (Core::Snes, _) => &["Nintendo - Super Nintendo Entertainment System"],
        (Core::Gb, _) => &["Nintendo - Game Boy"],
        (Core::Gba, _) => &["Nintendo - Game Boy Advance"],
        (Core::N64, _) => &["Nintendo - Nintendo 64"],
        (Core::Nds, _) => &["Nintendo - Nintendo DS"],
        (Core::Vb, _) => &["Nintendo - Virtual Boy"],
        (Core::SegaMd, _) => &["Sega - Mega Drive - Genesis"],
        (Core::SegaMs, _) => &["Sega - Master System - Mark III"],
        (Core::SegaGg, _) => &["Sega - Game Gear"],
        (Core::Atari2600, _) => &["Atari - 2600"],
        (Core::Atari7800, _) => &["Atari - 7800"],
        (Core::Lynx, _) => &["Atari - Lynx"],
        (Core::Ws, _) => &["Bandai - WonderSwan"],
        (Core::Ngp, _) => &["SNK - Neo Geo Pocket"],
        (Core::Pce, _) => &["NEC - PC Engine - TurboGrafx 16"],
        (Core::Psx, _) => &["Sony - PlayStation"],
        (Core::Psp, _) => &["Sony - PlayStation Portable"],
        (Core::Mame, _) | (Core::Arcade, _) => &[],
    }
}

/// libretro's thumbnail file-name rule: `&*/:`<>?\|"` become `_`.
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '&' | '*' | '/' | ':' | '`' | '<' | '>' | '?' | '\\' | '|' | '"' => '_',
            c => c,
        })
        .collect()
}

/// Candidate thumbnail names (without extension), best first, de-duplicated:
/// ROM file stem, catalog title, then the short form of the stem.
pub fn candidate_names(game: &Game) -> Vec<String> {
    let stem = Path::new(&game.file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    let short = |s: &str| {
        s.split(['(', '['])
            .next()
            .unwrap_or(s)
            .trim()
            .to_string()
    };
    let raw = [
        stem.clone(),
        game.title
            .trim()
            .to_string(),
        short(&stem),
    ];
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        let name = sanitize_name(&r);
        if !name.is_empty() && !name.contains(char::is_control) && !out.contains(&name)
        {
            out.push(name);
        }
    }
    out
}

fn thumb_url(base: &str, platform: &str, kind: ThumbKind, name: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode as enc};
    const SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_');
    format!(
        "{}/{}/{}/{}.png",
        base.trim_end_matches('/'),
        enc(platform, SET),
        kind.folder(),
        enc(name, SET),
    )
}

/// Ids come from the scan snapshot, but they end up in a file name, so be
/// strict anyway.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

type SharedFetch = Shared<BoxFuture<'static, ThumbResult>>;

struct Inner {
    client: reqwest::Client,
    permits: Semaphore,
    inflight: DashMap<PathBuf, SharedFetch>,
    /// Cache path -> when the last transient failure happened.
    failures: DashMap<PathBuf, Instant>,
}

/// Fetch-and-cache service. Cheap to clone.
#[derive(Clone)]
pub struct ThumbService {
    inner: Arc<Inner>,
}

impl Default for ThumbService {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "Remux/",
                env!("CARGO_PKG_VERSION"),
                " (game thumbnail cache)"
            ))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .expect("failed to build thumbnail client");
        Self {
            inner: Arc::new(Inner {
                client,
                permits: Semaphore::new(MAX_CONCURRENT_FETCHES),
                inflight: DashMap::new(),
                failures: DashMap::new(),
            }),
        }
    }
}

enum Probe {
    Found(Vec<u8>),
    NotFound,
    Transient,
}

impl ThumbService {
    fn cache_path(config: &Config, game: &Game, kind: ThumbKind) -> PathBuf {
        config
            .data_dir
            .join("games")
            .join("thumbs")
            .join(
                game.core
                    .to_string(),
            )
            .join(kind.to_string())
            .join(format!("{}.png", game.id))
    }

    /// Looks up (and if needed fetches) the art for `game`.
    pub async fn get(
        &self,
        config: &Config,
        game: &Game,
        kind: ThumbKind,
    ) -> ThumbResult {
        if !config.games_thumbnails_enabled || !safe_id(&game.id) {
            return ThumbResult::Missing;
        }
        let platforms = platforms(game.core, &game.file_name);
        let names = candidate_names(game);
        if platforms.is_empty() || names.is_empty() {
            return ThumbResult::Missing;
        }

        let png = Self::cache_path(config, game, kind);
        if tokio::fs::metadata(&png)
            .await
            .is_ok_and(|m| m.is_file())
        {
            return ThumbResult::Found(png);
        }
        let miss = png.with_extension("miss");
        if let Ok(modified) = tokio::fs::metadata(&miss)
            .await
            .and_then(|m| m.modified())
        {
            let age = SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default();
            if age < MISS_TTL {
                return ThumbResult::Missing;
            }
            let _ = tokio::fs::remove_file(&miss).await;
        }
        if let Some(at) = self
            .inner
            .failures
            .get(&png)
            .map(|e| *e)
        {
            let elapsed = at.elapsed();
            if elapsed < FAILURE_BACKOFF {
                return ThumbResult::Retry(FAILURE_BACKOFF - elapsed);
            }
        }

        let base = config
            .games_thumbnails_base_url
            .clone();
        let fut = match self
            .inner
            .inflight
            .entry(png.clone())
        {
            Entry::Occupied(e) => e
                .get()
                .clone(),
            Entry::Vacant(v) => {
                let inner = self
                    .inner
                    .clone();
                let key = png.clone();
                let handle = tokio::spawn(async move {
                    let result =
                        fetch(&inner, &base, platforms, &names, kind, &key).await;
                    inner
                        .inflight
                        .remove(&key);
                    result
                });
                let shared = async move {
                    handle
                        .await
                        .unwrap_or(ThumbResult::Retry(RETRY_AFTER_PENDING))
                }
                .boxed()
                .shared();
                v.insert(shared.clone());
                shared
            }
        };
        // The fetch is detached: if this request gives up, it keeps going and
        // fills the cache for the retry.
        tokio::time::timeout(REQUEST_BUDGET, fut)
            .await
            .unwrap_or(ThumbResult::Retry(RETRY_AFTER_PENDING))
    }
}

async fn fetch(
    inner: &Inner,
    base: &str,
    platforms: &[&str],
    names: &[String],
    kind: ThumbKind,
    png: &Path,
) -> ThumbResult {
    let Ok(_permit) = inner
        .permits
        .acquire()
        .await
    else {
        return ThumbResult::Retry(RETRY_AFTER_PENDING);
    };
    // A previous holder of this key may have finished while we queued.
    if tokio::fs::metadata(png)
        .await
        .is_ok()
    {
        return ThumbResult::Found(png.to_path_buf());
    }

    let mut transient = false;
    for platform in platforms {
        for name in names {
            let url = thumb_url(base, platform, kind, name);
            match probe(&inner.client, &url).await {
                Probe::Found(bytes) => match store(png, &bytes).await {
                    Ok(()) => {
                        inner
                            .failures
                            .remove(png);
                        return ThumbResult::Found(png.to_path_buf());
                    }
                    Err(e) => {
                        warn!(error = %e, path = %png.display(), "games: caching thumbnail failed");
                        transient = true;
                    }
                },
                Probe::NotFound => {}
                Probe::Transient => transient = true,
            }
        }
    }

    if transient {
        if inner
            .failures
            .len()
            > 1024
        {
            inner
                .failures
                .retain(|_, at| at.elapsed() < FAILURE_BACKOFF);
        }
        inner
            .failures
            .insert(png.to_path_buf(), Instant::now());
        return ThumbResult::Retry(FAILURE_BACKOFF);
    }
    if let Some(dir) = png.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    if let Err(e) = tokio::fs::write(png.with_extension("miss"), b"").await {
        debug!(error = %e, "games: writing thumbnail miss marker failed");
    }
    ThumbResult::Missing
}

async fn probe(client: &reqwest::Client, url: &str) -> Probe {
    let resp = match client
        .get(url)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, url, "games: thumbnail request failed");
            return Probe::Transient;
        }
    };
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Probe::NotFound;
    }
    if !status.is_success() {
        debug!(%status, url, "games: thumbnail upstream error");
        return Probe::Transient;
    }
    if resp
        .content_length()
        .is_some_and(|n| n as usize > MAX_THUMB_BYTES)
    {
        warn!(url, "games: thumbnail exceeds size cap");
        return Probe::Transient;
    }
    let mut body: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream
        .next()
        .await
    {
        let Ok(chunk) = chunk else {
            return Probe::Transient;
        };
        if body.len() + chunk.len() > MAX_THUMB_BYTES {
            warn!(url, "games: thumbnail exceeds size cap");
            return Probe::Transient;
        }
        body.extend_from_slice(&chunk);
    }
    if !body.starts_with(PNG_MAGIC) {
        debug!(url, "games: thumbnail is not a PNG");
        return Probe::Transient;
    }
    Probe::Found(body)
}

/// Writes beside the target and renames, so readers never see a torn file.
async fn store(png: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = png.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = png.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    tokio::fs::write(&tmp, bytes).await?;
    if let Err(e) = tokio::fs::rename(&tmp, png).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    let _ = tokio::fs::remove_file(png.with_extension("miss")).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(file_name: &str, title: &str, core: Core) -> Game {
        Game {
            id: "abc123".into(),
            title: title.into(),
            system: "X".into(),
            core,
            file_name: file_name.into(),
            path: PathBuf::new(),
            size_bytes: 0,
        }
    }

    #[test]
    fn sanitize_replaces_reserved_characters() {
        assert_eq!(
            sanitize_name("A&B*C/D:E`F<G>H?I\\J|K\"L"),
            "A_B_C_D_E_F_G_H_I_J_K_L"
        );
        assert_eq!(
            sanitize_name("Q*Bert's Qubes (USA)"),
            "Q_Bert's Qubes (USA)"
        );
        assert_eq!(sanitize_name("Plain Name (Europe)"), "Plain Name (Europe)");
    }

    #[test]
    fn candidates_are_stem_title_short_deduped() {
        let g = game(
            "Adventure Island (USA).nes",
            "Adventure Island (USA)",
            Core::Nes,
        );
        assert_eq!(
            candidate_names(&g),
            vec!["Adventure Island (USA)", "Adventure Island"]
        );
        let g = game("crash.7z", "Crash: Bandicoot", Core::Psx);
        assert_eq!(candidate_names(&g), vec!["crash", "Crash_ Bandicoot"]);
        let g = game("Dr. Mario [!].nes", "Dr. Mario [!]", Core::Nes);
        assert_eq!(candidate_names(&g), vec!["Dr. Mario [!]", "Dr. Mario"]);
    }

    #[test]
    fn platform_mapping() {
        let p = |c, f| platforms(c, f);
        assert_eq!(
            p(Core::Nes, "a.nes"),
            ["Nintendo - Nintendo Entertainment System"]
        );
        assert_eq!(
            p(Core::Snes, "a.sfc"),
            ["Nintendo - Super Nintendo Entertainment System"]
        );
        assert_eq!(p(Core::Gb, "a.gb"), ["Nintendo - Game Boy"]);
        assert_eq!(
            p(Core::Gb, "a.GBC"),
            ["Nintendo - Game Boy Color", "Nintendo - Game Boy"]
        );
        assert_eq!(p(Core::Gba, "a.gba"), ["Nintendo - Game Boy Advance"]);
        assert_eq!(p(Core::SegaMd, "a.md"), ["Sega - Mega Drive - Genesis"]);
        assert_eq!(p(Core::Psx, "a.cue"), ["Sony - PlayStation"]);
        assert!(p(Core::Mame, "pacman.zip").is_empty());
    }

    #[test]
    fn url_is_percent_encoded() {
        assert_eq!(
            thumb_url(
                "https://thumbnails.libretro.com/",
                "Nintendo - Nintendo Entertainment System",
                ThumbKind::Boxart,
                "Adventure Island (USA)"
            ),
            "https://thumbnails.libretro.com/Nintendo%20-%20Nintendo%20Entertainment%20System/Named_Boxarts/Adventure%20Island%20%28USA%29.png"
        );
    }

    #[test]
    fn kind_parses_leniently() {
        assert_eq!(ThumbKind::parse_lenient(Some("SNAP")), ThumbKind::Snap);
        assert_eq!(ThumbKind::parse_lenient(Some("title")), ThumbKind::Title);
        assert_eq!(ThumbKind::parse_lenient(Some("wat")), ThumbKind::Boxart);
        assert_eq!(ThumbKind::parse_lenient(None), ThumbKind::Boxart);
    }
}
