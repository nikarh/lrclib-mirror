//! A YAML writer for the strings of a Lyricsfile that writes them as `serde_yaml` (libyaml) does, several times
//! faster: plain when libyaml would, single-quoted when a plain string would read as another type or isn't allowed
//! plain, and as a literal block (`|`, `|-`, `|+`) when it has line breaks. Anything else (control characters, tabs, a
//! space before a line break, ...) is left to `serde_yaml` (`None`).
//!
//! A difference to `serde_yaml` can't make an answer wrong: the importer keeps a Lyricsfile as built only when it is
//! byte for byte the stored one, and stores it whole otherwise.

/// Writes `s` as the value of a mapping key whose children are indented by `indent`, with the line break after it.
/// `None` when it should be left to `serde_yaml`.
pub fn write_value(out: &mut String, s: &str, indent: usize) -> Option<()> {
    if s.contains('\n') {
        return write_literal(out, s, indent);
    }
    if !s.chars().all(printable) {
        return None;
    }
    if s.is_empty() || s.bytes().all(|b| b.is_ascii_digit()) {
        // Would read as null or a number: `serde_yaml` asks for quotes.
        out.push('\'');
        out.push_str(s);
        out.push_str("'\n");
        return Some(());
    }
    if needs_inference(s) {
        // Might read as a boolean or a number: left to `serde_yaml`.
        return None;
    }
    if plain_allowed(s) {
        out.push_str(s);
    } else {
        out.push('\'');
        out.push_str(&s.replace('\'', "''"));
        out.push('\'');
    }
    out.push('\n');
    Some(())
}

/// Characters libyaml writes as they are (with its Unicode output on), leaving out line breaks.
fn printable(c: char) -> bool {
    matches!(c, ' '..='~')
        || (c >= '\u{a0}'
            && !matches!(
                c,
                '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}'
            ))
}

/// Strings `serde_yaml` checks for another type before writing them: empty, null and boolean words, and what looks like
/// a number (it could be one).
fn needs_inference(s: &str) -> bool {
    if s.is_empty()
        || matches!(
            s,
            "~" | "null" | "Null" | "NULL" | "true" | "True" | "TRUE" | "false" | "False" | "FALSE"
        )
    {
        return true;
    }
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    let after_dot = unsigned.strip_prefix('.').unwrap_or(unsigned);
    let numeric_start = after_dot.starts_with(|c: char| c.is_ascii_digit());
    let word = !s.contains(' ');
    let special = matches!(
        unsigned.to_ascii_lowercase().as_str(),
        "inf" | "infinity" | "nan" | ".inf" | ".nan"
    );
    (numeric_start && word) || special
}

/// Whether libyaml writes a single-line string plain in a block mapping value.
fn plain_allowed(s: &str) -> bool {
    if s.starts_with(' ') || s.ends_with(' ') || s.starts_with("---") || s.starts_with("...") {
        return false;
    }
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let followed_by_space = chars.get(i + 1).is_none_or(|n| *n == ' ');
        let preceded_by_space = i > 0 && chars.get(i - 1) == Some(&' ');
        if i == 0 {
            match c {
                '#' | ',' | '[' | ']' | '{' | '}' | '&' | '*' | '!' | '|' | '>' | '\'' | '"'
                | '%' | '@' | '`' => {
                    return false;
                }
                '?' | ':' | '-' if followed_by_space => return false,
                _ => {}
            }
        } else {
            match c {
                ':' if followed_by_space => return false,
                '#' if preceded_by_space => return false,
                _ => {}
            }
        }
    }
    true
}

/// A string with line breaks, as a literal block: `|` and its chomping and indentation indicators, then the lines.
fn write_literal(out: &mut String, s: &str, indent: usize) -> Option<()> {
    // libyaml writes these double-quoted.
    if s.ends_with(' ') || s.contains(" \n") || !s.chars().all(|c| c == '\n' || printable(c)) {
        return None;
    }
    out.push('|');
    if s.starts_with([' ', '\n']) {
        out.push('2');
    }
    if s == "\n" || s.ends_with("\n\n") {
        out.push('+');
    } else if !s.ends_with('\n') {
        out.push('-');
    }
    out.push('\n');
    let body = s.strip_suffix('\n').unwrap_or(s);
    for line in body.split('\n') {
        if !line.is_empty() {
            out.extend(std::iter::repeat_n(' ', indent));
            out.push_str(line);
        }
        out.push('\n');
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize)]
    struct Doc<'a> {
        lines: Vec<Line<'a>>,
        plain: &'a str,
    }

    #[derive(serde::Serialize)]
    struct Line<'a> {
        text: &'a str,
        start_ms: i64,
    }

    fn ours(s: &str) -> Option<String> {
        let mut out = String::from("lines:\n- text: ");
        write_value(&mut out, s, 4)?;
        out.push_str("  start_ms: 1\nplain: ");
        write_value(&mut out, s, 2)?;
        Some(out)
    }

    fn check(s: &str) -> bool {
        let Some(ours) = ours(s) else { return false };
        let theirs = serde_yaml::to_string(&Doc {
            lines: vec![Line {
                text: s,
                start_ms: 1,
            }],
            plain: s,
        })
        .unwrap();
        assert_eq!(ours, theirs, "{s:?}");
        true
    }

    #[test]
    fn writes_like_serde_yaml() {
        for s in [
            "plain words",
            "it's",
            "'quoted'",
            "\"dq\"",
            "- dash",
            "-x",
            "key: value",
            "key:value",
            "Chorus:",
            "a #b",
            "a# b",
            "yes",
            "no",
            "On",
            "?q",
            "? q",
            ":c",
            ",c",
            "a,b",
            "[x]",
            "x[y]",
            "{x}",
            "@at",
            "%p",
            "Ünïcödé 中文 🎵",
            "--- x",
            "... x",
            "...",
            "a\\b",
            "line1\nline2",
            "trailing\n",
            "two\n\n",
            "x\n\n\n",
            " lead\nx",
            "\nlead",
            "x\n y",
            "a\n\nb",
            "(Chorus)",
            "99 problems",
            "<<",
            "=",
            "1_000 x",
        ] {
            check(s);
        }
        // Random strings over characters that matter.
        let alphabet: Vec<char> = "ab Z09 '\"-?:#,[]{}&*!|>%@`.+\n~é中\\/()"
            .chars()
            .chain(['.', '.', '1'])
            .collect();
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut written = 0;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = usize::try_from(x % 9).unwrap();
            let s: String = (0..len)
                .map(|i| alphabet[usize::try_from((x >> (i * 6)) % alphabet.len() as u64).unwrap()])
                .collect();
            if check(&s) {
                written += 1;
            }
        }
        assert!(written > 100_000, "{written}");
    }
}
