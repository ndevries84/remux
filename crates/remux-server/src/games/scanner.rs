//! On-disk discovery of ROMs and BIOS files.
//!
//! Layout (ported from Moonbase's `GamesService`):
//!
//! ```text
//! <root>/<System>/               one folder per console, e.g. "SNES"
//!     <bios files>               loose, non-ROM files with a BIOS extension
//!     <loose rom>.<ext>          ROMs directly in the system folder are games
//!     <Game name>/<rom>.<ext>    one folder per game; first playable direct
//!                                child is the ROM (no deeper recursion)
//! ```
//!
//! The scan is synchronous and blocking; callers run it on a blocking thread.
//! Every client-visible id is an opaque hex digest, never a path.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use serde::Serialize;
use tracing::{debug, warn};
use uuid::Uuid;

use super::{
    archive::{self, ArchiveRom},
    resolver::{self, Core},
};

#[derive(Debug, Clone, Serialize)]
pub struct Game {
    pub id: String,
    pub title: String,
    /// System folder name as it exists on disk.
    pub system: String,
    pub core: Core,
    pub file_name: String,
    #[serde(skip)]
    pub path: PathBuf,
    pub size_bytes: u64,
    /// The single ROM inside a `.zip`/`.7z` for a non-arcade core. The server
    /// unpacks it on demand, so this is what clients actually receive.
    /// `None` for plain ROMs, arcade archives (served intact) and archives
    /// that could not be inspected or hold no ROM (served as-is).
    #[serde(skip)]
    pub inner_rom: Option<ArchiveRom>,
}

impl Game {
    /// File name the ROM endpoint serves (the inner ROM's for unpacked archives).
    pub fn served_file_name(&self) -> &str {
        self.inner_rom
            .as_ref()
            .map_or(
                self.file_name
                    .as_str(),
                |r| {
                    r.name
                        .as_str()
                },
            )
    }

    /// Size of what the ROM endpoint serves.
    pub fn served_size_bytes(&self) -> u64 {
        self.inner_rom
            .as_ref()
            .map_or(self.size_bytes, |r| r.size)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Bios {
    pub id: String,
    pub file_name: String,
    #[serde(skip)]
    pub path: PathBuf,
    pub size_bytes: u64,
}

/// Immutable result of one scan, with id lookups.
#[derive(Debug)]
pub struct GamesSnapshot {
    /// Sorted by title (case-insensitive), then id.
    pub games: Vec<Game>,
    /// Per system folder name; only systems that contain at least one game.
    pub bios: BTreeMap<String, Vec<Bios>>,
    game_by_id: HashMap<String, usize>,
    bios_by_id: HashMap<String, (String, usize)>,
    pub scanned_at: Instant,
}

impl GamesSnapshot {
    pub fn empty() -> Self {
        Self {
            games: Vec::new(),
            bios: BTreeMap::new(),
            game_by_id: HashMap::new(),
            bios_by_id: HashMap::new(),
            scanned_at: Instant::now(),
        }
    }

    /// Resolves an opaque id from the index. Unknown ids (including anything
    /// that merely looks like a path) simply miss.
    pub fn game(&self, id: &str) -> Option<&Game> {
        self.game_by_id
            .get(id)
            .map(|&i| &self.games[i])
    }

    pub fn bios_file(&self, id: &str) -> Option<&Bios> {
        let (system, i) = self
            .bios_by_id
            .get(id)?;
        self.bios
            .get(system)
            .and_then(|v| v.get(*i))
    }

    /// Distinct system folder names with at least one game, sorted.
    pub fn systems(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self
            .games
            .iter()
            .map(|g| {
                g.system
                    .as_str()
            })
            .collect();
        v.sort_unstable_by_key(|s| s.to_ascii_lowercase());
        v.dedup();
        v
    }
}

const ID_NAMESPACE_SEED: &[u8] = b"remux-games-id";

/// Stable id: lowercase hex (32 chars) of a v5 UUID over
/// `<kind>:<root index>:<path relative to canonical root, '/'-joined>`.
fn make_id(kind: &str, root_index: usize, rel: &str) -> String {
    let ns = Uuid::new_v5(&Uuid::NAMESPACE_OID, ID_NAMESPACE_SEED);
    Uuid::new_v5(&ns, format!("{kind}:{root_index}:{rel}").as_bytes())
        .simple()
        .to_string()
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy()
        .starts_with('.')
}

/// Sorted direct children; unreadable directories yield nothing.
fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| !is_hidden(&e.file_name()))
            .map(|e| e.path())
            .collect(),
        Err(e) => {
            debug!(dir = %dir.display(), error = %e, "games: cannot read directory");
            Vec::new()
        }
    };
    v.sort();
    v
}

/// Canonicalizes `path` and returns it only when it is a regular file located
/// inside `root` (already canonical). Symlinks escaping the root are dropped.
fn safe_file(root: &Path, path: &Path) -> Option<(PathBuf, u64)> {
    let canon = path
        .canonicalize()
        .ok()?;
    if !canon.starts_with(root) {
        warn!(path = %path.display(), "games: skipping file that resolves outside its root");
        return None;
    }
    let meta = fs::metadata(&canon).ok()?;
    meta.is_file()
        .then_some((canon, meta.len()))
}

/// Like [`safe_file`] for directories (a symlinked system/game folder that
/// escapes the root is ignored wholesale).
fn safe_dir(root: &Path, path: &Path) -> Option<PathBuf> {
    let canon = path
        .canonicalize()
        .ok()?;
    (canon.starts_with(root) && canon.is_dir()).then_some(canon)
}

fn rel_string(root: &Path, file: &Path) -> Option<String> {
    let rel = file
        .strip_prefix(root)
        .ok()?;
    Some(
        rel.components()
            .map(|c| {
                c.as_os_str()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| {
            n.to_string_lossy()
                .into_owned()
        })
        .unwrap_or_default()
}

/// A discovered ROM before core resolution.
struct RawGame {
    path: PathBuf,
    size: u64,
    title: String,
    inner_rom: Option<ArchiveRom>,
}

/// Cheap archive listing (no extraction) for archives that will be unpacked,
/// i.e. any archive outside the arcade family. Failures are not fatal: the
/// game stays listed and its archive is served as-is.
fn inspect_archive(path: &Path, name_core: Option<Core>) -> Option<ArchiveRom> {
    if !resolver::is_archive(path) || name_core.is_some_and(Core::is_arcade_family) {
        return None;
    }
    match archive::inspect(path) {
        Ok(rom) => rom,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "games: cannot inspect archive");
            None
        }
    }
}

fn discover_roms(
    root: &Path,
    system_dir: &Path,
    name_core: Option<Core>,
) -> Vec<RawGame> {
    // Playability depends on the core only for arcade-family names; when the
    // name is unrecognised any ROM/archive qualifies (as in Moonbase).
    let playable = |p: &Path| match name_core {
        Some(c) => resolver::is_playable_for_core(c, p),
        None => resolver::is_rom_file(p),
    };
    let mut out = Vec::new();

    // One folder per game: first playable direct child.
    for entry in read_dir_sorted(system_dir) {
        let Some(game_dir) = safe_dir(root, &entry) else {
            continue;
        };
        if game_dir == *system_dir {
            continue;
        }
        let title = file_name(&entry);
        let rom = read_dir_sorted(&game_dir)
            .into_iter()
            .filter(|p| playable(p))
            .find_map(|p| safe_file(root, &p));
        if let Some((path, size)) = rom {
            let inner_rom = inspect_archive(&path, name_core);
            out.push(RawGame {
                path,
                size,
                title,
                inner_rom,
            });
        }
    }

    // Loose ROMs directly in the system folder.
    for entry in read_dir_sorted(system_dir) {
        if !playable(&entry) {
            continue;
        }
        if let Some((path, size)) = safe_file(root, &entry) {
            let title = path
                .file_stem()
                .map(|s| {
                    s.to_string_lossy()
                        .into_owned()
                })
                .unwrap_or_default();
            let inner_rom = inspect_archive(&path, name_core);
            out.push(RawGame {
                path,
                size,
                title,
                inner_rom,
            });
        }
    }
    out
}

fn discover_bios(root: &Path, system_dir: &Path, root_index: usize) -> Vec<Bios> {
    read_dir_sorted(system_dir)
        .into_iter()
        .filter(|p| !resolver::is_rom_file(p) && resolver::is_bios_file(p))
        .filter_map(|p| {
            let (path, size_bytes) = safe_file(root, &p)?;
            let rel = rel_string(root, &path)?;
            Some(Bios {
                id: make_id("bios", root_index, &rel),
                file_name: file_name(&path),
                path,
                size_bytes,
            })
        })
        .collect()
}

/// Scans every root. Missing/unreadable roots are skipped with a warning.
pub fn scan(roots: &[PathBuf]) -> GamesSnapshot {
    let mut games: Vec<Game> = Vec::new();
    let mut bios: BTreeMap<String, Vec<Bios>> = BTreeMap::new();
    let mut seen_games: std::collections::HashSet<String> = Default::default();

    for (root_index, raw_root) in roots
        .iter()
        .enumerate()
    {
        let root = match raw_root.canonicalize() {
            Ok(r) if r.is_dir() => r,
            _ => {
                warn!(root = %raw_root.display(), "games: root is not an accessible directory");
                continue;
            }
        };
        for entry in read_dir_sorted(&root) {
            let Some(system_dir) = safe_dir(&root, &entry) else {
                continue;
            };
            if system_dir == root {
                continue;
            }
            let system = file_name(&entry);
            let name_core = resolver::core_for_system_name(&system);
            let raw = discover_roms(&root, &system_dir, name_core);
            if raw.is_empty() {
                continue;
            }
            // An unpacked archive stands for the ROM inside it, so its
            // extension can resolve an unrecognised system folder.
            let paths: Vec<PathBuf> = raw
                .iter()
                .map(|r| match &r.inner_rom {
                    Some(rom) => PathBuf::from(&rom.name),
                    None => r
                        .path
                        .clone(),
                })
                .collect();
            let core = resolver::resolve_system_core(&system, &paths).core;

            for r in raw {
                let Some(rel) = rel_string(&root, &r.path) else {
                    continue;
                };
                let id = make_id("game", root_index, &rel);
                if !seen_games.insert(id.clone()) {
                    continue;
                }
                games.push(Game {
                    id,
                    title: r.title,
                    system: system.clone(),
                    core,
                    file_name: file_name(&r.path),
                    path: r.path,
                    size_bytes: r.size,
                    inner_rom: r.inner_rom,
                });
            }
            let mut b = discover_bios(&root, &system_dir, root_index);
            if !b.is_empty() {
                bios.entry(system)
                    .or_default()
                    .append(&mut b);
            }
        }
    }

    games.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(
                &b.title
                    .to_lowercase(),
            )
            .then_with(|| {
                a.id.cmp(&b.id)
            })
    });
    let game_by_id = games
        .iter()
        .enumerate()
        .map(|(i, g)| (g.id.clone(), i))
        .collect();
    let mut bios_by_id = HashMap::new();
    for (system, list) in &bios {
        for (i, b) in list
            .iter()
            .enumerate()
        {
            bios_by_id.insert(b.id.clone(), (system.clone(), i));
        }
    }
    GamesSnapshot {
        games,
        bios,
        game_by_id,
        bios_by_id,
        scanned_at: Instant::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};

    fn make_layout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        // SNES: loose rom, game folder with rom, nested deeper folder (ignored),
        // unsupported extension, BIOS file.
        create_dir_all(r.join("SNES/Super Metroid")).unwrap();
        create_dir_all(r.join("SNES/Too/Deep/Game")).unwrap();
        write(r.join("SNES/Loose Game.sfc"), b"loose").unwrap();
        write(r.join("SNES/Super Metroid/metroid.smc"), b"metroid!").unwrap();
        write(r.join("SNES/Super Metroid/readme.txt"), b"x").unwrap();
        write(r.join("SNES/Too/Deep/Game/deep.sfc"), b"deep").unwrap();
        write(r.join("SNES/notes.txt"), b"nope").unwrap();
        write(r.join("SNES/.hidden.sfc"), b"nope").unwrap();
        // PlayStation: archive game and BIOS.
        create_dir_all(r.join("PlayStation/Crash")).unwrap();
        write(r.join("PlayStation/Crash/crash.7z"), b"archive").unwrap();
        write(r.join("PlayStation/scph1001.bin"), b"bios").unwrap();
        // Arcade: only zip counts.
        create_dir_all(r.join("Arcade/Pac-Man")).unwrap();
        write(r.join("Arcade/Pac-Man/pacman.zip"), b"zip").unwrap();
        write(r.join("Arcade/Pac-Man/pacman.nes"), b"nes").unwrap();
        // System with no games is omitted.
        create_dir_all(r.join("Empty")).unwrap();
        write(r.join("Empty/readme.txt"), b"x").unwrap();
        // Loose file at root level is not a system.
        write(r.join("stray.sfc"), b"x").unwrap();
        dir
    }

    #[test]
    fn scans_expected_layout() {
        let dir = make_layout();
        let snap = scan(&[dir
            .path()
            .to_path_buf()]);

        let titles: Vec<&str> = snap
            .games
            .iter()
            .map(|g| {
                g.title
                    .as_str()
            })
            .collect();
        assert_eq!(
            titles,
            vec!["Crash", "Loose Game", "Pac-Man", "Super Metroid"]
        );
        assert_eq!(snap.systems(), vec!["Arcade", "PlayStation", "SNES"]);

        let metroid = snap
            .games
            .iter()
            .find(|g| g.title == "Super Metroid")
            .unwrap();
        assert_eq!(metroid.core, Core::Snes);
        assert_eq!(metroid.file_name, "metroid.smc");
        assert_eq!(metroid.size_bytes, 8);
        assert_eq!(metroid.system, "SNES");

        let pac = snap
            .games
            .iter()
            .find(|g| g.title == "Pac-Man")
            .unwrap();
        assert_eq!(pac.core, Core::Arcade);
        assert_eq!(pac.file_name, "pacman.zip");

        let crash = snap
            .games
            .iter()
            .find(|g| g.title == "Crash")
            .unwrap();
        assert_eq!(crash.core, Core::Psx);

        let bios = &snap.bios["PlayStation"];
        assert_eq!(bios.len(), 1);
        assert_eq!(bios[0].file_name, "scph1001.bin");
        assert_eq!(bios[0].size_bytes, 4);
        assert!(
            !snap
                .bios
                .contains_key("SNES")
        );
    }

    #[test]
    fn ids_are_hex_stable_and_resolvable() {
        let dir = make_layout();
        let roots = [dir
            .path()
            .to_path_buf()];
        let a = scan(&roots);
        let b = scan(&roots);

        let ids_a: Vec<&str> = a
            .games
            .iter()
            .map(|g| {
                g.id.as_str()
            })
            .collect();
        let ids_b: Vec<&str> = b
            .games
            .iter()
            .map(|g| {
                g.id.as_str()
            })
            .collect();
        assert_eq!(ids_a, ids_b);

        let re = regex::Regex::new("^[0-9a-f]+$").unwrap();
        for g in &a.games {
            assert!(re.is_match(&g.id), "{}", g.id);
            assert_eq!(
                a.game(&g.id)
                    .unwrap()
                    .path,
                g.path
            );
        }
        let bios = &a.bios["PlayStation"][0];
        assert!(re.is_match(&bios.id));
        assert_eq!(
            a.bios_file(&bios.id)
                .unwrap()
                .file_name,
            "scph1001.bin"
        );
        // Game ids never resolve as BIOS and vice versa; junk ids miss.
        assert!(
            a.game(&bios.id)
                .is_none()
        );
        assert!(
            a.bios_file(&a.games[0].id)
                .is_none()
        );
        assert!(
            a.game("../etc/passwd")
                .is_none()
        );
    }

    #[test]
    fn same_tree_under_different_root_index_gets_different_ids() {
        let dir = make_layout();
        let p = dir
            .path()
            .to_path_buf();
        let one = scan(&[p.clone()]);
        let two = scan(&[PathBuf::from("/definitely/not/here"), p]);
        assert_eq!(
            one.games
                .len(),
            two.games
                .len()
        );
        assert_ne!(one.games[0].id, two.games[0].id);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_escaping_root_are_skipped() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        write(
            outside
                .path()
                .join("secret.sfc"),
            b"secret",
        )
        .unwrap();
        create_dir_all(
            outside
                .path()
                .join("Evil Game"),
        )
        .unwrap();
        write(
            outside
                .path()
                .join("Evil Game/evil.sfc"),
            b"evil",
        )
        .unwrap();

        let root = tempfile::tempdir().unwrap();
        create_dir_all(
            root.path()
                .join("SNES"),
        )
        .unwrap();
        write(
            root.path()
                .join("SNES/ok.sfc"),
            b"ok",
        )
        .unwrap();
        // file symlink escaping, game-folder symlink escaping, whole-system symlink escaping
        symlink(
            outside
                .path()
                .join("secret.sfc"),
            root.path()
                .join("SNES/secret.sfc"),
        )
        .unwrap();
        symlink(
            outside
                .path()
                .join("Evil Game"),
            root.path()
                .join("SNES/Evil Game"),
        )
        .unwrap();
        symlink(
            outside.path(),
            root.path()
                .join("GBA"),
        )
        .unwrap();
        // in-root symlink is fine
        symlink(
            root.path()
                .join("SNES/ok.sfc"),
            root.path()
                .join("SNES/alias.sfc"),
        )
        .unwrap();

        let snap = scan(&[root
            .path()
            .to_path_buf()]);
        let canon_root = root
            .path()
            .canonicalize()
            .unwrap();
        assert!(
            !snap
                .games
                .is_empty()
        );
        for g in &snap.games {
            assert!(
                g.path
                    .starts_with(&canon_root),
                "{:?}",
                g.path
            );
            assert_ne!(g.title, "secret");
            assert_ne!(g.title, "Evil Game");
        }
        assert!(
            snap.games
                .iter()
                .all(|g| g.system == "SNES")
        );
    }

    fn zip_at(path: &Path, entries: &[(&str, &[u8])]) {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            w.start_file(*name, opts)
                .unwrap();
            w.write_all(data)
                .unwrap();
        }
        w.finish()
            .unwrap();
    }

    #[test]
    fn archives_are_inspected_for_non_arcade_cores_only() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        create_dir_all(r.join("Mystery Box")).unwrap();
        create_dir_all(r.join("Arcade")).unwrap();
        create_dir_all(r.join("SNES")).unwrap();
        // Unrecognised folder: the inner extension picks the core.
        zip_at(
            &r.join("Mystery Box/Advance Wars.zip"),
            &[("readme.txt", b"hi"), ("aw.gba", b"gba-bytes")],
        );
        zip_at(&r.join("Arcade/pacman.zip"), &[("pacman.6e", b"x")]);
        zip_at(&r.join("SNES/Empty.zip"), &[("readme.txt", b"")]);
        write(r.join("SNES/Broken.zip"), b"nope").unwrap();

        let snap = scan(&[r.to_path_buf()]);
        let by = |t: &str| {
            snap.games
                .iter()
                .find(|g| g.title == t)
                .unwrap()
        };

        let aw = by("Advance Wars");
        assert_eq!(aw.core, Core::Gba);
        assert_eq!(aw.file_name, "Advance Wars.zip");
        assert_eq!(
            aw.inner_rom,
            Some(ArchiveRom {
                name: "aw.gba".into(),
                size: 9
            })
        );
        assert_eq!(aw.served_file_name(), "aw.gba");
        assert_eq!(aw.served_size_bytes(), 9);

        let pac = by("pacman");
        assert_eq!(pac.core, Core::Arcade);
        assert_eq!(pac.inner_rom, None);
        assert_eq!(pac.served_file_name(), "pacman.zip");

        for t in ["Empty", "Broken"] {
            let g = by(t);
            assert_eq!(g.core, Core::Snes);
            assert_eq!(g.inner_rom, None, "{t}");
            assert_eq!(g.served_file_name(), g.file_name);
            assert_eq!(g.served_size_bytes(), g.size_bytes);
        }
    }

    #[test]
    fn missing_root_yields_empty_snapshot() {
        let snap = scan(&[PathBuf::from("/definitely/not/here")]);
        assert!(
            snap.games
                .is_empty()
        );
        assert!(
            snap.bios
                .is_empty()
        );
    }
}
