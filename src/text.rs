//! Text handling that has to match LRCLIB's server byte for byte: the normalization of names (lookups compare
//! normalized names), and the LRC parsing behind plain lyrics derived from synced ones.
//!
//! Ported from LRCLIB (<https://github.com/tranxuanthang/lrclib>, MIT, Copyright (c) 2024 tranxuanthang):
//! `server/src/utils.rs` and `server/src/lyricsfile.rs`.

use std::sync::LazyLock;

use aho_corasick::AhoCorasick;
use regex::Regex;

const TO_SPACE: &[&str] = &[
    "`", "~", "!", "@", "#", "$", "%", "^", "&", "*", "(", ")", "_", "|", "+", "-", "=", "?", ";",
    ":", "\"", ",", ".", "<", ">", "{", "}", "[", "]", "\\", "/", "\x00", "\n",
];
const TO_REMOVE: &[&str] = &["'", "\u{2019}"];

#[allow(clippy::expect_used)] // constant patterns
static TO_SPACE_AC: LazyLock<AhoCorasick> =
    LazyLock::new(|| AhoCorasick::new(TO_SPACE).expect("valid patterns"));
#[allow(clippy::expect_used)] // constant patterns
static TO_REMOVE_AC: LazyLock<AhoCorasick> =
    LazyLock::new(|| AhoCorasick::new(TO_REMOVE).expect("valid patterns"));

#[allow(clippy::expect_used)] // constant pattern
static TIMESTAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(\d+):(\d{1,2})(?:[\.:](\d{1,3}))?\]").expect("valid regex"));
#[allow(clippy::expect_used)] // constant pattern
static ID_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^:\]]+):([^\]]*)\]").expect("valid regex"));

/// LRCLIB's normalization of track, artist and album names: lowercased, diacritics and punctuation removed,
/// whitespace collapsed (`prepare_input`).
#[must_use]
pub fn normalize(input: &str) -> String {
    let s = secular::lower_lay_string(input);
    let s = TO_SPACE_AC.replace_all(&s, &[" "; TO_SPACE.len()]);
    let s = TO_REMOVE_AC.replace_all(&s, &[""; TO_REMOVE.len()]);
    collapse::collapse(&s.to_lowercase())
}

/// A request parameter, normalized; `None` when absent or nothing is left of it (`process_param`).
pub fn normalize_param(param: Option<&str>) -> Option<String> {
    param.map(normalize).filter(|s| !s.trim().is_empty())
}

/// `Some` only for text that isn't blank.
#[must_use]
pub fn non_blank(value: Option<&str>) -> Option<&str> {
    value.filter(|s| !s.trim().is_empty())
}

/// The plain lyrics LRCLIB derives from synced ones: timestamps and ID tag lines removed (`plain_lyrics_from_lrc`).
#[must_use]
pub fn plain_from_lrc(synced: &str) -> Option<String> {
    let normalized = synced.replace("\r\n", "\n");
    let plain = normalized
        .split('\n')
        .filter_map(|line| {
            let trimmed = line.trim();
            let (timestamps, remaining) = timestamp_prefix(trimmed);
            if !timestamps.is_empty() {
                Some(remaining.trim_start())
            } else if is_id_tag_line(trimmed) {
                None
            } else {
                Some(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    non_blank(Some(&plain)).map(str::to_owned)
}

/// A line's leading timestamps (in milliseconds), and the rest of the line.
pub fn timestamp_prefix(line: &str) -> (Vec<i64>, &str) {
    let mut timestamps = Vec::new();
    let mut remaining = line;
    while let Some(captures) = TIMESTAMP.captures(remaining) {
        let (Some(full), Some(ms)) = (captures.get(0), timestamp_ms(&captures)) else {
            break;
        };
        timestamps.push(ms);
        remaining = remaining.get(full.end()..).unwrap_or_default();
    }
    (timestamps, remaining)
}

fn timestamp_ms(captures: &regex::Captures<'_>) -> Option<i64> {
    let minutes = captures.get(1)?.as_str().parse::<i64>().ok()?;
    let seconds = captures.get(2)?.as_str().parse::<i64>().ok()?;
    let milliseconds = captures.get(3).map_or(Some(0), |fraction| {
        let value = fraction.as_str().parse::<i64>().ok()?;
        match fraction.as_str().len() {
            1 => value.checked_mul(100),
            2 => value.checked_mul(10),
            3 => Some(value),
            _ => None,
        }
    })?;
    minutes
        .checked_mul(60_000)?
        .checked_add(seconds.checked_mul(1_000)?)?
        .checked_add(milliseconds)
}

fn is_id_tag_line(line: &str) -> bool {
    ID_TAG
        .find(line)
        .is_some_and(|tag| tag.start() == 0 && tag.end() == line.len())
}

/// Whether synced lyrics carry the `[au: instrumental]` tag (`is_instrumental_lrc`).
#[cfg(test)]
pub fn is_instrumental_lrc(lyrics: &str) -> bool {
    ID_TAG.captures_iter(lyrics).any(|c| {
        c.get(1)
            .is_some_and(|k| k.as_str().trim().eq_ignore_ascii_case("au"))
            && c.get(2)
                .is_some_and(|v| v.as_str().trim().eq_ignore_ascii_case("instrumental"))
    })
}

/// Placeholder in a synced lyrics skeleton for "the next non-blank line of the plain lyrics".
const LINE: char = '\u{2}';

/// Synced lyrics mostly repeat the plain lyrics line by line, with timestamps. The skeleton keeps the timestamps and
/// every line that differs, and replaces each line that is the next non-blank line of the plain lyrics with a
/// placeholder. `None` when that doesn't give the synced lyrics back exactly.
#[must_use]
pub fn synced_skeleton(synced: &str, plain: &str) -> Option<String> {
    if synced.contains(LINE) {
        return None;
    }
    let mut plain_lines = plain
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .peekable();
    let mut out = String::with_capacity(synced.len() / 3);
    let mut replaced = false;
    for (i, line) in synced.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let prefix = skeleton_prefix_len(line);
        let rest = line.get(prefix..).unwrap_or_default();
        if prefix > 0 && !rest.is_empty() && plain_lines.peek() == Some(&rest) {
            plain_lines.next();
            out.push_str(line.get(..prefix).unwrap_or_default());
            out.push(LINE);
            replaced = true;
        } else {
            out.push_str(line);
        }
    }
    (replaced && synced_from_skeleton(&out, plain) == synced).then_some(out)
}

/// The synced lyrics back from a skeleton and the plain lyrics.
#[must_use]
pub fn synced_from_skeleton(skeleton: &str, plain: &str) -> String {
    let mut plain_lines = plain.split('\n').filter(|l| !l.trim().is_empty());
    let mut out = String::with_capacity(skeleton.len() + plain.len());
    for (i, line) in skeleton.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match line.strip_suffix(LINE) {
            Some(prefix) => {
                out.push_str(prefix);
                out.push_str(plain_lines.next().unwrap_or_default());
            }
            None => out.push_str(line),
        }
    }
    out
}

/// The length of a line's timestamps and the whitespace after them.
fn skeleton_prefix_len(line: &str) -> usize {
    let mut at = 0;
    let bytes = line.as_bytes();
    while let Some(m) = TIMESTAMP.find(line.get(at..).unwrap_or_default()) {
        at += m.end();
    }
    if at == 0 {
        return 0;
    }
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_lrclib() {
        assert_eq!(normalize("Café del Mar (Remix)"), "cafe del mar remix");
        assert_eq!(normalize("  Don't   STOP  me—now! "), "dont stop me—now");
        assert_eq!(normalize("AC/DC"), "ac dc");
        assert_eq!(normalize("Ærøskøbing"), "æroskobing");
        assert_eq!(normalize_param(Some("!!!")), None);
        assert_eq!(normalize_param(Some("Hello")), Some("hello".to_owned()));
    }

    #[test]
    fn plain_from_synced() {
        let synced =
            "[ar: Someone]\n[00:01.00] One\n[00:02.50]Two\n\nUntimed\n[00:03.00][00:04.00] Twice";
        assert_eq!(
            plain_from_lrc(synced).as_deref(),
            Some("One\nTwo\n\nUntimed\nTwice")
        );
        assert_eq!(plain_from_lrc("[ar: x]\n[ti: y]"), None);
        assert!(is_instrumental_lrc("[au: Instrumental]"));
    }

    #[test]
    fn skeletons_round_trip() {
        let plain = "One\nTwo\n\nThree\nFour";
        let synced = "[00:01.00] One\n[00:02.00] Two\n[00:03.00] Three\n[00:04.00] Not four\n[00:05.00] Four";
        let skeleton = synced_skeleton(synced, plain).unwrap();
        assert!(skeleton.len() < synced.len());
        assert_eq!(synced_from_skeleton(&skeleton, plain), synced);
        // Nothing to replace, or a placeholder in the input: no skeleton.
        assert_eq!(synced_skeleton("[00:01.00] x", "y"), None);
        assert_eq!(synced_skeleton("[00:01.00] \u{2}", "\u{2}"), None);
    }
}
