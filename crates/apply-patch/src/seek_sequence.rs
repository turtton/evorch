/// Attempt to find the sequence of `pattern` lines within `lines` beginning at or after `start`.
/// Returns the starting index of the match or `None` if not found. Matches are attempted with
/// decreasing strictness: exact match, then ignoring trailing whitespace, then ignoring leading
/// and trailing whitespace, then normalising Unicode punctuation and spaces.
/// When `eof` is true, only the ending position is searched (and never before
/// `start`), so patterns marked as file endings cannot match earlier lines.
///
/// Special cases handled defensively:
///  • Empty `pattern` → returns `Some(start)` (no-op match)
///  • `pattern.len() > lines.len()` → returns `None` (cannot match, avoids
///    out‑of‑bounds panic that occurred pre‑2025‑04‑12)
pub(crate) fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }

    // When the pattern is longer than the available input there is no possible
    // match. Early‑return to avoid the out‑of‑bounds slice that would occur in
    // the search loops below (previously caused a panic when
    // `pattern.len() > lines.len()`).
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof && lines.len() >= pattern.len() {
        let eof_start = lines.len() - pattern.len();
        eof_start.max(start)
    } else {
        start
    };
    // Exact match first.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    // Then rstrip match.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim_end() != pat.trim_end() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }
    // Finally, trim both sides to allow more lenience.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim() != pat.trim() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }

    // ------------------------------------------------------------------
    // Final, most permissive pass – attempt to match after *normalising*
    // common Unicode punctuation to their ASCII equivalents so that diffs
    // authored with plain ASCII characters can still be applied to source
    // files that contain typographic dashes / quotes, etc.  This mirrors the
    // fuzzy behaviour of `git apply` which ignores minor byte-level
    // differences when locating context lines.
    // ------------------------------------------------------------------

    fn normalise(s: &str) -> String {
        s.trim()
            .chars()
            .map(|c| match c {
                // Various dash / hyphen code-points → ASCII '-'
                '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
                | '\u{2212}' => '-',
                // Fancy single quotes → '\''
                '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
                // Fancy double quotes → '"'
                '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
                // Non-breaking space and other odd spaces → normal space
                '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
                | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
                | '\u{3000}' => ' ',
                other => other,
            })
            .collect::<String>()
    }

    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if normalise(&lines[i + p_idx]) != normalise(pat) {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::seek_sequence;
    use std::string::ToString;

    fn to_vec(strings: &[&str]) -> Vec<String> {
        strings.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn test_exact_match_finds_sequence() {
        let lines = to_vec(&["foo", "bar", "baz"]);
        let pattern = to_vec(&["bar", "baz"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false,),
            Some(1)
        );
    }

    #[test]
    fn test_rstrip_match_ignores_trailing_whitespace() {
        let lines = to_vec(&["foo   ", "bar\t\t"]);
        // Pattern omits trailing whitespace.
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false,),
            Some(0)
        );
    }

    #[test]
    fn test_trim_match_ignores_leading_and_trailing_whitespace() {
        let lines = to_vec(&["    foo   ", "   bar\t"]);
        // Pattern omits any additional whitespace.
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false,),
            Some(0)
        );
    }

    #[test]
    fn test_empty_pattern_and_start_bounds() {
        let lines = to_vec(&["one", "two"]);
        assert_eq!(seek_sequence(&lines, &[], 7, true), Some(7));
        assert_eq!(seek_sequence(&[], &[], 0, false), Some(0));
        assert_eq!(seek_sequence(&lines, &to_vec(&["two"]), 2, false), None);
        assert_eq!(
            seek_sequence(&lines, &to_vec(&["two"]), usize::MAX, true),
            None
        );
    }

    #[test]
    fn test_stricter_matches_win_over_earlier_fuzzy_matches() {
        let pattern = to_vec(&["word"]);
        assert_eq!(
            seek_sequence(&to_vec(&["word  ", "word"]), &pattern, 0, false),
            Some(1)
        );
        assert_eq!(
            seek_sequence(&to_vec(&[" word ", "word  "]), &pattern, 0, false),
            Some(1)
        );
        assert_eq!(
            seek_sequence(
                &to_vec(&["‘word’", " 'word' "]),
                &to_vec(&["'word'"]),
                0,
                false
            ),
            Some(1)
        );
    }

    #[test]
    fn test_eof_search_only_matches_the_file_end() {
        let lines = to_vec(&["one", "two", "one"]);
        assert_eq!(seek_sequence(&lines, &to_vec(&["one"]), 0, true), Some(2));
        assert_eq!(seek_sequence(&lines, &to_vec(&["two"]), 0, true), None);
        assert_eq!(seek_sequence(&lines, &to_vec(&["two"]), 0, false), Some(1));
    }

    #[test]
    fn test_normalises_unicode_punctuation_and_spaces() {
        for line in [
            "‐‑‒–—―−",
            "‘’‚‛",
            "“”„‟",
            "a\u{a0}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{202f}\u{205f}\u{3000}b",
        ] {
            let expected = match line.chars().next().unwrap() {
                '‐' => "-------",
                '‘' => "''''",
                '“' => "\"\"\"\"",
                _ => "a             b",
            };
            assert_eq!(
                seek_sequence(&to_vec(&[line]), &to_vec(&[expected]), 0, false),
                Some(0),
                "{line:?}"
            );
        }
        assert_eq!(
            seek_sequence(&to_vec(&["α"]), &to_vec(&["a"]), 0, false),
            None
        );
    }

    #[test]
    fn test_pattern_longer_than_input_returns_none() {
        let lines = to_vec(&["just one line"]);
        let pattern = to_vec(&["too", "many", "lines"]);
        // Should not panic – must return None when pattern cannot possibly fit.
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false,),
            None
        );
    }
}
