//! A compact form of the Lyricsfiles that can't be built from the lyrics (written by their authors: millisecond
//! timings, word timings, their own layout), exact to the byte.
//!
//! A Lyricsfile is taken line by line. The lines most of its bytes are in get a short form:
//!
//! - `start_ms`/`end_ms` of a line (indented by 2) or of a word (by 4): the number, as the difference to the timing
//!   it is told from (see [`Times`]);
//! - `- text:` of a line: the YAML scalar as it is, read (plain or single-quoted) for the words that follow;
//! - `  - text:` of a word: its length, when the word is what follows in its line's text and is written as
//!   [`write_value`] writes it (words make up their line's text);
//! - `plain:` up to the end, when it is the plain lyrics as [`write_value`] writes them.
//!
//! Other lines are kept as they are. The form is three parts: an opcode per line, numbers (differences, lengths) as
//! varints, and the text kept. [`pack`] checks that [`unpack`] gives the Lyricsfile back.

use crate::yaml::write_value;

use super::{Reader, put_varint};

const OP_TEXT: u8 = 0;
const OP_LINE_START: u8 = 1;
const OP_LINE_END: u8 = 2;
const OP_WORD_START: u8 = 3;
const OP_WORD_END: u8 = 4;
const OP_LINE_TEXT: u8 = 5;
const OP_WORD_TEXT: u8 = 6;
const OP_PLAIN: u8 = 7;

/// The numbered lines, by opcode.
const NUMBERED: [(u8, &str); 4] = [
    (OP_LINE_START, "  start_ms: "),
    (OP_LINE_END, "  end_ms: "),
    (OP_WORD_START, "    start_ms: "),
    (OP_WORD_END, "    end_ms: "),
];
/// A Lyricsfile [`super::derivations`] packs, and its plain lyrics.
pub const SAMPLE: &str = "lines:\n- text: 'It''s: me, #1'\n  start_ms: 1005\n  end_ms: 2010\n  words:\n  - text: 'It''s: \
                          '\n    start_ms: 1005\n  - text: me, #1\n    start_ms: 1500\nplain: |-\n  It's: me, #1\n\n   - \
                          two: 'x'\n";
pub const SAMPLE_PLAIN: &str = "It's: me, #1\n\n - two: 'x'";
const LINE_TEXT: &str = "- text: ";
const WORD_TEXT: &str = "  - text: ";
const PLAIN: &str = "plain: ";

/// The packed form of `lyricsfile`, `None` when it is no smaller or doesn't unpack to the same bytes. `plain` is the
/// row's plain lyrics.
#[must_use]
pub fn pack(lyricsfile: &str, plain: Option<&str>) -> Option<Vec<u8>> {
    let mut ops = Vec::new();
    let mut nums = Vec::new();
    let mut text = Vec::new();
    let mut times = Times::default();
    // The text of the current line, and how much of it its words took so far.
    let mut line: Option<String> = None;
    let mut taken = 0usize;
    let mut scratch = String::new();
    let mut rest = lyricsfile;
    while !rest.is_empty() {
        if let Some(p) = plain
            && rest.starts_with(PLAIN)
        {
            scratch.clear();
            scratch.push_str(PLAIN);
            if write_value(&mut scratch, p, 2).is_some() && scratch == rest {
                ops.push(OP_PLAIN);
                break;
            }
        }
        let end = rest.find('\n').map_or(rest.len(), |i| i + 1);
        let (piece, after) = rest.split_at(end);
        rest = after;
        if let Some(body) = piece.strip_suffix('\n') {
            if let Some((op, n)) = numbered(body) {
                ops.push(op);
                put_varint(&mut nums, zigzag(n.wrapping_sub(times.base(op))));
                times.saw(op, n);
                continue;
            }
            if let Some(raw) = body.strip_prefix(LINE_TEXT) {
                ops.push(OP_LINE_TEXT);
                put_varint(&mut nums, raw.len() as u64);
                text.extend_from_slice(raw.as_bytes());
                line = scalar(raw);
                taken = 0;
                times.word = None;
                continue;
            }
            if let (Some(raw), Some(l)) = (body.strip_prefix(WORD_TEXT), &line)
                && let Some(word) = scalar(raw)
                && l.get(taken..).is_some_and(|t| t.starts_with(&word))
            {
                scratch.clear();
                if write_value(&mut scratch, &word, 4).is_some()
                    && scratch.strip_suffix('\n') == Some(raw)
                {
                    ops.push(OP_WORD_TEXT);
                    put_varint(&mut nums, word.len() as u64);
                    taken += word.len();
                    continue;
                }
            }
        }
        ops.push(OP_TEXT);
        put_varint(&mut nums, piece.len() as u64);
        text.extend_from_slice(piece.as_bytes());
    }
    let mut out = Vec::with_capacity(ops.len() + nums.len() + text.len() + 8);
    put_varint(&mut out, ops.len() as u64);
    out.extend_from_slice(&ops);
    put_varint(&mut out, nums.len() as u64);
    out.extend_from_slice(&nums);
    out.extend_from_slice(&text);
    (out.len() < lyricsfile.len() && unpack(&out, plain).as_deref() == Some(lyricsfile))
        .then_some(out)
}

/// The Lyricsfile [`pack`] packed.
#[must_use]
pub fn unpack(packed: &[u8], plain: Option<&str>) -> Option<String> {
    let mut r = Reader::new(packed);
    let n = usize::try_from(r.varint()?).ok()?;
    let ops = r.bytes(n)?;
    let n = usize::try_from(r.varint()?).ok()?;
    let mut nums = Reader::new(r.bytes(n)?);
    let mut text = Reader::new(packed.get(r.at..)?);
    let mut out = String::with_capacity(packed.len() * 3);
    let mut times = Times::default();
    let mut line: Option<String> = None;
    let mut taken = 0usize;
    for &op in ops {
        match op {
            OP_TEXT => {
                let n = usize::try_from(nums.varint()?).ok()?;
                out.push_str(std::str::from_utf8(text.bytes(n)?).ok()?);
            }
            OP_LINE_TEXT => {
                let n = usize::try_from(nums.varint()?).ok()?;
                let raw = std::str::from_utf8(text.bytes(n)?).ok()?;
                out.push_str(LINE_TEXT);
                out.push_str(raw);
                out.push('\n');
                line = scalar(raw);
                taken = 0;
                times.word = None;
            }
            OP_WORD_TEXT => {
                let n = usize::try_from(nums.varint()?).ok()?;
                let word = line.as_deref()?.get(taken..taken.checked_add(n)?)?;
                taken += n;
                out.push_str(WORD_TEXT);
                write_value(&mut out, word, 4)?;
            }
            OP_PLAIN => {
                out.push_str(PLAIN);
                write_value(&mut out, plain?, 2)?;
            }
            _ => {
                let (_, key) = NUMBERED.iter().find(|(o, _)| *o == op)?;
                let n = times.base(op).wrapping_add(unzigzag(nums.varint()?));
                times.saw(op, n);
                out.push_str(key);
                out.push_str(&n.to_string());
                out.push('\n');
            }
        }
    }
    Some(out)
}

/// The last timings seen, which the next one is told from: a line starts where the previous one ended, ends some
/// time after it starts, and its words start one after another from its start.
#[derive(Default)]
struct Times {
    line_start: i64,
    line_end: Option<i64>,
    /// The last word's start, in the current line.
    word: Option<i64>,
}

impl Times {
    fn base(&self, op: u8) -> i64 {
        match op {
            OP_LINE_START => self.line_end.unwrap_or(self.line_start),
            OP_WORD_START | OP_WORD_END => self.word.unwrap_or(self.line_start),
            _ => self.line_start,
        }
    }

    fn saw(&mut self, op: u8, n: i64) {
        match op {
            OP_LINE_START => self.line_start = n,
            OP_LINE_END => self.line_end = Some(n),
            OP_WORD_START => self.word = Some(n),
            _ => {}
        }
    }
}

/// A numbered line (without its line break): its opcode and number, written as Rust writes it.
fn numbered(body: &str) -> Option<(u8, i64)> {
    NUMBERED.iter().find_map(|(op, key)| {
        let digits = body.strip_prefix(key)?;
        let n: i64 = digits.parse().ok()?;
        (n.to_string() == digits).then_some((*op, n))
    })
}

/// The string a single-line YAML scalar is, when plain or single-quoted.
fn scalar(raw: &str) -> Option<String> {
    if let Some(inner) = raw.strip_prefix('\'') {
        return Some(inner.strip_suffix('\'')?.replace("''", "'"));
    }
    if raw.starts_with(['"', '|', '>']) {
        return None;
    }
    Some(raw.to_owned())
}

#[allow(clippy::cast_sign_loss)]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[allow(clippy::cast_possible_wrap)]
fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORDS: &str = "version: '1.0'\nmetadata:\n  title: Custard Pie\n  artist: Led Zeppelin\n  duration_ms: 254000\n  \
                         instrumental: false\n  language: en\nlines:\n- text: Drop down, baby, it's me\n  start_ms: 11430\n  \
                         end_ms: 14494\n  words:\n  - text: 'Drop '\n    start_ms: 11430\n  - text: 'down, '\n    \
                         start_ms: 11644\n  - text: 'baby, '\n    start_ms: 11996\n  - text: it's me\n    start_ms: \
                         12344\n- text: '''Quoted'' line: here'\n  words: []\n  start_ms: 809\n- text: \"double\"\n  \
                         start_ms: 900\n  words:\n  - text: other\n    start_ms: 901\n    end_ms: 950\nplain: |-\n  \
                         Drop down, baby, it's me\n\n  'Quoted' line: here\n";

    #[test]
    fn packs_and_unpacks() {
        let plain = "Drop down, baby, it's me\n\n'Quoted' line: here";
        let packed = pack(WORDS, Some(plain)).unwrap();
        assert!(packed.len() < WORDS.len() * 2 / 3, "{}", packed.len());
        assert_eq!(unpack(&packed, Some(plain)).unwrap(), WORDS);
        // Another plain text: the `plain:` part is kept as it is.
        let packed = pack(WORDS, Some("other")).unwrap();
        assert_eq!(unpack(&packed, Some("other")).unwrap(), WORDS);
        let packed = pack(WORDS, None).unwrap();
        assert_eq!(unpack(&packed, None).unwrap(), WORDS);
    }

    #[test]
    fn packs_the_sample() {
        let packed = pack(SAMPLE, Some(SAMPLE_PLAIN)).unwrap();
        let mut r = Reader::new(&packed);
        let n = usize::try_from(r.varint().unwrap()).unwrap();
        let ops = r.bytes(n).unwrap();
        for op in [
            OP_LINE_TEXT,
            OP_WORD_TEXT,
            OP_WORD_START,
            OP_LINE_END,
            OP_PLAIN,
        ] {
            assert!(ops.contains(&op), "{op} in {ops:?}");
        }
    }

    #[test]
    fn keeps_what_it_cant_pack() {
        for lf in [
            "lines:\n- text: a\n  start_ms: 007\n  end_ms: -5\n  start_ms: 99999999999999999999\n",
            "lines:\n- text: |-\n    two\n    lines\n  words:\n  - text: two\n  - text: x\n",
            "no line break at the end\n  start_ms: 5",
            "",
        ] {
            if let Some(packed) = pack(lf, None) {
                assert_eq!(unpack(&packed, None).unwrap(), lf);
            }
        }
        assert_eq!(numbered("  start_ms: -5"), Some((OP_LINE_START, -5)));
        assert_eq!(numbered("  start_ms: 007"), None);
        assert_eq!(numbered("    end_ms: 1"), Some((OP_WORD_END, 1)));
    }

    #[test]
    fn zigzags() {
        for v in [0, 1, -1, i64::MAX, i64::MIN, 12345, -12345] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }
}
