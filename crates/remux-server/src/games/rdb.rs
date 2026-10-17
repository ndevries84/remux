//! Reader and index for libretro `.rdb` game databases.
//!
//! Format (RetroArch's `libretrodb`): the 8-byte magic `RARCHDB\0`, a
//! big-endian `u64` holding the offset of the trailing metadata block, then
//! back-to-back MessagePack maps (one per game) up to that offset. Only the
//! MessagePack subset the files use is understood; everything else is skipped
//! structurally.
//!
//! Files come from the network, so the reader is defensive: every length is
//! bounds-checked before it is used, nothing is allocated from a declared
//! length, nesting and record counts are capped, and any malformed input is a
//! [`RdbError`] rather than a panic.
//!
//! The data itself is the libretro-database project's
//! (<https://github.com/libretro/libretro-database>, CC-BY-SA-4.0); it is only
//! downloaded at runtime, never bundled.
//!
//! One game usually has several records sharing a CRC (a bare No-Intro record
//! plus the one carrying genre/developer/publisher), in no fixed order, so the
//! [`RdbIndex`] merges them per CRC (and per name) instead of keeping the
//! first or the last.

use std::{collections::HashMap, path::Path};

use super::names;

/// Largest `.rdb` accepted (download and parse).
pub const MAX_RDB_BYTES: usize = 64 * 1024 * 1024;
const MAGIC: &[u8; 8] = b"RARCHDB\0";
const HEADER_LEN: usize = 16;
/// Games per file we are willing to hold (the biggest real files have well
/// under 200k).
const MAX_RECORDS: usize = 1_000_000;
/// Keys per record map.
const MAX_FIELDS: usize = 128;
/// Nesting depth tolerated while skipping values we do not read.
const MAX_DEPTH: u32 = 16;
/// Longest text kept per field (longer values are cut).
const MAX_TEXT_BYTES: usize = 1024;
/// Shortest normalized name taken part in prefix matching.
const MIN_PREFIX_LEN: usize = 5;
/// Largest length difference tolerated by prefix matching.
const MAX_PREFIX_GAP: usize = 45;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RdbError {
    #[error("database is over the {MAX_RDB_BYTES} byte limit")]
    TooLarge,
    #[error("missing RARCHDB header")]
    BadMagic,
    #[error("metadata offset is outside the file")]
    BadOffset,
    #[error("database is truncated")]
    Truncated,
    #[error("malformed record: {0}")]
    Malformed(&'static str),
    #[error("more than {MAX_RECORDS} records")]
    TooManyRecords,
}

/// What a database can say about a game; every field is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameMeta {
    pub genre: Option<String>,
    pub developer: Option<String>,
    pub publisher: Option<String>,
    pub franchise: Option<String>,
    pub region: Option<String>,
    pub year: Option<u16>,
    pub players: Option<u8>,
    /// Description text; only LaunchBox supplies one.
    pub overview: Option<String>,
    /// Community rating; only LaunchBox supplies one.
    pub rating: Option<Rating>,
}

/// A community rating on the 0 to 5 scale, kept in hundredths so the
/// metadata stays `Eq`. Out-of-range or non-finite input is not a rating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rating(u16);

impl Rating {
    const MAX_HUNDREDTHS: u16 = 500;

    pub fn parse(value: f64) -> Option<Self> {
        if !value.is_finite() || !(0.0..=5.0).contains(&value) {
            return None;
        }
        let h = (value * 100.0).round() as u16;
        (h <= Self::MAX_HUNDREDTHS).then_some(Self(h))
    }

    pub fn from_hundredths(h: u16) -> Option<Self> {
        (h <= Self::MAX_HUNDREDTHS).then_some(Self(h))
    }

    pub fn hundredths(self) -> u16 {
        self.0
    }

    pub fn as_f64(self) -> f64 {
        f64::from(self.0) / 100.0
    }
}

impl GameMeta {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Fills this value's gaps from `other`; never overwrites a set field.
    pub fn fill_from(&mut self, other: &GameMeta) {
        fn fill<T: Clone>(slot: &mut Option<T>, from: &Option<T>) {
            if slot.is_none() {
                slot.clone_from(from);
            }
        }
        fill(&mut self.genre, &other.genre);
        fill(&mut self.developer, &other.developer);
        fill(&mut self.publisher, &other.publisher);
        fill(&mut self.franchise, &other.franchise);
        fill(&mut self.region, &other.region);
        fill(&mut self.year, &other.year);
        fill(&mut self.players, &other.players);
        fill(&mut self.overview, &other.overview);
        fill(&mut self.rating, &other.rating);
    }
}

/// One game record as stored in the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RdbRecord {
    pub name: Option<String>,
    pub rom_name: Option<String>,
    pub crc: Option<u32>,
    pub meta: GameMeta,
}

// ---------------------------------------------------------------------------
// MessagePack subset
// ---------------------------------------------------------------------------

/// A decoded MessagePack header; containers carry only their element count.
enum Head<'a> {
    Nil,
    Bool,
    Int(i128),
    Float,
    Str(&'a [u8]),
    Bin(&'a [u8]),
    Ext,
    Map(usize),
    Array(usize),
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], RdbError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(RdbError::Truncated)?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or(RdbError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn remaining(&self) -> usize {
        self.buf
            .len()
            - self.pos
    }

    fn uint(&mut self, n: usize) -> Result<u64, RdbError> {
        Ok(self
            .take(n)?
            .iter()
            .fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
    }

    /// A length prefix of `n` bytes, converted without truncation.
    fn len(&mut self, n: usize) -> Result<usize, RdbError> {
        usize::try_from(self.uint(n)?).map_err(|_| RdbError::Truncated)
    }

    /// A container size. Every element takes at least one byte, so a count
    /// larger than what is left is a lie; rejecting it up front keeps later
    /// loops bounded by the input size.
    fn count(&mut self, n: usize) -> Result<usize, RdbError> {
        let c = self.len(n)?;
        if c > self.remaining() {
            return Err(RdbError::Truncated);
        }
        Ok(c)
    }

    fn head(&mut self) -> Result<Head<'a>, RdbError> {
        let tag = self.take(1)?[0];
        Ok(match tag {
            0x00..=0x7f => Head::Int(i128::from(tag)),
            0xe0..=0xff => Head::Int(i128::from(tag as i8)),
            0x80..=0x8f => Head::Map(self.checked_inline(usize::from(tag & 0x0f))?),
            0x90..=0x9f => Head::Array(self.checked_inline(usize::from(tag & 0x0f))?),
            0xa0..=0xbf => Head::Str(self.take(usize::from(tag & 0x1f))?),
            0xc0 => Head::Nil,
            0xc2 | 0xc3 => Head::Bool,
            0xc4 => {
                let n = self.len(1)?;
                Head::Bin(self.take(n)?)
            }
            0xc5 => {
                let n = self.len(2)?;
                Head::Bin(self.take(n)?)
            }
            0xc6 => {
                let n = self.len(4)?;
                Head::Bin(self.take(n)?)
            }
            0xc7 | 0xc8 | 0xc9 => {
                let n = self.len(1 << (tag - 0xc7))?;
                self.take(1)?; // ext type
                self.take(n)?;
                Head::Ext
            }
            0xca => {
                self.take(4)?;
                Head::Float
            }
            0xcb => {
                self.take(8)?;
                Head::Float
            }
            0xcc => Head::Int(i128::from(self.uint(1)?)),
            0xcd => Head::Int(i128::from(self.uint(2)?)),
            0xce => Head::Int(i128::from(self.uint(4)?)),
            0xcf => Head::Int(i128::from(self.uint(8)?)),
            0xd0 => Head::Int(i128::from(self.uint(1)? as u8 as i8)),
            0xd1 => Head::Int(i128::from(self.uint(2)? as u16 as i16)),
            0xd2 => Head::Int(i128::from(self.uint(4)? as u32 as i32)),
            0xd3 => Head::Int(i128::from(self.uint(8)? as i64)),
            0xd4..=0xd8 => {
                self.take(1)?; // ext type
                self.take(1 << (tag - 0xd4))?;
                Head::Ext
            }
            0xd9 => {
                let n = self.len(1)?;
                Head::Str(self.take(n)?)
            }
            0xda => {
                let n = self.len(2)?;
                Head::Str(self.take(n)?)
            }
            0xdb => {
                let n = self.len(4)?;
                Head::Str(self.take(n)?)
            }
            0xdc => Head::Array(self.count(2)?),
            0xdd => Head::Array(self.count(4)?),
            0xde => Head::Map(self.count(2)?),
            0xdf => Head::Map(self.count(4)?),
            0xc1 => return Err(RdbError::Malformed("reserved type")),
        })
    }

    fn checked_inline(&self, n: usize) -> Result<usize, RdbError> {
        if n > self.remaining() {
            return Err(RdbError::Truncated);
        }
        Ok(n)
    }

    /// Consumes one value (and everything nested in it).
    fn skip(&mut self, depth: u32) -> Result<(), RdbError> {
        if depth > MAX_DEPTH {
            return Err(RdbError::Malformed("nested too deeply"));
        }
        match self.head()? {
            Head::Map(n) => {
                for _ in 0..n {
                    self.skip(depth + 1)?;
                    self.skip(depth + 1)?;
                }
            }
            Head::Array(n) => {
                for _ in 0..n {
                    self.skip(depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn record(&mut self) -> Result<RdbRecord, RdbError> {
        let Head::Map(fields) = self.head()? else {
            return Err(RdbError::Malformed("record is not a map"));
        };
        if fields > MAX_FIELDS {
            return Err(RdbError::Malformed("too many fields"));
        }
        let mut rec = RdbRecord::default();
        for _ in 0..fields {
            let key = match self.head()? {
                Head::Str(k) => k,
                Head::Map(_) | Head::Array(_) => {
                    return Err(RdbError::Malformed("container used as key"));
                }
                _ => {
                    self.skip(0)?;
                    continue;
                }
            };
            let value = self.head()?;
            match (key, value) {
                (b"name", Head::Str(v)) => rec.name = text(v),
                (b"rom_name", Head::Str(v)) => rec.rom_name = text(v),
                (b"genre", Head::Str(v)) => {
                    rec.meta
                        .genre = text(v)
                }
                (b"developer", Head::Str(v)) => {
                    rec.meta
                        .developer = text(v)
                }
                (b"publisher", Head::Str(v)) => {
                    rec.meta
                        .publisher = text(v)
                }
                (b"franchise", Head::Str(v)) => {
                    rec.meta
                        .franchise = text(v)
                }
                (b"region", Head::Str(v)) => {
                    rec.meta
                        .region = text(v)
                }
                (b"releaseyear", Head::Int(v)) => {
                    rec.meta
                        .year = u16::try_from(v)
                        .ok()
                        .filter(|y| (1..=9999).contains(y));
                }
                (b"users", Head::Int(v)) => {
                    rec.meta
                        .players = u8::try_from(v)
                        .ok()
                        .filter(|p| *p > 0);
                }
                (b"crc", Head::Bin(v)) => {
                    rec.crc = <[u8; 4]>::try_from(v)
                        .ok()
                        .map(u32::from_be_bytes);
                }
                // A container value we do not read: consume what it holds.
                (_, Head::Map(n)) => {
                    for _ in 0..n {
                        self.skip(1)?;
                        self.skip(1)?;
                    }
                }
                (_, Head::Array(n)) => {
                    for _ in 0..n {
                        self.skip(1)?;
                    }
                }
                _ => {}
            }
        }
        Ok(rec)
    }
}

/// Trimmed text, cut at [`MAX_TEXT_BYTES`]; `None` when blank.
fn text(raw: &[u8]) -> Option<String> {
    let raw = raw
        .get(..MAX_TEXT_BYTES)
        .unwrap_or(raw);
    let s = String::from_utf8_lossy(raw);
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Parses the game records of an `.rdb` file.
pub fn parse(bytes: &[u8]) -> Result<Vec<RdbRecord>, RdbError> {
    if bytes.len() > MAX_RDB_BYTES {
        return Err(RdbError::TooLarge);
    }
    if bytes.len() < HEADER_LEN || &bytes[..MAGIC.len()] != MAGIC {
        return Err(RdbError::BadMagic);
    }
    let end = u64::from_be_bytes(
        bytes[8..16]
            .try_into()
            .map_err(|_| RdbError::BadOffset)?,
    );
    let end = usize::try_from(end)
        .ok()
        .filter(|e| (HEADER_LEN..=bytes.len()).contains(e))
        .ok_or(RdbError::BadOffset)?;

    let mut reader = Reader {
        buf: &bytes[..end],
        pos: HEADER_LEN,
    };
    let mut records = Vec::new();
    while reader.remaining() > 0 {
        if records.len() >= MAX_RECORDS {
            return Err(RdbError::TooManyRecords);
        }
        // Real files end the record region with a nil right before the
        // metadata block the offset points at.
        if reader.buf[reader.pos] == 0xc0 {
            reader.pos += 1;
            continue;
        }
        let rec = reader.record()?;
        if rec
            .crc
            .is_some()
            || rec
                .name
                .is_some()
            || !rec
                .meta
                .is_empty()
        {
            records.push(rec);
        }
    }
    Ok(records)
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

/// Name key used for matching (see [`super::names`]): case, accents,
/// punctuation, separators and articles do not matter. Text inside
/// parentheses is kept, so region variants stay distinct.
pub fn normalize_name(name: &str) -> String {
    names::key(name, names::Tags::Keep)
}

/// A `rom_name` without its file extension.
fn rom_stem(rom_name: &str) -> &str {
    match rom_name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && (1..=4).contains(&ext.len())
                && ext
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric()) =>
        {
            stem
        }
        _ => rom_name,
    }
}

/// Searchable form of one database, with records merged per CRC and name.
#[derive(Debug, Default)]
pub struct RdbIndex {
    metas: Vec<GameMeta>,
    /// Canonical (database) name of each entry, parallel to `metas`.
    names: Vec<Option<String>>,
    by_crc: HashMap<u32, u32>,
    by_name: HashMap<String, u32>,
}

impl RdbIndex {
    pub fn build(records: Vec<RdbRecord>) -> Self {
        let mut idx = Self::default();
        for rec in records {
            // A record that says nothing cannot help any lookup.
            if rec
                .meta
                .is_empty()
            {
                continue;
            }
            let mut slot: Option<u32> = None;
            let canon = rec
                .name
                .clone()
                .or_else(|| {
                    rec.rom_name
                        .as_deref()
                        .map(rom_stem)
                        .map(str::to_string)
                });
            if let Some(crc) = rec.crc {
                let i = idx.attach(
                    idx.by_crc
                        .get(&crc)
                        .copied(),
                    &rec.meta,
                    &canon,
                );
                idx.by_crc
                    .insert(crc, i);
                slot = Some(i);
            }
            let keys = [
                rec.rom_name
                    .as_deref()
                    .map(rom_stem)
                    .map(normalize_name),
                rec.name
                    .as_deref()
                    .map(normalize_name),
            ];
            let mut seen: Option<&str> = None;
            for key in keys
                .iter()
                .flatten()
                .filter(|k| !k.is_empty())
            {
                if seen == Some(key.as_str()) {
                    continue;
                }
                seen = Some(key);
                match idx
                    .by_name
                    .get(key)
                    .copied()
                {
                    Some(existing) => {
                        if slot != Some(existing) {
                            let meta = rec
                                .meta
                                .clone();
                            idx.metas[existing as usize].fill_from(&meta);
                        }
                    }
                    None => {
                        let i = match slot {
                            Some(i) => i,
                            None => {
                                let i = idx.attach(None, &rec.meta, &canon);
                                slot = Some(i);
                                i
                            }
                        };
                        idx.by_name
                            .insert(key.clone(), i);
                    }
                }
            }
        }
        idx
    }

    /// Merges `meta` into the entry at `existing`, or starts a new entry.
    fn attach(
        &mut self,
        existing: Option<u32>,
        meta: &GameMeta,
        name: &Option<String>,
    ) -> u32 {
        match existing {
            Some(i) => {
                self.metas[i as usize].fill_from(meta);
                if self.names[i as usize].is_none() {
                    self.names[i as usize].clone_from(name);
                }
                i
            }
            None => {
                self.metas
                    .push(meta.clone());
                self.names
                    .push(name.clone());
                (self
                    .metas
                    .len()
                    - 1) as u32
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.metas
            .is_empty()
    }

    /// Best match for a game: the first CRC that is known, else the first
    /// normalized name (see [`normalize_name`]) that is, else (only when
    /// `allow_prefix`) the unambiguous closest prefix relative of a name.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn lookup(
        &self,
        crcs: &[u32],
        names: &[String],
        allow_prefix: bool,
    ) -> Option<GameMeta> {
        self.lookup_named(crcs, names, allow_prefix)
            .map(|(m, _)| m)
    }

    /// [`lookup`](Self::lookup) that also returns the matched entry's
    /// canonical database name (cleaner than a file name, so a good key for
    /// other databases).
    pub fn lookup_named(
        &self,
        crcs: &[u32],
        names: &[String],
        allow_prefix: bool,
    ) -> Option<(GameMeta, Option<String>)> {
        let pick = |i: &u32| {
            Some((
                self.metas[*i as usize].clone(),
                self.names[*i as usize].clone(),
            ))
        };
        for crc in crcs {
            if let Some(m) = self
                .by_crc
                .get(crc)
                .and_then(pick)
            {
                return Some(m);
            }
        }
        for name in names {
            if let Some(m) = self
                .by_name
                .get(name)
                .and_then(pick)
            {
                return Some(m);
            }
        }
        if allow_prefix {
            for name in names {
                if let Some(i) = self.prefix_match(name) {
                    return pick(&i);
                }
            }
        }
        None
    }

    /// The key that is a prefix of `name` (or has it as a prefix) with the
    /// smallest length gap; `None` when nothing qualifies or two different
    /// games tie.
    fn prefix_match(&self, name: &str) -> Option<u32> {
        if name.len() < MIN_PREFIX_LEN {
            return None;
        }
        let mut best: Option<(usize, u32)> = None;
        let mut tie = false;
        for (key, &i) in &self.by_name {
            if key.len() < MIN_PREFIX_LEN
                || !(key.starts_with(name) || name.starts_with(key.as_str()))
            {
                continue;
            }
            let gap = key
                .len()
                .abs_diff(name.len());
            if gap > MAX_PREFIX_GAP {
                continue;
            }
            match best {
                Some((g, _)) if gap > g => {}
                Some((g, b)) if gap == g => {
                    if b != i {
                        tie = true;
                    }
                }
                _ => {
                    best = Some((gap, i));
                    tie = false;
                }
            }
        }
        best.filter(|_| !tie)
            .map(|(_, i)| i)
    }
}

/// Normalized name candidates for a game, best first, de-duplicated: the
/// file stems then the catalog title.
pub fn name_candidates<'a>(
    stems: impl IntoIterator<Item = &'a str>,
    title: &str,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let stems = stems
        .into_iter()
        .map(|s| {
            Path::new(s)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(s)
        });
    for raw in stems.chain(std::iter::once(title)) {
        let n = normalize_name(raw);
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A tiny `.rdb` writer so tests never need real database content.

    /// A synthetic game record.
    #[derive(Debug, Clone, Default)]
    pub struct Rec {
        pub name: Option<String>,
        pub rom_name: Option<String>,
        pub crc: Option<u32>,
        pub genre: Option<String>,
        pub developer: Option<String>,
        pub publisher: Option<String>,
        pub franchise: Option<String>,
        pub region: Option<String>,
        pub year: Option<u16>,
        pub users: Option<u8>,
    }

    impl Rec {
        pub fn named(name: &str) -> Self {
            Self {
                name: Some(name.into()),
                rom_name: Some(format!("{name}.nes")),
                ..Default::default()
            }
        }
        pub fn crc(mut self, crc: u32) -> Self {
            self.crc = Some(crc);
            self
        }
        pub fn genre(mut self, v: &str) -> Self {
            self.genre = Some(v.into());
            self
        }
        pub fn developer(mut self, v: &str) -> Self {
            self.developer = Some(v.into());
            self
        }
        pub fn publisher(mut self, v: &str) -> Self {
            self.publisher = Some(v.into());
            self
        }
        pub fn franchise(mut self, v: &str) -> Self {
            self.franchise = Some(v.into());
            self
        }
        pub fn region(mut self, v: &str) -> Self {
            self.region = Some(v.into());
            self
        }
        pub fn year(mut self, v: u16) -> Self {
            self.year = Some(v);
            self
        }
        pub fn users(mut self, v: u8) -> Self {
            self.users = Some(v);
            self
        }
    }

    pub fn put_str(out: &mut Vec<u8>, s: &str) {
        let b = s.as_bytes();
        if b.len() < 32 {
            out.push(0xa0 | b.len() as u8);
        } else if b.len() < 256 {
            out.extend([0xd9, b.len() as u8]);
        } else {
            out.push(0xda);
            out.extend((b.len() as u16).to_be_bytes());
        }
        out.extend(b);
    }

    pub fn put_bin(out: &mut Vec<u8>, b: &[u8]) {
        out.extend([0xc4, b.len() as u8]);
        out.extend(b);
    }

    /// Serializes `recs` the way libretro's tooling does: header, maps with
    /// string keys, bin CRCs, int years/users and extra fields a reader must
    /// skip (`serial`, `size`, an array), then the nil + count trailer.
    pub fn build(recs: &[Rec]) -> Vec<u8> {
        let mut body = Vec::new();
        for r in recs {
            let mut fields: Vec<(&str, Vec<u8>)> = Vec::new();
            let s = |v: &str| {
                let mut o = Vec::new();
                put_str(&mut o, v);
                o
            };
            if let Some(v) = &r.name {
                fields.push(("name", s(v)));
            }
            if let Some(v) = &r.rom_name {
                fields.push(("rom_name", s(v)));
            }
            fields.push(("serial", {
                let mut o = Vec::new();
                put_bin(&mut o, b"\x01\x02\x03");
                o
            }));
            if let Some(v) = &r.genre {
                fields.push(("genre", s(v)));
            }
            if let Some(v) = &r.developer {
                fields.push(("developer", s(v)));
            }
            if let Some(v) = &r.publisher {
                fields.push(("publisher", s(v)));
            }
            if let Some(v) = &r.franchise {
                fields.push(("franchise", s(v)));
            }
            if let Some(v) = &r.region {
                fields.push(("region", s(v)));
            }
            if let Some(v) = r.year {
                let mut o = vec![0xcd];
                o.extend(v.to_be_bytes());
                fields.push(("releaseyear", o));
            }
            if let Some(v) = r.users {
                fields.push(("users", vec![0xcc, v]));
            }
            fields.push(("size", {
                let mut o = vec![0xce];
                o.extend(40976u32.to_be_bytes());
                o
            }));
            fields.push(("tags", vec![0x92, 0x01, 0xa1, b'x']));
            if let Some(c) = r.crc {
                fields.push(("crc", {
                    let mut o = Vec::new();
                    put_bin(&mut o, &c.to_be_bytes());
                    o
                }));
            }
            body.push(0xde);
            body.extend((fields.len() as u16).to_be_bytes());
            for (k, v) in fields {
                put_str(&mut body, k);
                body.extend(v);
            }
        }
        // Like the real files: a nil ends the records, and the offset points
        // just past it at the metadata map.
        body.push(0xc0);
        let end = (16 + body.len()) as u64;
        let mut out = b"RARCHDB\0".to_vec();
        out.extend(end.to_be_bytes());
        out.extend(body);
        out.extend([0x81, 0xa5]);
        out.extend(b"count");
        out.extend([0xcd, 0x00, recs.len() as u8]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::*, *};

    fn index(recs: &[Rec]) -> RdbIndex {
        RdbIndex::build(parse(&build(recs)).unwrap())
    }

    #[test]
    fn parses_synthetic_records() {
        let bytes = build(&[
            Rec::named("Alpha (USA)")
                .crc(0xDEADBEEF)
                .genre("Platform")
                .developer("Dev")
                .publisher("Pub")
                .franchise("Fran")
                .region("USA")
                .year(1988)
                .users(2),
            Rec::named("Bare").crc(1),
        ]);
        let recs = parse(&bytes).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].crc, Some(0xDEADBEEF));
        assert_eq!(
            recs[0]
                .name
                .as_deref(),
            Some("Alpha (USA)")
        );
        assert_eq!(
            recs[0].meta,
            GameMeta {
                genre: Some("Platform".into()),
                developer: Some("Dev".into()),
                publisher: Some("Pub".into()),
                franchise: Some("Fran".into()),
                region: Some("USA".into()),
                year: Some(1988),
                players: Some(2),
                ..Default::default()
            }
        );
        assert!(
            recs[1]
                .meta
                .is_empty()
        );
    }

    #[test]
    fn records_with_one_crc_merge_in_either_order() {
        let rich = Rec::named("Game (USA)")
            .crc(7)
            .genre("Puzzle")
            .developer("D")
            .year(1990);
        let bare = Rec::named("Game (USA)")
            .crc(7)
            .region("USA");
        for recs in [[rich.clone(), bare.clone()], [bare.clone(), rich.clone()]] {
            let got = index(&recs)
                .lookup(&[7], &[], false)
                .unwrap();
            assert_eq!(
                got.genre
                    .as_deref(),
                Some("Puzzle")
            );
            assert_eq!(
                got.developer
                    .as_deref(),
                Some("D")
            );
            assert_eq!(
                got.region
                    .as_deref(),
                Some("USA")
            );
            assert_eq!(got.year, Some(1990));
        }
    }

    #[test]
    fn crc_beats_name_and_first_candidate_wins() {
        let idx = index(&[
            Rec::named("One")
                .crc(1)
                .genre("A"),
            Rec::named("Two")
                .crc(2)
                .genre("B"),
        ]);
        let by = |crcs: &[u32], names: &[&str]| {
            idx.lookup(
                crcs,
                &names
                    .iter()
                    .map(|n| normalize_name(n))
                    .collect::<Vec<_>>(),
                false,
            )
            .and_then(|m| m.genre)
        };
        assert_eq!(by(&[2, 1], &["One"]).as_deref(), Some("B"));
        assert_eq!(by(&[99], &["One"]).as_deref(), Some("A"));
        assert_eq!(by(&[99], &["Nope"]), None);
    }

    #[test]
    fn name_matching_ignores_case_accents_and_punctuation() {
        let idx = index(&[Rec::named("Pokémon: Red (USA)")
            .crc(3)
            .genre("RPG")]);
        let names = name_candidates(["POKEMON red [USA].zip"], "x");
        assert_eq!(
            names,
            vec![normalize_name("pokemon red usa"), "x".to_string()]
        );
        // "[USA]" and "(USA)" normalize alike.
        assert_eq!(
            idx.lookup(&[], &names, false)
                .and_then(|m| m.genre)
                .as_deref(),
            Some("RPG")
        );
    }

    #[test]
    fn prefix_fallback_is_unambiguous_only() {
        let idx = index(&[
            Rec::named("Tetris (USA)").genre("Puzzle"),
            Rec::named("Tetris (EUR)").genre("Puzzle2"),
            Rec::named("Super Mario Land (World)").genre("Platform"),
        ]);
        let look = |n: &str| {
            idx.lookup(&[], &[normalize_name(n)], true)
                .and_then(|m| m.genre)
        };
        // Two games equally close: no answer.
        assert_eq!(look("Tetris"), None);
        // One close relative: found.
        assert_eq!(look("Super Mario Land").as_deref(), Some("Platform"));
        // Too short and not allowed without the flag.
        assert_eq!(look("Tet"), None);
        assert_eq!(
            idx.lookup(&[], &[normalize_name("Super Mario Land")], false),
            None
        );
    }

    #[test]
    fn empty_records_do_not_index() {
        let idx = index(&[Rec::named("Nothing").crc(5)]);
        assert!(idx.is_empty());
        assert_eq!(idx.lookup(&[5], &[normalize_name("Nothing")], true), None);
    }

    #[test]
    fn malformed_input_errors_without_panicking() {
        let good = build(&[Rec::named("Alpha")
            .crc(1)
            .genre("G")]);
        assert!(parse(&good).is_ok());

        assert_eq!(parse(b""), Err(RdbError::BadMagic));
        assert_eq!(
            parse(b"not an rdb at all, just text"),
            Err(RdbError::BadMagic)
        );
        // Every truncation point either parses or errors, never panics.
        for cut in 0..good.len() {
            let _ = parse(&good[..cut]);
        }
        // Truncated before the declared metadata offset.
        assert!(parse(&good[..good.len() - 25]).is_err());

        // Offset past the end of the file.
        let mut bad = good.clone();
        bad[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(parse(&bad), Err(RdbError::BadOffset));
        let mut bad = good.clone();
        bad[8..16].copy_from_slice(&3u64.to_be_bytes());
        assert_eq!(parse(&bad), Err(RdbError::BadOffset));

        // A string that claims to be 4 GiB, a map that claims 4 billion keys,
        // an array of that many elements, reserved tag, deep nesting.
        let wrap = |body: &[u8]| {
            let mut v = b"RARCHDB\0".to_vec();
            v.extend(((16 + body.len()) as u64).to_be_bytes());
            v.extend(body);
            v
        };
        assert!(
            parse(&wrap(&[
                0x81, 0xa4, b'n', b'a', b'm', b'e', 0xdb, 0xff, 0xff, 0xff, 0xff
            ]))
            .is_err()
        );
        assert!(parse(&wrap(&[0xdf, 0xff, 0xff, 0xff, 0xff])).is_err());
        assert!(
            parse(&wrap(&[0x81, 0xa1, b'x', 0xdd, 0xff, 0xff, 0xff, 0xff])).is_err()
        );
        assert!(parse(&wrap(&[0x81, 0xa1, b'x', 0xc1])).is_err());
        let mut deep = vec![0x81, 0xa1, b'x'];
        deep.extend(std::iter::repeat_n(0x91u8, 200));
        deep.push(0xc0);
        assert!(parse(&wrap(&deep)).is_err());
        // A top-level value that is not a record.
        assert!(parse(&wrap(&[0x05])).is_err());
    }

    #[test]
    fn trailing_nil_inside_the_record_region_is_tolerated() {
        // Real files put a nil just before the offset's metadata block.
        let bytes = build(&[Rec::named("A")
            .crc(1)
            .genre("G")]);
        let end = u64::from_be_bytes(
            bytes[8..16]
                .try_into()
                .unwrap(),
        ) as usize;
        assert_eq!(bytes[end - 1], 0xc0);
        assert_eq!(
            parse(&bytes)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn oversized_input_is_rejected_before_parsing() {
        let big = vec![0u8; MAX_RDB_BYTES + 1];
        assert_eq!(parse(&big), Err(RdbError::TooLarge));
    }

    #[test]
    fn out_of_range_numbers_are_dropped() {
        let mut rec = vec![0x83];
        put_str(&mut rec, "name");
        put_str(&mut rec, "Only Name");
        put_str(&mut rec, "releaseyear");
        rec.extend([0xcd, 0xff, 0xff]);
        put_str(&mut rec, "users");
        rec.extend([0xd0, 0xff]);
        let mut file = b"RARCHDB\0".to_vec();
        file.extend(((16 + rec.len()) as u64).to_be_bytes());
        file.extend(rec);
        let recs = parse(&file).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0]
                .meta
                .year,
            None
        );
        assert_eq!(
            recs[0]
                .meta
                .players,
            None
        );
    }
}
