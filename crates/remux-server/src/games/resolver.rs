//! System-folder / ROM-extension to EmulatorJS core resolution.
//!
//! Ported from Moonbase's `GameSystemCoreResolver.cs`. Playback core selection
//! is deliberately strict and single-result: a wrong core means nothing plays.

use std::path::Path;

use serde::Serialize;
use strum_macros::{Display, EnumString};

/// EmulatorJS / libretro core identifiers, serialized exactly as the Moonfin
/// client expects them (note the camelCase `segaMD` style ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, Serialize)]
pub enum Core {
    #[strum(serialize = "nes")]
    #[serde(rename = "nes")]
    Nes,
    #[strum(serialize = "snes")]
    #[serde(rename = "snes")]
    Snes,
    #[strum(serialize = "gb")]
    #[serde(rename = "gb")]
    Gb,
    #[strum(serialize = "gba")]
    #[serde(rename = "gba")]
    Gba,
    #[strum(serialize = "segaMD")]
    #[serde(rename = "segaMD")]
    SegaMd,
    #[strum(serialize = "segaMS")]
    #[serde(rename = "segaMS")]
    SegaMs,
    #[strum(serialize = "segaGG")]
    #[serde(rename = "segaGG")]
    SegaGg,
    #[strum(serialize = "n64")]
    #[serde(rename = "n64")]
    N64,
    #[strum(serialize = "nds")]
    #[serde(rename = "nds")]
    Nds,
    #[strum(serialize = "vb")]
    #[serde(rename = "vb")]
    Vb,
    #[strum(serialize = "atari2600")]
    #[serde(rename = "atari2600")]
    Atari2600,
    #[strum(serialize = "atari7800")]
    #[serde(rename = "atari7800")]
    Atari7800,
    #[strum(serialize = "lynx")]
    #[serde(rename = "lynx")]
    Lynx,
    #[strum(serialize = "ws")]
    #[serde(rename = "ws")]
    Ws,
    #[strum(serialize = "ngp")]
    #[serde(rename = "ngp")]
    Ngp,
    #[strum(serialize = "pce")]
    #[serde(rename = "pce")]
    Pce,
    #[strum(serialize = "psx")]
    #[serde(rename = "psx")]
    Psx,
    #[strum(serialize = "psp")]
    #[serde(rename = "psp")]
    Psp,
    #[strum(serialize = "mame")]
    #[serde(rename = "mame")]
    Mame,
    #[strum(serialize = "arcade")]
    #[serde(rename = "arcade")]
    Arcade,
}

impl Core {
    /// Core used when nothing about a system folder could be recognised.
    pub const DEFAULT: Core = Core::Nes;

    /// MAME-family cores only play `.zip` archives, never loose ROM files.
    pub fn is_arcade_family(self) -> bool {
        matches!(self, Core::Mame | Core::Arcade)
    }
}

/// Lowercase ROM extension (without dot) -> core.
const EXTENSION_TO_CORE: &[(&str, Core)] = &[
    ("nes", Core::Nes),
    ("fds", Core::Nes),
    ("sfc", Core::Snes),
    ("smc", Core::Snes),
    ("gb", Core::Gb),
    ("gbc", Core::Gb),
    ("gba", Core::Gba),
    ("md", Core::SegaMd),
    ("gen", Core::SegaMd),
    ("smd", Core::SegaMd),
    ("sms", Core::SegaMs),
    ("gg", Core::SegaGg),
    ("sg", Core::SegaMs),
    ("n64", Core::N64),
    ("z64", Core::N64),
    ("v64", Core::N64),
    ("nds", Core::Nds),
    ("vb", Core::Vb),
    ("a26", Core::Atari2600),
    ("a78", Core::Atari7800),
    ("lnx", Core::Lynx),
    ("ws", Core::Ws),
    ("wsc", Core::Ws),
    ("ngp", Core::Ngp),
    // Moonbase maps ".ngc" to Neo Geo Pocket, although it is more commonly a
    // GameCube image. Kept byte-for-byte for parity; Moonbase's own review
    // flags this as an unanswered open question.
    ("ngc", Core::Ngp),
    ("pce", Core::Pce),
    ("chd", Core::Psx),
    ("pbp", Core::Psx),
    ("cso", Core::Psp),
    ("iso", Core::Psp),
];

/// Normalized (lowercase alphanumerics only) system folder alias -> core.
const SYSTEM_NAME_TO_CORE: &[(&str, Core)] = &[
    ("nes", Core::Nes),
    ("famicom", Core::Nes),
    ("snes", Core::Snes),
    ("superfamicom", Core::Snes),
    ("supernintendo", Core::Snes),
    ("gb", Core::Gb),
    ("gameboy", Core::Gb),
    ("gbc", Core::Gb),
    ("gameboycolor", Core::Gb),
    ("gba", Core::Gba),
    ("gameboyadvance", Core::Gba),
    ("genesis", Core::SegaMd),
    ("megadrive", Core::SegaMd),
    ("segagenesis", Core::SegaMd),
    ("mastersystem", Core::SegaMs),
    ("sms", Core::SegaMs),
    ("gamegear", Core::SegaGg),
    ("gg", Core::SegaGg),
    ("n64", Core::N64),
    ("nintendo64", Core::N64),
    ("nds", Core::Nds),
    ("nintendods", Core::Nds),
    ("virtualboy", Core::Vb),
    ("atari2600", Core::Atari2600),
    ("atari7800", Core::Atari7800),
    ("lynx", Core::Lynx),
    ("wonderswan", Core::Ws),
    ("neogeopocket", Core::Ngp),
    ("pcengine", Core::Pce),
    ("turbografx16", Core::Pce),
    ("psx", Core::Psx),
    ("ps1", Core::Psx),
    ("psone", Core::Psx),
    ("playstation", Core::Psx),
    ("psp", Core::Psp),
    ("playstationportable", Core::Psp),
    ("mame", Core::Mame),
    ("arcade", Core::Arcade),
];

const BIOS_EXTENSIONS: &[&str] = &["bin", "bios", "rom", "img", "sys", "bs"];
// TODO(games spike): archives are served as a single opaque file; extraction
// of the inner ROM (needed for non-arcade cores) is not implemented yet.
const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "7z"];

fn extension_lower(path: &Path) -> Option<String> {
    path.extension()
        .map(|e| {
            e.to_string_lossy()
                .to_ascii_lowercase()
        })
}

fn ext_in(path: &Path, set: &[&str]) -> bool {
    extension_lower(path).is_some_and(|e| set.contains(&e.as_str()))
}

pub fn is_archive(path: &Path) -> bool {
    ext_in(path, ARCHIVE_EXTENSIONS)
}

pub fn is_known_rom_extension(path: &Path) -> bool {
    core_for_extension(path).is_some()
}

/// A ROM is any known ROM extension or a supported archive.
pub fn is_rom_file(path: &Path) -> bool {
    is_known_rom_extension(path) || is_archive(path)
}

pub fn is_bios_file(path: &Path) -> bool {
    ext_in(path, BIOS_EXTENSIONS)
}

/// Whether `path` can be launched on `core`: arcade-family cores need `.zip`.
pub fn is_playable_for_core(core: Core, path: &Path) -> bool {
    is_rom_file(path)
        && (!core.is_arcade_family() || extension_lower(path).as_deref() == Some("zip"))
}

pub fn core_for_extension(path: &Path) -> Option<Core> {
    let ext = extension_lower(path)?;
    EXTENSION_TO_CORE
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, c)| *c)
}

/// Lowercase and keep only alphanumerics: "Super Nintendo" -> "supernintendo".
pub fn normalize_system_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_fuzzy_match(normalized: &str, alias: &str) -> bool {
    alias.len() >= 3 && (normalized.contains(alias) || alias.contains(normalized))
}

fn exact_core(normalized: &str) -> Option<Core> {
    SYSTEM_NAME_TO_CORE
        .iter()
        .find(|(a, _)| *a == normalized)
        .map(|(_, c)| *c)
}

/// Fuzzy alias match; `None` when nothing matches or the closest-length
/// candidates disagree on the core.
fn fuzzy_core(normalized: &str) -> Option<Core> {
    if normalized.len() < 3 {
        return None;
    }
    let mut best: Option<(Core, usize)> = None;
    let mut ambiguous = false;
    for (alias, core) in SYSTEM_NAME_TO_CORE {
        if !is_fuzzy_match(normalized, alias) {
            continue;
        }
        let diff = alias
            .len()
            .abs_diff(normalized.len());
        match best {
            None => best = Some((*core, diff)),
            Some((_, d)) if diff < d => {
                best = Some((*core, diff));
                ambiguous = false;
            }
            Some((c, d)) if diff == d && c != *core => ambiguous = true,
            _ => {}
        }
    }
    if ambiguous {
        None
    } else {
        best.map(|(c, _)| c)
    }
}

/// Resolution of a system folder name to a core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub core: Core,
    /// True when nothing recognised the folder and [`Core::DEFAULT`] was used.
    pub defaulted: bool,
}

/// Name only (exact alias, then fuzzy alias); no extension fallback.
pub fn core_for_system_name(system_name: &str) -> Option<Core> {
    let normalized = normalize_system_name(system_name);
    exact_core(&normalized).or_else(|| fuzzy_core(&normalized))
}

/// Exact alias, fuzzy alias, extension of the first recognised ROM, then
/// [`Core::DEFAULT`].
pub fn resolve_system_core<P: AsRef<Path>>(
    system_name: &str,
    roms: &[P],
) -> Resolution {
    if let Some(core) = core_for_system_name(system_name) {
        return Resolution {
            core,
            defaulted: false,
        };
    }
    if let Some(core) = roms
        .iter()
        .find_map(|r| core_for_extension(r.as_ref()))
    {
        return Resolution {
            core,
            defaulted: false,
        };
    }
    Resolution {
        core: Core::DEFAULT,
        defaulted: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const NO_ROMS: &[PathBuf] = &[];

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_system_name("Super Nintendo"), "supernintendo");
        assert_eq!(normalize_system_name("Sega-Mega_Drive"), "segamegadrive");
    }

    #[test]
    fn exact_aliases_resolve() {
        for (name, core) in [
            ("NES", Core::Nes),
            ("nes", Core::Nes),
            ("Super Nintendo", Core::Snes),
            ("Mega Drive", Core::SegaMd),
            ("Game-Boy Advance", Core::Gba),
            ("PlayStation", Core::Psx),
            ("TurboGrafx-16", Core::Pce),
            ("MAME", Core::Mame),
            ("Arcade", Core::Arcade),
        ] {
            let r = resolve_system_core(name, NO_ROMS);
            assert_eq!(r.core, core, "{name}");
            assert!(!r.defaulted, "{name}");
        }
    }

    #[test]
    fn fuzzy_alias_matches_longer_folder_names() {
        let r = resolve_system_core("Sega Genesis Collection", NO_ROMS);
        assert_eq!(r.core, Core::SegaMd);
        assert!(!r.defaulted);
    }

    #[test]
    fn short_names_never_fuzzy_match() {
        assert_eq!(fuzzy_core("ab"), None);
    }

    #[test]
    fn long_name_without_alias_uses_extension_fallback() {
        let roms = [PathBuf::from("/x/Mario.SMC")];
        let r = resolve_system_core("Nintendo Entertainment System", &roms);
        assert_eq!(r.core, Core::Snes);
        assert!(!r.defaulted);

        let roms = [PathBuf::from("/x/Zelda.nes")];
        let r = resolve_system_core("Nintendo Entertainment System", &roms);
        assert_eq!(r.core, Core::Nes);
        assert!(!r.defaulted);
    }

    #[test]
    fn unknown_everything_defaults_to_nes() {
        let roms = [PathBuf::from("/x/thing.zip")];
        let r = resolve_system_core("Totally Unknown Box", &roms);
        assert_eq!(r.core, Core::DEFAULT);
        assert!(r.defaulted);
    }

    #[test]
    fn core_ids_round_trip_as_strings() {
        assert_eq!(Core::SegaMd.to_string(), "segaMD");
        assert_eq!(
            "segaGG"
                .parse::<Core>()
                .unwrap(),
            Core::SegaGg
        );
        assert_eq!(serde_json::to_string(&Core::SegaMs).unwrap(), "\"segaMS\"");
        assert_eq!(Core::Atari7800.to_string(), "atari7800");
    }

    #[test]
    fn extension_classification() {
        assert!(is_rom_file(Path::new("a.SFC")));
        assert!(is_rom_file(Path::new("a.zip")));
        assert!(is_rom_file(Path::new("a.7z")));
        assert!(!is_rom_file(Path::new("a.txt")));
        assert!(!is_rom_file(Path::new("noext")));
        assert!(is_bios_file(Path::new("scph1001.BIN")));
        assert!(!is_bios_file(Path::new("a.nes")));
        assert!(is_archive(Path::new("a.7z")));
    }

    #[test]
    fn arcade_cores_only_play_zip() {
        assert!(is_playable_for_core(Core::Mame, Path::new("pacman.zip")));
        assert!(!is_playable_for_core(Core::Mame, Path::new("pacman.7z")));
        assert!(!is_playable_for_core(Core::Arcade, Path::new("x.nes")));
        assert!(is_playable_for_core(Core::Snes, Path::new("x.7z")));
        assert!(!is_playable_for_core(Core::Snes, Path::new("x.txt")));
    }
}
