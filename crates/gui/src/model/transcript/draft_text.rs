//! The readable text of a call's arguments while the JSON is still incomplete.
//!
//! A delegate's `prompt` or a shell's `command` is a long string that arrives in
//! pieces. The card shows that string itself (line breaks kept) instead of the
//! JSON around it. The input is the newest part of the raw argument text, so
//! parsing is deliberately loose: it finds the last `"field":"` opening, reads
//! up to the closing quote or the end of what has arrived, and decodes the JSON
//! escapes it recognises. Nothing here redacts; callers redact the result.

/// The argument whose string is the card's text, per tool.
pub(super) fn text_field(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "delegate" => Some("prompt"),
        "bash" | "shell" => Some("command"),
        _ => None,
    }
}

/// The decoded string of `field` in `raw` and whether it is complete from its
/// first character, or `None` while it has not started.
///
/// When `head_cut` (the start of `raw` was dropped) and the field's opening is
/// gone, `raw` is taken to be the middle of the string: a call whose text is
/// longer than the retained window still shows its newest part.
pub(super) fn field_text(raw: &str, field: &str, head_cut: bool) -> Option<(String, bool)> {
    match last_opening(raw, field) {
        Some(start) => Some((unescape_string(&raw[start..]), true)),
        None if head_cut && !raw.trim_start().starts_with('{') => {
            Some((unescape_string(raw), false))
        }
        None => None,
    }
}

/// Byte offset just after the last unescaped `"field"` `:` `"`.
fn last_opening(raw: &str, field: &str) -> Option<usize> {
    let key = format!("\"{field}\"");
    raw.rmatch_indices(&key).find_map(|(at, _)| {
        // A quote that follows a backslash belongs to a string, not to a key.
        if raw[..at].ends_with('\\') {
            return None;
        }
        let after = raw[at + key.len()..].trim_start();
        let after = after.strip_prefix(':')?.trim_start();
        after.starts_with('"').then(|| raw.len() - after.len() + 1)
    })
}

/// Decodes JSON string escapes up to the first unescaped quote. An escape cut
/// off by the end of the text is dropped; one that is not valid stays literal.
pub(super) fn unescape_string(text: &str) -> String {
    decode(text, true)
}

/// Decodes the escapes of a whole JSON text, structure included. This is only
/// for recognising credentials that were written with escapes.
pub(super) fn unescape_all(text: &str) -> String {
    decode(text, false)
}

fn decode(text: &str, stop_at_quote: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices();
    while let Some((at, character)) = chars.next() {
        match character {
            '"' if stop_at_quote => break,
            '\\' => {
                let Some((_, escape)) = chars.next() else {
                    break;
                };
                match escape {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    '"' | '\\' | '/' => out.push(escape),
                    'u' => {
                        let rest = &text[at + 2..];
                        if let Some((decoded, used)) = unicode_escape(rest) {
                            out.push(decoded);
                            // The digits and the second escape are all ASCII.
                            for _ in 0..used {
                                chars.next();
                            }
                        } else if unicode_escape_incomplete(rest) {
                            break;
                        } else {
                            out.push_str("\\u");
                        }
                    }
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn hex_unit(text: &str) -> Option<u32> {
    let digits = text.get(..4)?;
    digits
        .chars()
        .all(|c| c.is_ascii_hexdigit())
        .then(|| u32::from_str_radix(digits, 16).ok())
        .flatten()
}

/// A `\uXXXX` escape (or surrogate pair) whose digits start `rest`, and how
/// many characters after the `u` it consumed.
fn unicode_escape(rest: &str) -> Option<(char, usize)> {
    let high = hex_unit(rest)?;
    if (0xD800..0xDC00).contains(&high) {
        let low = rest
            .get(4..)?
            .strip_prefix("\\u")
            .and_then(hex_unit)
            .filter(|low| (0xDC00..0xE000).contains(low))?;
        let combined = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
        return char::from_u32(combined).map(|c| (c, 10));
    }
    char::from_u32(high).map(|c| (c, 4))
}

/// Whether `rest` (after `\u`) is a valid escape that the end of the text cut
/// short, as opposed to one that is wrong.
fn unicode_escape_incomplete(rest: &str) -> bool {
    if rest.len() < 4 {
        return rest.chars().all(|c| c.is_ascii_hexdigit());
    }
    let Some(high) = hex_unit(rest) else {
        return false;
    };
    if !(0xD800..0xDC00).contains(&high) {
        return false;
    }
    let tail = &rest[4..];
    tail.len() < 6
        && tail.chars().enumerate().all(|(index, c)| match index {
            0 => c == '\\',
            1 => c == 'u',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_escapes_a_model_writes_into_a_string() {
        assert_eq!(
            unescape_string(r#"a\nb\t\"q\" \\ \/ \u3042\ud83d\ude00 end"#),
            "a\nb\t\"q\" \\ / あ😀 end"
        );
        // The closing quote ends the string; what follows is other JSON.
        assert_eq!(unescape_string(r#"done","name":"x""#), "done");
    }

    #[test]
    fn decoding_a_whole_json_text_keeps_its_structure_quotes() {
        assert_eq!(
            unescape_all(
                r#"{"a":"x
y","b":"ghp_"}"#
            ),
            "{\"a\":\"x\ny\",\"b\":\"ghp_\"}"
        );
    }

    #[test]
    fn an_escape_cut_by_the_end_of_the_text_is_dropped_not_misread() {
        for cut in [
            r"abc\",
            r"abc\u",
            r"abc\u30",
            r"abc\u304",
            r"abc\ud83d",
            r"abc\ud83d\",
            r"abc\ud83d\ude",
        ] {
            assert_eq!(unescape_string(cut), "abc", "{cut}");
        }
        // Invalid escapes are kept as written.
        assert_eq!(unescape_string(r"a\qb\uZZZZc"), r"a\qb\uZZZZc");
    }

    #[test]
    fn finds_the_field_after_other_arguments_and_ignores_quoted_lookalikes() {
        let raw = r#"{"name":"w","prompt":"line 1\nline 2 says \"prompt\": \"x\"\nline 3"#;
        assert_eq!(
            field_text(raw, "prompt", false),
            Some(("line 1\nline 2 says \"prompt\": \"x\"\nline 3".into(), true))
        );
        assert_eq!(field_text(r#"{"name":"w","pro"#, "prompt", false), None);
        assert_eq!(field_text(r#"{"prompt" : "#, "prompt", false), None);
        assert_eq!(
            field_text(r#"{"prompt" : "a"#, "prompt", false),
            Some(("a".into(), true))
        );
    }

    #[test]
    fn a_window_that_lost_the_opening_shows_the_middle_of_the_string() {
        assert_eq!(
            field_text(r"ddle of it\nand more", "prompt", true),
            Some(("ddle of it\nand more".into(), false))
        );
        // Not the middle of a string when the window is the whole JSON.
        assert_eq!(field_text(r#"{"name":"w"}"#, "prompt", true), None);
        assert_eq!(field_text(r"middle", "prompt", false), None);
    }
}
