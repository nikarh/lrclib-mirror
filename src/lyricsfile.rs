//! The Lyricsfile (YAML) LRCLIB builds from a track's plain and synced lyrics. Most stored Lyricsfiles are exactly
//! what it builds, so the store keeps only the ones that aren't.
//!
//! Ported from LRCLIB (<https://github.com/tranxuanthang/lrclib>, MIT, Copyright (c) 2024 tranxuanthang):
//! `server/src/lyricsfile.rs` (`build_lyricsfile`). The YAML is what the same serializer (`serde_yaml` 0.9) writes:
//! by [`crate::yaml`] for the common strings, by `serde_yaml` itself for the rest.

use serde::Serialize;

use crate::text::{non_blank, plain_from_lrc, timestamp_prefix};
use crate::yaml::write_value;

const VERSION: &str = "1.0";

/// The track a Lyricsfile describes.
#[derive(Debug, Clone, Copy)]
pub struct Meta<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub duration: f64,
}

/// The first part of a Lyricsfile: the version and the track.
#[derive(Serialize)]
struct Head<'a> {
    version: &'a str,
    metadata: Metadata<'a>,
}

#[derive(Serialize)]
struct Metadata<'a> {
    title: &'a str,
    artist: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    album: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<i64>,
    instrumental: bool,
}

/// The rest: the lyrics.
#[derive(Serialize)]
struct Body<'a> {
    lines: Vec<Line<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plain: Option<std::borrow::Cow<'a, str>>,
}

#[derive(Serialize)]
struct Line<'a> {
    text: &'a str,
    start_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_ms: Option<i64>,
}

/// LRCLIB's Lyricsfile for a track's lyrics: [`head`] and [`body`]. LRCLIB's server serializes them as one document,
/// which gives the same YAML (top-level fields in order).
#[must_use]
pub fn build(
    meta: &Meta<'_>,
    plain: Option<&str>,
    synced: Option<&str>,
    instrumental: bool,
) -> Option<String> {
    let mut out = head(meta, instrumental)?;
    out.push_str(&body(plain, synced, instrumental)?);
    Some(out)
}

/// The first part of a Lyricsfile, up to `lines:`.
#[must_use]
pub fn head(meta: &Meta<'_>, instrumental: bool) -> Option<String> {
    let head = Head {
        version: VERSION,
        metadata: Metadata {
            title: meta.title.trim(),
            artist: meta.artist.trim(),
            album: non_blank(Some(meta.album.trim())),
            duration_ms: duration_ms(meta.duration),
            instrumental,
        },
    };
    fast_head(&head).or_else(|| serde_yaml::to_string(&head).ok())
}

fn fast_head(head: &Head<'_>) -> Option<String> {
    use std::fmt::Write as _;
    let m = &head.metadata;
    let mut out = String::with_capacity(160);
    out.push_str("version: '1.0'\nmetadata:\n  title: ");
    write_value(&mut out, m.title, 4)?;
    out.push_str("  artist: ");
    write_value(&mut out, m.artist, 4)?;
    if let Some(album) = m.album {
        out.push_str("  album: ");
        write_value(&mut out, album, 4)?;
    }
    if let Some(ms) = m.duration_ms {
        let _ = writeln!(out, "  duration_ms: {ms}");
    }
    let _ = writeln!(out, "  instrumental: {}", m.instrumental);
    Some(out)
}

/// The part of a Lyricsfile that comes from the lyrics, from `lines:` on.
#[must_use]
pub fn body(plain: Option<&str>, synced: Option<&str>, instrumental: bool) -> Option<String> {
    let plain = non_blank(plain);
    let synced = non_blank(synced);
    let timed = if instrumental {
        Vec::new()
    } else {
        synced.map(parse_lrc_lines).unwrap_or_default()
    };
    let lines = timed
        .iter()
        .enumerate()
        .map(|(i, (start_ms, text))| Line {
            text,
            start_ms: *start_ms,
            end_ms: timed.get(i + 1).map(|next| next.0),
        })
        .collect();
    let plain = if instrumental {
        None
    } else {
        plain
            .map(std::borrow::Cow::Borrowed)
            .or_else(|| synced.and_then(plain_from_lrc).map(std::borrow::Cow::Owned))
    };
    let body = Body { lines, plain };
    fast_body(&body).or_else(|| serde_yaml::to_string(&body).ok())
}

fn fast_body(body: &Body<'_>) -> Option<String> {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(
        body.lines.iter().map(|l| l.text.len() + 40).sum::<usize>()
            + body.plain.as_ref().map_or(0, |p| p.len() + 16),
    );
    if body.lines.is_empty() {
        out.push_str("lines: []\n");
    } else {
        out.push_str("lines:\n");
        for line in &body.lines {
            out.push_str("- text: ");
            write_value(&mut out, line.text, 4)?;
            let _ = writeln!(out, "  start_ms: {}", line.start_ms);
            if let Some(end) = line.end_ms {
                let _ = writeln!(out, "  end_ms: {end}");
            }
        }
    }
    if let Some(plain) = &body.plain {
        out.push_str("plain: ");
        write_value(&mut out, plain, 2)?;
    }
    Some(out)
}

/// Where the part of a Lyricsfile that comes from the lyrics starts: after the `metadata` mapping, at `lines:`.
#[must_use]
pub fn body_start(lyricsfile: &str) -> Option<usize> {
    lyricsfile.find("\nlines:").map(|i| i + 1)
}

fn parse_lrc_lines(synced: &str) -> Vec<(i64, &str)> {
    let mut lines = synced
        .lines()
        .flat_map(|line| {
            let (timestamps, remaining) = timestamp_prefix(line.trim());
            let text = remaining.trim();
            timestamps.into_iter().map(move |ms| (ms, text))
        })
        .collect::<Vec<_>>();
    lines.sort_by_key(|line| line.0);
    lines
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn duration_ms(duration: f64) -> Option<i64> {
    if !duration.is_finite() || duration <= 0.0 {
        return None;
    }
    let ms = (duration * 1_000.0).round();
    (ms <= i64::MAX as f64).then_some(ms as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_lrclib_documents() {
        let meta = Meta {
            title: " Run ",
            artist: "Someone",
            album: "",
            duration: 12.3456,
        };
        let doc = build(&meta, None, Some("[00:01.00] It's\n[00:00.50]First"), false).unwrap();
        assert_eq!(
            doc,
            "version: '1.0'\nmetadata:\n  title: Run\n  artist: Someone\n  duration_ms: 12346\n  instrumental: false\n\
             lines:\n- text: First\n  start_ms: 500\n  end_ms: 1000\n- text: It's\n  start_ms: 1000\nplain: |-\n  It's\n  First\n"
        );
        let body = body_start(&doc).unwrap();
        assert!(doc[body..].starts_with("lines:"));
        let instrumental = build(&meta, Some("x"), Some("[00:01.00] x"), true).unwrap();
        assert!(instrumental.ends_with("instrumental: true\nlines: []\n"));
    }
}
