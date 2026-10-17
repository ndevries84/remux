//! Game-name matching keys shared by the libretro and LaunchBox databases.
//!
//! Two sources rarely spell a title the same way: No-Intro file names write
//! "Legend of Zelda, The - Link's Awakening (USA)", LaunchBox writes "The
//! Legend of Zelda: Link's Awakening". A key is what survives the spelling
//! differences that do not change which game is meant:
//!
//! - case, accents, punctuation, apostrophes and every kind of separator
//!   (`:`, ` - `, `–`) are dropped, so `Chip 'n Dale - X` equals
//!   `Chip 'n Dale: X`;
//! - `&` reads as `and`;
//! - a leading article (`The`, `A`, `An`) is dropped and so is a trailing
//!   `, The` (also in front of a subtitle or a `(region)` tag), so both
//!   word orders meet;
//! - a "Vol." in front of a number is dropped (`Captain Tsubasa Vol. II` is
//!   `Captain Tsubasa II`).
//!
//! Nothing here is fuzzy: two titles that differ in any other word keep
//! different keys.

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// What to do with `(...)` and `[...]` groups (region and dump tags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tags {
    /// Keep the text, so region variants stay distinct (libretro entries).
    Keep,
    /// Remove the groups, so a file name collapses to the bare title.
    Drop,
}

/// Shortest loose key (possessive prefix removed) that may be tried.
const MIN_LOOSE_LEN: usize = 10;

/// The matching key of `raw`.
pub fn key(raw: &str, tags: Tags) -> String {
    squash(&tidy(&clean(raw, tags)))
}

/// A second, weaker key for a title that starts with a possessive brand or
/// author ("Disney's Chip 'n Dale Rescue Rangers", "David Crane's A Boy and
/// His Blob"): the key of the title without that prefix. `None` when there is
/// no such prefix or what is left is too short to be a safe key.
pub fn loose_key(raw: &str, tags: Tags) -> Option<String> {
    let text = clean(raw, tags);
    let rest = strip_possessive(&text)?;
    let k = squash(&tidy(rest));
    (k.len() >= MIN_LOOSE_LEN).then_some(k)
}

/// Lowercase, accent-free text with `&` spelled out, curly apostrophes made
/// plain and (for [`Tags::Drop`]) bracketed groups removed. Word structure and
/// separators are kept for the article rules.
fn clean(raw: &str, tags: Tags) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    let mut depth = 0usize;
    for ch in raw
        .nfd()
        .filter(|c| !is_combining_mark(*c))
    {
        match ch {
            '(' | '[' if tags == Tags::Drop => depth += 1,
            ')' | ']' if tags == Tags::Drop => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            '&' => out.push_str(" and "),
            '\u{2018}' | '\u{2019}' => out.push('\''),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// Article and "Vol." rules on cleaned text.
fn tidy(text: &str) -> String {
    let moved = drop_trailing_articles(text);
    let moved = drop_volume_words(&moved);
    drop_leading_article(&moved).to_string()
}

/// Letters and digits only.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

const ARTICLES: [&str; 3] = ["the", "an", "a"];

fn is_separator(c: char) -> bool {
    matches!(c, '-' | '\u{2013}' | '\u{2014}' | ':' | '(' | '[')
}

/// Removes `, the` / `, a` / `, an` when it ends the title or is followed by
/// a subtitle separator or a tag.
fn drop_trailing_articles(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(',') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match article_len(after) {
            Some(n) => rest = &after[n..],
            None => {
                out.push(',');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Bytes of ` the` in `after` (the text following a comma) when it is a whole
/// article standing at the end of its part of the title.
fn article_len(after: &str) -> Option<usize> {
    let word = after.trim_start();
    let skipped = after.len() - word.len();
    ARTICLES
        .iter()
        .find_map(|art| {
            let tail = word.strip_prefix(art)?;
            if tail
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
            {
                return None;
            }
            let next = tail
                .trim_start()
                .chars()
                .next();
            match next {
                None => Some(skipped + art.len()),
                Some(c) if is_separator(c) => Some(skipped + art.len()),
                _ => None,
            }
        })
}

/// Drops a leading `the` / `a` / `an` when more title follows.
fn drop_leading_article(text: &str) -> &str {
    let t = text.trim_start();
    for art in ARTICLES {
        if let Some(rest) = t.strip_prefix(art)
            && rest.starts_with(char::is_whitespace)
            && rest
                .trim_start()
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
        {
            return rest.trim_start();
        }
    }
    t
}

/// Whether `word` is a number: digits or a small roman numeral.
fn is_number(word: &str) -> bool {
    let w = word.trim_matches(|c: char| !c.is_alphanumeric());
    !w.is_empty()
        && (w
            .chars()
            .all(|c| c.is_ascii_digit())
            || (w.len() <= 5
                && w.chars()
                    .all(|c| matches!(c, 'i' | 'v' | 'x'))))
}

/// "vol. II" / "volume 2" in front of a number reads as just the number.
fn drop_volume_words(text: &str) -> String {
    let words: Vec<&str> = text
        .split_whitespace()
        .collect();
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    for (i, w) in words
        .iter()
        .enumerate()
    {
        let bare = w.trim_end_matches('.');
        if (bare == "vol" || bare == "volume")
            && words
                .get(i + 1)
                .is_some_and(|n| is_number(n))
        {
            continue;
        }
        out.push(w);
    }
    out.join(" ")
}

/// The text after a leading possessive ("disney's ", "sid meier's ").
fn strip_possessive(text: &str) -> Option<&str> {
    let t = text.trim_start();
    for (n, (end, _)) in t
        .match_indices(char::is_whitespace)
        .enumerate()
    {
        if n >= 2 {
            break;
        }
        if t[..end].ends_with("'s") {
            let rest = t[end..].trim_start();
            return (!rest.is_empty()).then_some(rest);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> String {
        key(s, Tags::Drop)
    }

    #[test]
    fn case_accents_and_punctuation_are_ignored() {
        assert_eq!(k("Pokémon: Red Version"), "pokemonredversion");
        assert_eq!(k("Tetris [!]"), "tetris");
        assert_eq!(k("(unbalanced"), "");
        assert_eq!(k("x) y"), "xy");
        // Tags are kept (and so region variants stay apart) when asked.
        assert_eq!(key("Tetris (USA)", Tags::Keep), "tetrisusa");
        assert_ne!(
            key("Tetris (USA)", Tags::Keep),
            key("Tetris (Europe)", Tags::Keep)
        );
    }

    #[test]
    fn trailing_and_leading_articles_meet() {
        let want = k("The Legend of Zelda");
        assert_eq!(k("Legend of Zelda, The (USA) (Rev 1)"), want);
        assert_eq!(
            k("Battle of Olympus, The (USA)"),
            k("The Battle of Olympus")
        );
        assert_eq!(k("Bard's Tale, A"), k("A Bard's Tale"));
        assert_eq!(k("Boy and His Blob, A"), k("Boy and His Blob"));
        assert_eq!(k("Goonies II, The"), k("The Goonies II"));
        // Tags kept: the article still goes, the region stays.
        assert_eq!(
            key("Legend of Zelda, The (USA)", Tags::Keep),
            key("The Legend of Zelda (USA)", Tags::Keep)
        );
    }

    #[test]
    fn article_before_a_subtitle_is_handled() {
        assert_eq!(
            k("Legend of Zelda, The - Link's Awakening (USA)"),
            k("The Legend of Zelda: Link's Awakening")
        );
        assert_eq!(
            k("Flintstones, The - The Rescue of Dino & Hoppy (USA)"),
            k("The Flintstones: The Rescue of Dino and Hoppy")
        );
        assert_eq!(
            k("Jetsons, The - Cogswell's Caper"),
            k("The Jetsons: Cogswell's Caper")
        );
    }

    #[test]
    fn words_that_only_look_like_articles_stay() {
        // A comma that is not an article, and articles inside words.
        assert_ne!(k("Hello, Theo"), k("Hello"));
        assert_eq!(k("Theme Park"), "themepark");
        assert_eq!(k("Another World"), "anotherworld");
        assert_eq!(k("A-Train"), "atrain");
        // A lone article is a title, not an article.
        assert_eq!(k("The"), "the");
    }

    #[test]
    fn separators_are_equivalent() {
        let a = k("Chip 'n Dale - Rescue Rangers 2 (USA)");
        assert_eq!(a, k("Chip 'n Dale: Rescue Rangers 2"));
        assert_eq!(a, k("Chip \u{2019}n Dale \u{2013} Rescue Rangers 2"));
        assert_eq!(a, k("Chip n Dale Rescue Rangers 2"));
    }

    #[test]
    fn ampersand_reads_as_and() {
        assert_eq!(
            k("Town & Country Surf Designs"),
            k("Town and Country Surf Designs")
        );
        assert_eq!(k("Dino & Hoppy"), k("Dino and Hoppy"));
    }

    #[test]
    fn volume_words_before_a_number_are_dropped() {
        assert_eq!(
            k("Captain Tsubasa Vol. II - Super Striker"),
            k("Captain Tsubasa II: Super Striker")
        );
        assert_eq!(k("Dragon Volume 3"), k("Dragon 3"));
        // Not before a word.
        assert_ne!(k("Vol Au Vent"), k("Au Vent"));
    }

    #[test]
    fn similar_but_different_games_stay_distinct() {
        assert_ne!(
            k("Zelda II - The Adventure of Link"),
            k("The Legend of Zelda")
        );
        assert_ne!(k("Rescue Rangers"), k("Rescue Rangers 2"));
        assert_ne!(k("Contra"), k("Super C"));
        assert_ne!(k("Adventure Island II"), k("Adventure Island IV"));
        // Roman numerals are not folded into digits.
        assert_ne!(k("Final Fantasy II"), k("Final Fantasy 2"));
        // "The" in the middle of a title is part of it.
        assert_ne!(k("Mario the Plumber"), k("Mario Plumber"));
    }

    #[test]
    fn loose_key_drops_a_possessive_prefix() {
        assert_eq!(
            loose_key("Disney's Chip 'n Dale Rescue Rangers", Tags::Drop).as_deref(),
            Some(k("Chip 'n Dale Rescue Rangers").as_str())
        );
        assert_eq!(
            loose_key(
                "David Crane's A Boy and His Blob - Trouble on Blobolonia (USA)",
                Tags::Drop
            ),
            Some(k("A Boy and His Blob: Trouble on Blobolonia"))
        );
        assert_eq!(
            loose_key("Sid Meier's Pirates! Gold", Tags::Drop).as_deref(),
            Some("piratesgold")
        );
        // No possessive, or too little left to be safe.
        assert_eq!(loose_key("Super Mario Bros.", Tags::Drop), None);
        assert_eq!(loose_key("Mario's Picross", Tags::Drop), None);
        assert_eq!(loose_key("Hudson's", Tags::Drop), None);
    }
}
