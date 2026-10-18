//! Unpacking of single-ROM `.zip` / `.7z` archives for non-arcade cores.
//!
//! Ported from Moonbase's `GamesService` (`ExtractRomFromArchive` /
//! `GetExtractedRomInfo`): native libretro cores and EmulatorJS want the raw
//! ROM, so the server serves the archive's ROM instead of the archive.
//! Arcade-family cores (mame/arcade) never come through here; they need the
//! archive intact.
//!
//! Selection rule (Moonbase): among the archive's files, the first entry whose
//! extension is a known ROM extension wins, otherwise the largest file. On top
//! of that Remux ignores macOS resource-fork junk (`__MACOSX/`, `._*`), other
//! dotfiles, empty files, symlinks and encrypted entries, none of which can be
//! a playable ROM.
//!
//! Safety: archive entry paths are never used to build an output path. Only a
//! sanitized single-segment base name is kept, and the file is written into a
//! directory Remux picked (`<cache>/<game id>/`), so zip-slip cannot happen.
//!
//! Serving works through an on-disk cache: the ROM is extracted once to
//! `<data_dir>/games/extracted/<game id>/<rom name>` (temp file + atomic
//! rename) and then served like any plain file, so Range and HEAD requests
//! work. A `.source` stamp records the archive's size and mtime; a changed
//! archive invalidates the cached copy.

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::UNIX_EPOCH,
};

use dashmap::DashMap;
use tracing::debug;

use super::resolver;

/// Default ceiling on a single unpacked ROM (1 GiB).
pub const DEFAULT_MAX_EXTRACT_BYTES: u64 = 1024 * 1024 * 1024;

/// Longest file name (in bytes) most filesystems accept.
const MAX_NAME_BYTES: usize = 255;
const STAMP_FILE: &str = ".source";
const STAMP_VERSION: &str = "v1";

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("7z error: {0}")]
    SevenZ(#[from] sevenz_rust2::Error),
    #[error("not a supported archive")]
    Unsupported,
    #[error("archive holds no playable ROM")]
    NoRom,
    #[error("ROM is {size} bytes, over the {max} byte limit")]
    TooLarge { size: u64, max: u64 },
    #[error("archive changed or is inconsistent while unpacking")]
    Changed,
}

/// The ROM a client is served out of an archive. `name` is a bare file name
/// with no path from inside the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRom {
    pub name: String,
    pub size: u64,
    /// CRC-32 of the ROM as recorded in the archive index (no decompression
    /// needed to read it). `None` when the archive does not record one.
    pub crc32: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Zip,
    SevenZ,
}

fn format_of(path: &Path) -> Option<Format> {
    match path
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "zip" => Some(Format::Zip),
        "7z" => Some(Format::SevenZ),
        _ => None,
    }
}

/// The chosen archive entry: how to find it again, plus what it is called.
#[derive(Debug, Clone)]
struct Candidate {
    /// Zip entry index.
    index: usize,
    /// Entry name exactly as stored (7z lookup key).
    raw_name: String,
    /// Sanitized base name.
    name: String,
    size: u64,
    /// CRC-32 recorded in the archive index.
    crc32: Option<u32>,
}

/// Last path segment of an entry name (either separator), or `None` when the
/// entry cannot be a ROM we are willing to write: no name, `.`/`..`, hidden
/// files, macOS junk, control characters or an over-long name.
fn entry_basename(raw: &str) -> Option<String> {
    let mut segments = raw
        .split(['/', '\\'])
        .collect::<Vec<_>>();
    let name = segments
        .pop()?
        .trim();
    if segments
        .iter()
        .any(|s| s.eq_ignore_ascii_case("__MACOSX"))
    {
        return None;
    }
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('.')
        || name.len() > MAX_NAME_BYTES
        || name
            .chars()
            .any(char::is_control)
    {
        return None;
    }
    Some(name.to_string())
}

/// Moonbase's pick: first known-ROM extension, else the largest file.
fn choose(candidates: impl IntoIterator<Item = Candidate>) -> Option<Candidate> {
    let mut largest: Option<Candidate> = None;
    for c in candidates {
        if resolver::is_known_rom_extension(Path::new(&c.name)) {
            return Some(c);
        }
        if largest
            .as_ref()
            .is_none_or(|l| c.size > l.size)
        {
            largest = Some(c);
        }
    }
    largest
}

fn select(archive: &Path) -> Result<Option<Candidate>, ArchiveError> {
    match format_of(archive) {
        Some(Format::Zip) => select_zip(archive),
        Some(Format::SevenZ) => select_7z(archive),
        None => Err(ArchiveError::Unsupported),
    }
}

fn select_zip(path: &Path) -> Result<Option<Candidate>, ArchiveError> {
    let mut zip = zip::ZipArchive::new(io::BufReader::new(File::open(path)?))?;
    let mut candidates = Vec::new();
    for index in 0..zip.len() {
        // Raw access reads only the central directory entry: no decryption or
        // decompression is attempted while listing.
        let f = zip.by_index_raw(index)?;
        if f.is_dir() || f.is_symlink() || f.encrypted() || f.size() == 0 {
            continue;
        }
        let Some(name) = entry_basename(f.name()) else {
            continue;
        };
        candidates.push(Candidate {
            index,
            raw_name: f
                .name()
                .to_string(),
            name,
            size: f.size(),
            crc32: Some(f.crc32()),
        });
    }
    Ok(choose(candidates))
}

fn select_7z(path: &Path) -> Result<Option<Candidate>, ArchiveError> {
    let archive = sevenz_rust2::Archive::open(path)?;
    let candidates = archive
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.has_stream() && !f.is_directory() && !f.is_anti_item() && f.size() > 0
        })
        .filter_map(|(index, f)| {
            Some(Candidate {
                index,
                raw_name: f
                    .name()
                    .to_string(),
                name: entry_basename(f.name())?,
                size: f.size(),
                crc32: f
                    .has_crc
                    .then_some(f.crc as u32),
            })
        })
        .collect::<Vec<_>>();
    Ok(choose(candidates))
}

/// The ROM inside `archive`, read from the archive index only (nothing is
/// unpacked). `Ok(None)` when the archive holds no usable ROM.
pub fn inspect(archive: &Path) -> Result<Option<ArchiveRom>, ArchiveError> {
    Ok(select(archive)?.map(|c| ArchiveRom {
        name: c.name,
        size: c.size,
        crc32: c.crc32,
    }))
}

/// Copies `reader` to `writer`, failing as soon as more than `max` bytes
/// appear (the header size is attacker-controlled; this is the real limit).
fn copy_limited<R: Read + ?Sized, W: Write>(
    reader: &mut R,
    writer: &mut W,
    max: u64,
) -> Result<u64, ArchiveError> {
    let n = io::copy(&mut Read::take(&mut *reader, max.saturating_add(1)), writer)?;
    if n > max {
        return Err(ArchiveError::TooLarge { size: n, max });
    }
    Ok(n)
}

/// Unpacks the archive's ROM into `out_dir` as `<out_dir>/<base name>`,
/// atomically (temp file + rename). `out_dir` must exist.
pub fn extract_rom(
    archive: &Path,
    out_dir: &Path,
    max_bytes: u64,
) -> Result<ArchiveRom, ArchiveError> {
    let cand = select(archive)?.ok_or(ArchiveError::NoRom)?;
    if cand.size > max_bytes {
        return Err(ArchiveError::TooLarge {
            size: cand.size,
            max: max_bytes,
        });
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(out_dir)?;
    let written = match format_of(archive) {
        Some(Format::Zip) => {
            let mut zip =
                zip::ZipArchive::new(io::BufReader::new(File::open(archive)?))?;
            let mut entry = zip.by_index(cand.index)?;
            if entry.name() != cand.raw_name {
                return Err(ArchiveError::Changed);
            }
            copy_limited(&mut entry, tmp.as_file_mut(), max_bytes)?
        }
        Some(Format::SevenZ) => {
            let mut reader = sevenz_rust2::ArchiveReader::open(
                archive,
                sevenz_rust2::Password::empty(),
            )?;
            let mut outcome: Option<Result<u64, ArchiveError>> = None;
            reader.for_each_entries(|entry, data| {
                if outcome.is_some() {
                    return Ok(false);
                }
                if entry.name() == cand.raw_name && entry.size() == cand.size {
                    outcome = Some(copy_limited(data, tmp.as_file_mut(), max_bytes));
                    return Ok(false);
                }
                // Solid blocks are one stream: an entry we skip still has to
                // be consumed to reach the next one.
                io::copy(data, &mut io::sink())?;
                Ok(true)
            })?;
            outcome.ok_or(ArchiveError::Changed)??
        }
        None => return Err(ArchiveError::Unsupported),
    };
    if written != cand.size {
        return Err(ArchiveError::Changed);
    }
    tmp.as_file_mut()
        .flush()?;
    tmp.persist(out_dir.join(&cand.name))
        .map_err(|e| e.error)?;
    Ok(ArchiveRom {
        name: cand.name,
        size: written,
        crc32: cand.crc32,
    })
}

// ---------------------------------------------------------------------------
// Disk cache
// ---------------------------------------------------------------------------

/// A ROM unpacked into the cache, ready to be served as a plain file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
}

/// One async mutex per game id so concurrent first requests share a single
/// extraction. Entries are tiny and bounded by the game count.
static EXTRACT_LOCKS: LazyLock<DashMap<String, Arc<tokio::sync::Mutex<()>>>> =
    LazyLock::new(DashMap::new);

/// The cached ROM for `archive`, extracting it first when the cache has no
/// current copy. `cache_root` is `<data_dir>/games/extracted`.
pub async fn ensure_unpacked(
    cache_root: &Path,
    game_id: &str,
    archive: &Path,
    max_bytes: u64,
) -> Result<Unpacked, ArchiveError> {
    // Ids come from the snapshot (hex digests); anything else never touches
    // the filesystem.
    if game_id.is_empty()
        || !game_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric())
    {
        return Err(ArchiveError::Unsupported);
    }
    let lock = EXTRACT_LOCKS
        .entry(game_id.to_string())
        .or_default()
        .clone();
    // The guard is owned by the blocking closure, not this future: if the
    // request is dropped mid-extraction the thread keeps running, so the
    // lock must stay held until it finishes. A retry then waits and finds
    // the finished entry instead of wiping the directory under it.
    let guard = lock
        .lock_owned()
        .await;
    let dir = cache_root.join(game_id);
    let archive = archive.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        unpack_blocking(&dir, &archive, max_bytes)
    })
    .await
    .map_err(|e| ArchiveError::Io(io::Error::other(e)))?
}

fn stamp_head(archive: &Path) -> Result<String, ArchiveError> {
    let meta = fs::metadata(archive)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| {
            t.duration_since(UNIX_EPOCH)
                .ok()
        })
        .map_or(0, |d| d.as_nanos());
    Ok(format!("{STAMP_VERSION}\n{}\n{mtime}\n", meta.len()))
}

/// The cached ROM when the stamp matches the archive's current state and the
/// file is intact.
fn read_hit(dir: &Path, head: &str) -> Option<Unpacked> {
    let stamp = fs::read_to_string(dir.join(STAMP_FILE)).ok()?;
    let mut lines = stamp
        .strip_prefix(head)?
        .lines();
    let name = entry_basename(lines.next()?)?;
    let size: u64 = lines
        .next()?
        .parse()
        .ok()?;
    let path = dir.join(&name);
    let meta = fs::metadata(&path).ok()?;
    (meta.is_file() && meta.len() == size).then_some(Unpacked { path, name, size })
}

fn unpack_blocking(
    dir: &Path,
    archive: &Path,
    max_bytes: u64,
) -> Result<Unpacked, ArchiveError> {
    let head = stamp_head(archive)?;
    if let Some(hit) = read_hit(dir, &head) {
        return Ok(hit);
    }
    // Stale or missing: start from an empty directory so a ROM that was
    // renamed inside a replaced archive does not leave the old file behind.
    #[cfg(test)]
    test_gate::reached(dir);
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir)?;
    let result = extract_into_cache(dir, archive, &head, max_bytes);
    if result.is_err() {
        let _ = fs::remove_dir_all(dir);
    }
    result
}

fn extract_into_cache(
    dir: &Path,
    archive: &Path,
    head: &str,
    max_bytes: u64,
) -> Result<Unpacked, ArchiveError> {
    let rom = extract_rom(archive, dir, max_bytes)?;
    // The archive must not have been replaced while it was being read.
    if stamp_head(archive)? != head {
        return Err(ArchiveError::Changed);
    }
    // The stamp goes in last: a crash before this point leaves no valid stamp,
    // so the half-built entry is discarded next time.
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(dir)?;
    write!(tmp, "{head}{}\n{}\n", rom.name, rom.size)?;
    tmp.persist(dir.join(STAMP_FILE))
        .map_err(|e| e.error)?;
    debug!(archive = %archive.display(), rom = %rom.name, bytes = rom.size, "games: unpacked archive ROM");
    Ok(Unpacked {
        path: dir.join(&rom.name),
        name: rom.name,
        size: rom.size,
    })
}

/// Test hook that parks a real extraction (cache miss) until released, so a
/// test can drop the requesting future while the blocking thread is running.
#[cfg(test)]
pub(crate) mod test_gate {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};

    pub struct Gate {
        pub entered: tokio::sync::Notify,
        pub runs: AtomicUsize,
        released: Mutex<bool>,
        cv: Condvar,
    }

    impl Gate {
        pub fn release(&self) {
            *self
                .released
                .lock()
                .unwrap() = true;
            self.cv
                .notify_all();
        }
    }

    static GATES: LazyLock<DashMap<PathBuf, Arc<Gate>>> = LazyLock::new(DashMap::new);

    pub fn install(dir: &Path) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            entered: tokio::sync::Notify::new(),
            runs: AtomicUsize::new(0),
            released: Mutex::new(false),
            cv: Condvar::new(),
        });
        GATES.insert(dir.to_path_buf(), gate.clone());
        gate
    }

    pub fn reached(dir: &Path) {
        let Some(gate) = GATES
            .get(dir)
            .map(|g| g.clone())
        else {
            return;
        };
        gate.runs
            .fetch_add(1, Ordering::SeqCst);
        gate.entered
            .notify_one();
        let mut released = gate
            .released
            .lock()
            .unwrap();
        while !*released {
            released = gate
                .cv
                .wait(released)
                .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            if name.ends_with('/') {
                w.add_directory(*name, opts)
                    .unwrap();
            } else {
                w.start_file(*name, opts)
                    .unwrap();
                w.write_all(data)
                    .unwrap();
            }
        }
        w.finish()
            .unwrap();
    }

    fn write_7z(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = sevenz_rust2::ArchiveWriter::create(path).unwrap();
        for (name, data) in entries {
            w.push_archive_entry(
                sevenz_rust2::ArchiveEntry::new_file(name),
                Some(*data),
            )
            .unwrap();
        }
        w.finish()
            .unwrap();
    }

    #[test]
    fn basename_sanitizing() {
        assert_eq!(entry_basename("a/b/game.nes").as_deref(), Some("game.nes"));
        assert_eq!(
            entry_basename("a\\b\\game.nes").as_deref(),
            Some("game.nes")
        );
        assert_eq!(
            entry_basename("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(entry_basename("dir/"), None);
        assert_eq!(entry_basename(".."), None);
        assert_eq!(entry_basename("a/.."), None);
        assert_eq!(entry_basename("._game.nes"), None);
        assert_eq!(entry_basename("__MACOSX/._game.nes"), None);
        assert_eq!(entry_basename("__macosx/game.nes"), None);
        assert_eq!(entry_basename("a\0b.nes"), None);
        assert_eq!(entry_basename(&"x".repeat(300)), None);
    }

    #[test]
    fn zip_rom_beats_readme_and_picks_first_known_rom() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir
            .path()
            .join("g.zip");
        write_zip(
            &p,
            &[
                ("readme.txt", &[b'r'; 500]),
                ("sub/", b""),
                ("sub/First.sfc", b"first-rom"),
                ("Second.sfc", b"second-rom-bigger"),
            ],
        );
        let rom = inspect(&p)
            .unwrap()
            .unwrap();
        assert_eq!(
            rom,
            ArchiveRom {
                name: "First.sfc".into(),
                size: 9,
                crc32: Some(crc32fast::hash(b"first-rom")),
            }
        );
    }

    #[test]
    fn zip_without_known_extension_falls_back_to_largest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir
            .path()
            .join("g.zip");
        write_zip(
            &p,
            &[("a.dat", b"12"), ("b.dat", b"12345"), ("c.dat", b"123")],
        );
        let rom = inspect(&p)
            .unwrap()
            .unwrap();
        assert_eq!(rom.name, "b.dat");
        assert_eq!(rom.size, 5);
    }

    #[test]
    fn junk_only_and_empty_archives_have_no_rom() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir
            .path()
            .join("g.zip");
        write_zip(
            &p,
            &[
                ("__MACOSX/._g.nes", b"junk"),
                (".hidden", b"x"),
                ("empty.nes", b""),
            ],
        );
        assert_eq!(inspect(&p).unwrap(), None);
        let e = dir
            .path()
            .join("e.zip");
        write_zip(&e, &[]);
        assert_eq!(inspect(&e).unwrap(), None);
        assert!(matches!(
            extract_rom(&e, dir.path(), 1024),
            Err(ArchiveError::NoRom)
        ));
    }

    #[test]
    fn corrupt_archives_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["bad.zip", "bad.7z"] {
            let p = dir
                .path()
                .join(name);
            fs::write(&p, b"this is not an archive").unwrap();
            assert!(inspect(&p).is_err(), "{name}");
            assert!(extract_rom(&p, dir.path(), 1024).is_err(), "{name}");
        }
        // Truncated real zip.
        let good = dir
            .path()
            .join("good.zip");
        write_zip(&good, &[("a.nes", &[7u8; 4096])]);
        let bytes = fs::read(&good).unwrap();
        let cut = dir
            .path()
            .join("cut.zip");
        fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
        assert!(inspect(&cut).is_err());
        assert!(matches!(
            inspect(
                &dir.path()
                    .join("x.rar")
            ),
            Err(ArchiveError::Unsupported)
        ));
    }

    #[test]
    fn zip_slip_names_stay_inside_the_output_dir() {
        let base = tempfile::tempdir().unwrap();
        let out = base
            .path()
            .join("cache/gameid");
        fs::create_dir_all(&out).unwrap();
        let p = base
            .path()
            .join("evil.zip");
        write_zip(
            &p,
            &[("../../../evil.nes", b"pwned"), ("..\\..\\win.sfc", b"x")],
        );
        let rom = extract_rom(&p, &out, 1024).unwrap();
        assert_eq!(rom.name, "evil.nes");
        assert_eq!(fs::read(out.join("evil.nes")).unwrap(), b"pwned");
        assert!(
            !base
                .path()
                .join("evil.nes")
                .exists()
        );
        assert!(
            !base
                .path()
                .join("cache/evil.nes")
                .exists()
        );
        let mut all = fs::read_dir(&out)
            .unwrap()
            .count();
        all += fs::read_dir(
            base.path()
                .join("cache"),
        )
        .unwrap()
        .count();
        assert_eq!(all, 1 + 1, "only the ROM and the gameid dir exist");
    }

    #[test]
    fn oversize_rom_is_rejected_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir
            .path()
            .join("g.zip");
        write_zip(&p, &[("big.nes", &[1u8; 2048])]);
        let out = dir
            .path()
            .join("out");
        fs::create_dir_all(&out).unwrap();
        assert!(matches!(
            extract_rom(&p, &out, 1000),
            Err(ArchiveError::TooLarge { max: 1000, .. })
        ));
        assert_eq!(
            fs::read_dir(&out)
                .unwrap()
                .count(),
            0
        );
        // Exactly at the limit is fine.
        assert_eq!(
            extract_rom(&p, &out, 2048)
                .unwrap()
                .size,
            2048
        );
    }

    #[test]
    fn sevenz_roundtrip_with_readme_and_multi_entry() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir
            .path()
            .join("g.7z");
        write_7z(
            &p,
            &[
                ("readme.txt", &[b'r'; 300]),
                ("dir/Game.gba", b"gba-rom-bytes"),
                ("other.gba", b"another"),
            ],
        );
        let rom = inspect(&p)
            .unwrap()
            .unwrap();
        assert_eq!(rom.name, "Game.gba");
        assert_eq!(rom.size, 13);
        let out = dir
            .path()
            .join("out");
        fs::create_dir_all(&out).unwrap();
        let got = extract_rom(&p, &out, 1 << 20).unwrap();
        assert_eq!(got, rom);
        assert_eq!(fs::read(out.join("Game.gba")).unwrap(), b"gba-rom-bytes");
    }

    #[test]
    fn sevenz_target_after_other_entries_in_one_solid_block() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir
            .path()
            .join("g.7z");
        write_7z(
            &p,
            &[
                ("a.txt", &[b'a'; 1000]),
                ("b.dat", &[b'b'; 10]),
                ("z.nes", b"nes!"),
            ],
        );
        let out = dir
            .path()
            .join("out");
        fs::create_dir_all(&out).unwrap();
        assert_eq!(
            extract_rom(&p, &out, 1 << 20)
                .unwrap()
                .name,
            "z.nes"
        );
        assert_eq!(fs::read(out.join("z.nes")).unwrap(), b"nes!");
    }

    #[tokio::test]
    async fn cache_extracts_once_dedupes_and_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir
            .path()
            .join("g.zip");
        write_zip(&p, &[("Game.nes", b"version-one")]);
        let cache = dir
            .path()
            .join("cache");

        // Concurrent first requests share one extraction.
        let (a, b) = tokio::join!(
            ensure_unpacked(&cache, "abc123", &p, 1 << 20),
            ensure_unpacked(&cache, "abc123", &p, 1 << 20),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a, b);
        assert_eq!(a.path, cache.join("abc123/Game.nes"));
        assert_eq!(fs::read(&a.path).unwrap(), b"version-one");

        // Second request is a cache hit: the cached file is not rewritten.
        let before = fs::metadata(&a.path)
            .unwrap()
            .modified()
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let again = ensure_unpacked(&cache, "abc123", &p, 1 << 20)
            .await
            .unwrap();
        assert_eq!(again, a);
        assert_eq!(
            fs::metadata(&again.path)
                .unwrap()
                .modified()
                .unwrap(),
            before
        );

        // Replacing the archive (different content, name and size) invalidates.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        write_zip(&p, &[("Renamed.nes", b"version-two-longer")]);
        let fresh = ensure_unpacked(&cache, "abc123", &p, 1 << 20)
            .await
            .unwrap();
        assert_eq!(fresh.name, "Renamed.nes");
        assert_eq!(fs::read(&fresh.path).unwrap(), b"version-two-longer");
        assert!(
            !cache
                .join("abc123/Game.nes")
                .exists(),
            "stale ROM removed"
        );
    }

    #[tokio::test]
    async fn cache_rejects_unsafe_ids_and_cleans_up_failures() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir
            .path()
            .join("g.zip");
        write_zip(&p, &[("Game.nes", &[3u8; 100])]);
        let cache = dir
            .path()
            .join("cache");
        for bad in ["", "../x", "a/b", "a.b"] {
            assert!(matches!(
                ensure_unpacked(&cache, bad, &p, 1 << 20).await,
                Err(ArchiveError::Unsupported)
            ));
        }
        assert!(matches!(
            ensure_unpacked(&cache, "tiny", &p, 10).await,
            Err(ArchiveError::TooLarge { .. })
        ));
        assert!(
            !cache
                .join("tiny")
                .exists()
        );
    }

    #[tokio::test]
    async fn dropped_request_keeps_the_lock_until_unpacking_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir
            .path()
            .join("g.zip");
        write_zip(&p, &[("Game.nes", b"payload")]);
        let cache = dir
            .path()
            .join("cache");
        let gate = test_gate::install(&cache.join("dropgame1"));

        let first = tokio::spawn({
            let (cache, p) = (cache.clone(), p.clone());
            async move { ensure_unpacked(&cache, "dropgame1", &p, 1 << 20).await }
        });
        // The extraction is now running on the blocking thread.
        gate.entered
            .notified()
            .await;
        first.abort();
        let _ = first.await;

        let second = tokio::spawn({
            let (cache, p) = (cache.clone(), p.clone());
            async move { ensure_unpacked(&cache, "dropgame1", &p, 1 << 20).await }
        });
        // Let the second request reach the lock queue, then finish the first
        // extraction. Correctness does not depend on the ordering.
        tokio::task::yield_now().await;
        gate.release();

        let got = second
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(&got.path).unwrap(), b"payload");
        assert_eq!(
            gate.runs
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the retry must reuse the finished extraction"
        );
    }
}
