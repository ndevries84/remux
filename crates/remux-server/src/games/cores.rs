//! Server-installed EmulatorJS bundle (`<data_dir>/games/emulatorjs/data/`).
//!
//! An admin can have the server fetch the pinned EmulatorJS release (or upload
//! the same archive) so the web player no longer depends on the CDN. The
//! pipeline is: stream to a temp file (size cap, sha256 + size verified for a
//! download) -> extract the archive's `data/` folder into a staging directory
//! (path-contained, size capped) -> verify the required files and version ->
//! swap the staging directory into place atomically. A failed attempt leaves
//! any previous install untouched. At most one install runs at a time.

use std::{
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use strum_macros::Display;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{Config, api::moonfin_emulator::EMULATORJS_VERSION};

/// Files without which the self-hosted player cannot start.
pub const REQUIRED_FILES: [&str; 3] =
    ["loader.js", "emulator.min.js", "emulator.min.css"];
/// Written into the data folder by an install; records the EmulatorJS version
/// it holds. A marker that differs from [`EMULATORJS_VERSION`] makes the
/// install stale (the player goes back to the CDN).
pub const VERSION_MARKER: &str = ".remux-emulatorjs-version";
/// Pinned release archive (EmulatorJS `v4.2.3`, a single 7z asset).
pub const DEFAULT_URL: &str =
    "https://github.com/EmulatorJS/EmulatorJS/releases/download/v4.2.3/4.2.3.7z";
pub const DEFAULT_SIZE: u64 = 303_554_683;
pub const DEFAULT_SHA256: &str =
    "07d451bc06fa3ad04ab30d9b94eb63ac34ad0babee52d60357b002bde8f3850b";
const MAX_ENTRIES: u64 = 20_000;
const MAX_VERSION_JSON_BYTES: u64 = 64 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `<data_dir>/games/emulatorjs`.
pub fn base_dir(config: &Config) -> PathBuf {
    config
        .data_dir
        .join("games")
        .join("emulatorjs")
}

/// The folder the player and `/Moonfin/EmulatorJS/data/` serve from.
pub fn data_dir(config: &Config) -> PathBuf {
    base_dir(config).join("data")
}

/// What is on disk in the emulator data folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataState {
    /// Complete bundle of the pinned version (or hand-copied, without marker).
    Installed,
    /// Complete, but marked as another EmulatorJS version (the marker text).
    Stale { found: String },
    /// Required files are missing.
    Missing,
}

/// Classifies `data`: all [`REQUIRED_FILES`] must exist, and the version
/// marker (when present) must match. A folder without a marker (copied in by
/// hand) is accepted.
pub async fn data_state(data: &Path) -> DataState {
    for name in REQUIRED_FILES {
        let is_file = tokio::fs::metadata(data.join(name))
            .await
            .map(|m| m.is_file())
            .unwrap_or(false);
        if !is_file {
            return DataState::Missing;
        }
    }
    match tokio::fs::read_to_string(data.join(VERSION_MARKER)).await {
        Ok(v) if v.trim() == EMULATORJS_VERSION => DataState::Installed,
        Ok(v) => DataState::Stale {
            found: v
                .trim()
                .to_string(),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => DataState::Installed,
        Err(_) => DataState::Stale {
            found: "unreadable".into(),
        },
    }
}

/// True when `data` holds a complete bundle of the pinned version.
pub async fn data_installed(data: &Path) -> bool {
    data_state(data).await == DataState::Installed
}

/// A SHA-256 digest, proven to be 64 hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sha256Digest([u8; 32]);

impl std::str::FromStr for Sha256Digest {
    type Err = CoresError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let bad = || CoresError::Config("sha256 must be 64 hex digits".into());
        if s.len() != 64 || !s.is_ascii() {
            return Err(bad());
        }
        let mut out = [0u8; 32];
        for (i, byte) in out
            .iter_mut()
            .enumerate()
        {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| bad())?;
        }
        Ok(Self(out))
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .try_for_each(|b| write!(f, "{b:02x}"))
    }
}

/// Where the bundle is downloaded from and what it must hash to.
#[derive(Debug, Clone)]
pub struct CoresSource {
    pub url: String,
    pub sha256: Sha256Digest,
    /// Exact byte length the download must have, when known.
    pub size: Option<u64>,
}

impl CoresSource {
    /// The pinned release, or the operator override. An override must come
    /// with its checksum: nothing unverified is ever extracted.
    pub fn from_config(config: &Config) -> Result<Self, CoresError> {
        let url = config
            .games_cores_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let sha = config
            .games_cores_sha256
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match (url, sha) {
            (None, None) => Ok(Self {
                url: DEFAULT_URL.into(),
                sha256: DEFAULT_SHA256.parse()?,
                size: Some(DEFAULT_SIZE),
            }),
            (Some(url), Some(sha)) => Ok(Self {
                url: url.into(),
                sha256: sha.parse()?,
                size: config.games_cores_size,
            }),
            (Some(_), None) => Err(CoresError::Config(
                "games_cores_url requires games_cores_sha256".into(),
            )),
            (None, Some(_)) => Err(CoresError::Config(
                "games_cores_sha256 requires games_cores_url".into(),
            )),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoresError {
    #[error("{0}")]
    Config(String),
    #[error("download failed: {0}")]
    Http(String),
    #[error("download failed: server answered {0}")]
    Status(u16),
    #[error("archive is larger than the {max} byte limit")]
    TooLarge { max: u64 },
    #[error("size mismatch: expected {expected} bytes, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("checksum mismatch")]
    ChecksumMismatch,
    #[error("unsupported archive (expected a 7z or zip file)")]
    UnsupportedArchive,
    #[error("archive is unreadable: {0}")]
    Archive(String),
    #[error("archive contains an unsafe path")]
    UnsafePath,
    #[error("archive holds more than {MAX_ENTRIES} entries")]
    TooManyEntries,
    #[error("archive is missing {0}")]
    Missing(&'static str),
    #[error("bundle is EmulatorJS {found}, expected {expected}")]
    VersionMismatch { found: String, expected: String },
    #[error("disk error: {0}")]
    Io(#[from] io::Error),
    #[error("install task failed")]
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Display, Serialize)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum CoresState {
    #[default]
    Idle,
    Downloading,
    Installed,
    Failed,
}

/// Wire shape of `GET /Moonfin/Games/Cores/Status`: Moonbase's five keys plus
/// additive progress counters that only exist while downloading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoresStatus {
    pub installed: bool,
    pub downloading: bool,
    pub state: CoresState,
    pub error: Option<String>,
    pub files_installed: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_downloaded: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

#[derive(Default, Clone)]
struct Inner {
    state: CoresState,
    error: Option<String>,
    downloaded: u64,
    total: Option<u64>,
}

/// In-memory install state machine (see [`CoresState`]). Cheap to clone.
#[derive(Clone, Default)]
pub struct CoresService {
    inner: Arc<Mutex<Inner>>,
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Exclusive right to run an install. Dropping it unfinished (a panic, a
/// dropped upload request) records a failure instead of leaving the state
/// stuck on `downloading`.
pub struct Claim {
    svc: CoresService,
    previous: Inner,
    done: bool,
}

impl Claim {
    pub fn set_total(&self, total: u64) {
        lock(
            &self
                .svc
                .inner,
        )
        .total = Some(total);
    }

    pub fn set_downloaded(&self, n: u64) {
        lock(
            &self
                .svc
                .inner,
        )
        .downloaded = n;
    }

    pub fn succeed(mut self) {
        self.finish(Inner {
            state: CoresState::Installed,
            ..Default::default()
        });
    }

    pub fn fail(mut self, error: impl fmt::Display) {
        self.finish(Inner {
            state: CoresState::Failed,
            error: Some(error.to_string()),
            ..Default::default()
        });
    }

    /// Gives the slot back as if nothing had been attempted.
    pub fn release(mut self) {
        let previous = self
            .previous
            .clone();
        self.finish(previous);
    }

    fn finish(&mut self, next: Inner) {
        *lock(
            &self
                .svc
                .inner,
        ) = next;
        self.done = true;
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.done {
            self.finish(Inner {
                state: CoresState::Failed,
                error: Some("install was interrupted".into()),
                ..Default::default()
            });
        }
    }
}

struct Limits {
    max_download: u64,
    max_extract: u64,
}

impl Limits {
    fn of(config: &Config) -> Self {
        Self {
            max_download: config.games_cores_max_download_bytes,
            max_extract: config.games_cores_max_extract_bytes,
        }
    }
}

impl CoresService {
    /// Takes the install slot, or `None` when an install is already running.
    pub fn try_begin(&self) -> Option<Claim> {
        let mut g = lock(&self.inner);
        if g.state == CoresState::Downloading {
            return None;
        }
        let previous = g.clone();
        *g = Inner {
            state: CoresState::Downloading,
            ..Default::default()
        };
        Some(Claim {
            svc: self.clone(),
            previous,
            done: false,
        })
    }

    pub async fn status(&self, config: &Config) -> CoresStatus {
        let snapshot = lock(&self.inner).clone();
        let data = data_dir(config);
        let installed = data_installed(&data).await;
        let files_installed = if installed {
            tokio::task::spawn_blocking(move || count_files(&data))
                .await
                .unwrap_or(0)
        } else {
            0
        };
        let downloading = snapshot.state == CoresState::Downloading;
        CoresStatus {
            installed,
            downloading,
            state: snapshot.state,
            error: snapshot.error,
            files_installed,
            bytes_downloaded: downloading.then_some(snapshot.downloaded),
            total_bytes: snapshot
                .total
                .filter(|_| downloading),
        }
    }

    /// Starts downloading the configured source in the background. A no-op
    /// when an install is already running.
    pub fn start_install(&self, config: &Config) {
        let Some(claim) = self.try_begin() else {
            return;
        };
        let config = config.clone();
        tokio::spawn(async move {
            let result = download_and_install(&claim, &config).await;
            conclude(claim, result);
        });
    }

    /// Extracts an uploaded archive (already on disk at `archive`) in the
    /// background. The temp file is removed afterwards.
    pub fn start_extract(&self, claim: Claim, config: &Config, archive: PathBuf) {
        let config = config.clone();
        tokio::spawn(async move {
            let result = install_archive(&archive, &config).await;
            let _ = tokio::fs::remove_file(&archive).await;
            conclude(claim, result);
        });
    }

    /// Removes leftovers of a crashed install and restores a half-swapped
    /// `data` folder. Only call while no install can be running.
    pub async fn cleanup_stale(&self, config: &Config) {
        if lock(&self.inner).state == CoresState::Downloading {
            return;
        }
        let base = base_dir(config);
        let _ = tokio::task::spawn_blocking(move || cleanup_base(&base)).await;
    }
}

fn conclude(claim: Claim, result: Result<u32, CoresError>) {
    match result {
        Ok(files) => {
            info!(files, "games: EmulatorJS bundle installed");
            claim.succeed();
        }
        Err(e) => {
            warn!(error = %e, "games: EmulatorJS install failed");
            claim.fail(e);
        }
    }
}

/// Path for an upload's temp file under `<base>/tmp/` (created if needed).
pub async fn upload_temp_path(config: &Config) -> io::Result<PathBuf> {
    let tmp = base_dir(config).join("tmp");
    tokio::fs::create_dir_all(&tmp).await?;
    Ok(tmp.join(format!("{}.upload", Uuid::new_v4())))
}

pub fn max_upload_bytes(config: &Config) -> u64 {
    config.games_cores_max_download_bytes
}

/// Archive kinds accepted for install, told apart by their magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    SevenZ,
    Zip,
}

impl ArchiveFormat {
    pub fn sniff(head: &[u8]) -> Option<Self> {
        if head.starts_with(&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]) {
            Some(Self::SevenZ)
        } else if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
            Some(Self::Zip)
        } else {
            None
        }
    }
}

async fn download_and_install(
    claim: &Claim,
    config: &Config,
) -> Result<u32, CoresError> {
    let source = CoresSource::from_config(config)?;
    let limits = Limits::of(config);
    let tmp = base_dir(config).join("tmp");
    {
        let base = base_dir(config);
        let _ = tokio::task::spawn_blocking(move || cleanup_base(&base)).await;
    }
    tokio::fs::create_dir_all(&tmp).await?;
    let archive = tmp.join(format!("{}.download", Uuid::new_v4()));
    let result = async {
        download(claim, &source, &archive, limits.max_download).await?;
        install_archive(&archive, config).await
    }
    .await;
    let _ = tokio::fs::remove_file(&archive).await;
    result
}

async fn download(
    claim: &Claim,
    source: &CoresSource,
    dest: &Path,
    max: u64,
) -> Result<(), CoresError> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("Remux/", env!("CARGO_PKG_VERSION"), " (cores)"))
        .connect_timeout(Duration::from_secs(15))
        .timeout(DOWNLOAD_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| {
            CoresError::Http(
                e.without_url()
                    .to_string(),
            )
        })?;
    let resp = client
        .get(&source.url)
        .send()
        .await
        .map_err(|e| {
            CoresError::Http(
                e.without_url()
                    .to_string(),
            )
        })?;
    if !resp
        .status()
        .is_success()
    {
        return Err(CoresError::Status(
            resp.status()
                .as_u16(),
        ));
    }
    if let Some(len) = resp.content_length() {
        if len > max {
            return Err(CoresError::TooLarge { max });
        }
        if let Some(expected) = source.size
            && len != expected
        {
            return Err(CoresError::SizeMismatch {
                expected,
                actual: len,
            });
        }
        claim.set_total(len);
    } else if let Some(expected) = source.size {
        claim.set_total(expected);
    }
    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream
        .next()
        .await
    {
        let chunk = chunk.map_err(|e| {
            CoresError::Http(
                e.without_url()
                    .to_string(),
            )
        })?;
        total += chunk.len() as u64;
        if total > max {
            return Err(CoresError::TooLarge { max });
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await?;
        claim.set_downloaded(total);
    }
    file.flush()
        .await?;
    drop(file);
    if let Some(expected) = source.size
        && total != expected
    {
        return Err(CoresError::SizeMismatch {
            expected,
            actual: total,
        });
    }
    let digest = Sha256Digest(
        hasher
            .finalize()
            .into(),
    );
    if digest != source.sha256 {
        return Err(CoresError::ChecksumMismatch);
    }
    Ok(())
}

/// Extracts `archive` into a fresh staging folder, verifies it and swaps it
/// in as `data/`. Returns the number of files installed.
async fn install_archive(archive: &Path, config: &Config) -> Result<u32, CoresError> {
    let base = base_dir(config);
    let archive = archive.to_path_buf();
    let max_extract = config.games_cores_max_extract_bytes;
    tokio::task::spawn_blocking(move || {
        install_archive_blocking(&base, &archive, max_extract)
    })
    .await
    .map_err(|_| CoresError::Task)?
}

fn install_archive_blocking(
    base: &Path,
    archive: &Path,
    max_extract: u64,
) -> Result<u32, CoresError> {
    let mut head = [0u8; 6];
    let n = File::open(archive)?.read(&mut head)?;
    let format =
        ArchiveFormat::sniff(&head[..n]).ok_or(CoresError::UnsupportedArchive)?;
    fs::create_dir_all(base)?;
    let staging = base.join(format!("data.staging-{}", Uuid::new_v4()));
    fs::create_dir(&staging)?;
    let result = extract_verify(archive, format, &staging, max_extract)
        .and_then(|files| swap_into_place(base, &staging).map(|()| files));
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn extract_verify(
    archive: &Path,
    format: ArchiveFormat,
    staging: &Path,
    max_extract: u64,
) -> Result<u32, CoresError> {
    let mut sink = Sink {
        staging,
        prefix: String::new(),
        budget: max_extract,
        entries: 0,
        files: 0,
    };
    match format {
        ArchiveFormat::SevenZ => extract_7z(archive, &mut sink)?,
        ArchiveFormat::Zip => extract_zip(archive, &mut sink)?,
    }
    for name in REQUIRED_FILES {
        if !staging
            .join(name)
            .is_file()
        {
            return Err(CoresError::Missing(name));
        }
    }
    check_version(&staging.join("version.json"))?;
    fs::write(staging.join(VERSION_MARKER), EMULATORJS_VERSION)?;
    Ok(sink.files)
}

fn check_version(path: &Path) -> Result<(), CoresError> {
    if !path.is_file() {
        return Err(CoresError::Missing("version.json"));
    }
    let mut raw = Vec::new();
    File::open(path)?
        .take(MAX_VERSION_JSON_BYTES)
        .read_to_end(&mut raw)?;
    let found = serde_json::from_slice::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| {
            v.get("version")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if found != EMULATORJS_VERSION {
        return Err(CoresError::VersionMismatch {
            found,
            expected: EMULATORJS_VERSION.into(),
        });
    }
    Ok(())
}

/// Picks the folder inside the archive that holds `loader.js`: `data/` when
/// the release layout is present, otherwise the shallowest `loader.js`. The
/// result is `""` (archive root) or ends in `/`. Names with `..` or `\` are
/// never candidates.
fn choose_prefix<'a>(names: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut best: Option<&str> = None;
    for name in names {
        if name.contains('\\')
            || name
                .split('/')
                .any(|c| c == "..")
        {
            continue;
        }
        let name = name.trim_start_matches("./");
        if name == "data/loader.js" {
            return Some("data/".into());
        }
        if name == "loader.js" || name.ends_with("/loader.js") {
            let shallower = best.is_none_or(|b| name.len() < b.len());
            if shallower {
                best = Some(name);
            }
        }
    }
    best.map(|n| n[..n.len() - "loader.js".len()].to_string())
}

/// Writes archive entries under the staging folder, enforcing path
/// containment and the extraction budget.
struct Sink<'a> {
    staging: &'a Path,
    prefix: String,
    budget: u64,
    entries: u64,
    files: u32,
}

impl Sink<'_> {
    fn entry(
        &mut self,
        name: &str,
        is_dir: bool,
        reader: &mut dyn Read,
    ) -> Result<(), CoresError> {
        self.entries += 1;
        if self.entries > MAX_ENTRIES {
            return Err(CoresError::TooManyEntries);
        }
        let normalized = name.trim_start_matches("./");
        let rel = if self
            .prefix
            .is_empty()
        {
            Some(normalized)
        } else {
            normalized.strip_prefix(
                self.prefix
                    .as_str(),
            )
        };
        // Entries outside the data folder (repo files) are consumed, not kept.
        let Some(rel) = rel.filter(|r| !r.is_empty()) else {
            self.copy_to(reader, &mut io::sink())?;
            return Ok(());
        };
        let target = crate::api::moonfin_emulator::contained_path(self.staging, rel)
            .ok_or(CoresError::UnsafePath)?;
        if is_dir {
            fs::create_dir_all(&target)?;
            return Ok(());
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = io::BufWriter::new(File::create(&target)?);
        self.copy_to(reader, &mut out)?;
        out.flush()?;
        self.files += 1;
        Ok(())
    }

    fn copy_to(
        &mut self,
        reader: &mut dyn Read,
        out: &mut dyn Write,
    ) -> Result<(), CoresError> {
        let n = io::copy(
            &mut reader.take(
                self.budget
                    .saturating_add(1),
            ),
            out,
        )?;
        if n > self.budget {
            return Err(CoresError::TooLarge { max: self.budget });
        }
        self.budget -= n;
        Ok(())
    }
}

fn extract_7z(archive: &Path, sink: &mut Sink<'_>) -> Result<(), CoresError> {
    let listing = sevenz_rust2::Archive::open(archive)
        .map_err(|e| CoresError::Archive(e.to_string()))?;
    sink.prefix = choose_prefix(
        listing
            .files
            .iter()
            .map(|f| f.name()),
    )
    .ok_or(CoresError::Missing("loader.js"))?;
    drop(listing);
    let mut reader =
        sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::empty())
            .map_err(|e| CoresError::Archive(e.to_string()))?;
    let mut failure: Option<CoresError> = None;
    let outcome = reader.for_each_entries(|entry, data| {
        if failure.is_some() {
            return Ok(false);
        }
        if entry.is_anti_item() {
            return Ok(true);
        }
        match sink.entry(entry.name(), entry.is_directory(), data) {
            Ok(()) => Ok(true),
            Err(e) => {
                failure = Some(e);
                Ok(false)
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    outcome.map_err(|e| CoresError::Archive(e.to_string()))
}

fn extract_zip(archive: &Path, sink: &mut Sink<'_>) -> Result<(), CoresError> {
    let mut zip = zip::ZipArchive::new(io::BufReader::new(File::open(archive)?))
        .map_err(|e| CoresError::Archive(e.to_string()))?;
    sink.prefix =
        choose_prefix(zip.file_names()).ok_or(CoresError::Missing("loader.js"))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| CoresError::Archive(e.to_string()))?;
        let name = entry
            .name()
            .to_string();
        let is_dir = entry.is_dir();
        sink.entry(&name, is_dir, &mut entry)?;
    }
    Ok(())
}

/// Replaces `<base>/data` with `staging` (same filesystem, so each step is an
/// atomic rename). On failure the previous install is put back.
fn swap_into_place(base: &Path, staging: &Path) -> Result<(), CoresError> {
    let data = base.join("data");
    let old = base.join(format!("data.old-{}", Uuid::new_v4()));
    let had_previous = fs::symlink_metadata(&data).is_ok();
    if had_previous {
        fs::rename(&data, &old)?;
    }
    if let Err(e) = fs::rename(staging, &data) {
        if had_previous {
            let _ = fs::rename(&old, &data);
        }
        return Err(e.into());
    }
    if had_previous {
        let _ = fs::remove_dir_all(&old);
    }
    Ok(())
}

fn cleanup_base(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    let data = base.join("data");
    let mut olds = Vec::new();
    for entry in entries.flatten() {
        let name = entry
            .file_name()
            .to_string_lossy()
            .into_owned();
        let path = entry.path();
        if name == "tmp" || name.starts_with("data.staging-") {
            let _ = fs::remove_dir_all(&path);
        } else if name.starts_with("data.old-") {
            olds.push(path);
        }
    }
    // A crash between the two swap renames leaves only `data.old-*`.
    let mut olds = olds.into_iter();
    if fs::symlink_metadata(&data).is_err()
        && let Some(first) = olds.next()
    {
        let _ = fs::rename(first, &data);
    }
    for stale in olds {
        let _ = fs::remove_dir_all(stale);
    }
}

fn count_files(root: &Path) -> u32 {
    let mut count = 0u32;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() && entry.file_name() != VERSION_MARKER {
                count = count.saturating_add(1);
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_source_matches_player_version() {
        assert!(DEFAULT_URL.contains(&format!("v{EMULATORJS_VERSION}/")));
        assert!(DEFAULT_URL.ends_with(&format!("{EMULATORJS_VERSION}.7z")));
        let source = CoresSource::from_config(&Config::default()).unwrap();
        assert_eq!(source.size, Some(DEFAULT_SIZE));
        assert_eq!(
            source
                .sha256
                .to_string(),
            DEFAULT_SHA256
        );
    }

    #[test]
    fn override_requires_checksum() {
        let mut c = Config {
            games_cores_url: Some("http://x/y.7z".into()),
            ..Default::default()
        };
        assert!(matches!(
            CoresSource::from_config(&c),
            Err(CoresError::Config(_))
        ));
        c.games_cores_sha256 = Some("zz".into());
        assert!(CoresSource::from_config(&c).is_err());
        c.games_cores_sha256 = Some("AB".repeat(32));
        assert!(CoresSource::from_config(&c).is_ok());
        let c = Config {
            games_cores_sha256: Some("ab".repeat(32)),
            ..Default::default()
        };
        assert!(CoresSource::from_config(&c).is_err());
    }

    #[test]
    fn prefix_selection() {
        let p = |names: &[&str]| {
            choose_prefix(
                names
                    .iter()
                    .copied(),
            )
        };
        assert_eq!(
            p(&["CHANGES.md", "data/loader.js"]).as_deref(),
            Some("data/")
        );
        assert_eq!(p(&["loader.js", "a.js"]).as_deref(), Some(""));
        assert_eq!(p(&["x/y/loader.js", "x/loader.js"]).as_deref(), Some("x/"));
        assert_eq!(p(&["../loader.js", "a/../loader.js"]), None);
        assert_eq!(p(&["a.js"]), None);
    }

    #[test]
    fn claim_lifecycle() {
        let svc = CoresService::default();
        let c = svc
            .try_begin()
            .unwrap();
        assert!(
            svc.try_begin()
                .is_none()
        );
        c.release();
        let c = svc
            .try_begin()
            .unwrap();
        drop(c);
        let g = lock(&svc.inner);
        assert_eq!(g.state, CoresState::Failed);
        assert_eq!(
            g.error
                .as_deref(),
            Some("install was interrupted")
        );
    }
}
