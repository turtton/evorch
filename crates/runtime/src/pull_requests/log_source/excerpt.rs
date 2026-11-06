//! Turning a raw CI log into a bounded, redacted excerpt.
//!
//! The order is the safety argument:
//!
//! 1. A log that the raw byte limit cut is cut back to its last complete line, so no half
//!    line (and no half credential) is left at the end.
//! 2. Terminal escape sequences and control characters are removed. They go *before* the
//!    redaction, so an escape code placed inside a credential cannot hide it from the rules.
//! 3. The whole text is redacted. A text that was cut is redacted as a stream, which also
//!    masks an unfinished credential at its end.
//! 4. Only then are lines capped and the tail chosen, so nothing cut by a limit was ever
//!    unredacted.

/// The longest line kept, in bytes. A longer one (minified output, a dumped blob) is cut.
pub const CI_LOG_LINE_BYTES_MAX: usize = 1024;

/// A log after cleaning: redacted lines, ready to be cut to a budget.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct CleanedLog {
    /// The redacted lines, without leading or trailing blank ones; each at most
    /// [`CI_LOG_LINE_BYTES_MAX`] bytes.
    pub lines: Vec<String>,
    /// How many credentials the redaction masked.
    pub redactions: usize,
    /// Whether the bytes were not valid UTF-8 and were repaired.
    pub lossy_utf8: bool,
    /// Whether a line was cut to the per-line limit.
    pub line_cut: bool,
}

/// Cleans `raw`, which `gh` printed; `raw_truncated` says the byte limit ended it.
pub(super) fn clean_log(raw: &[u8], raw_truncated: bool) -> CleanedLog {
    let raw = if raw_truncated {
        // A cut log is kept only up to its last complete line.
        match raw.iter().rposition(|byte| *byte == b'\n') {
            Some(end) => &raw[..end],
            None => &[],
        }
    } else {
        raw
    };
    let decoded = String::from_utf8_lossy(raw);
    let lossy_utf8 = matches!(decoded, std::borrow::Cow::Owned(_));
    let stripped = strip_terminal(&decoded);
    let redactor = secret_guard::SecretRedactor::from_env();
    let redacted = if raw_truncated {
        redactor.redact_streaming(&stripped)
    } else {
        redactor.redact(&stripped)
    };
    let mut line_cut = false;
    let mut lines: Vec<String> = redacted
        .text
        .split('\n')
        .map(|line| {
            let line = line.trim_end();
            let (line, cut) = cap_line(line, CI_LOG_LINE_BYTES_MAX);
            line_cut |= cut;
            line
        })
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let leading = lines.iter().take_while(|line| line.is_empty()).count();
    lines.drain(..leading);
    CleanedLog {
        lines,
        redactions: redacted.count,
        lossy_utf8,
        line_cut,
    }
}

fn cap_line(line: &str, max: usize) -> (String, bool) {
    if line.len() <= max {
        return (line.to_owned(), false);
    }
    let end = line.floor_char_boundary(max.saturating_sub('…'.len_utf8()));
    (format!("{}…", &line[..end]), true)
}

/// Removes ANSI escape sequences, control characters (but not `\n` and `\t`), and the
/// invisible format characters that can reorder or hide text.
fn strip_terminal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\u{1b}' => match chars.peek().copied() {
                // CSI: parameters, then a final byte in 0x40..=0x7e.
                Some('[') => {
                    chars.next();
                    while let Some(next) = chars.peek().copied() {
                        if next == '\n' {
                            break;
                        }
                        chars.next();
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ESC \. A newline also ends it, so an unterminated one
                // cannot swallow the rest of the log.
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.peek().copied() {
                        if next == '\n' {
                            break;
                        }
                        chars.next();
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some('\n') | None => {}
                Some(_) => {
                    chars.next();
                }
            },
            '\n' | '\t' => out.push(character),
            _ if character.is_control() || is_invisible_format(character) => {}
            _ => out.push(character),
        }
    }
    out
}

/// Zero-width, bidirectional-control and byte-order characters.
fn is_invisible_format(character: char) -> bool {
    matches!(
        character,
        '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

/// The last lines of `lines` that fit `max_bytes` (counting one `\n` between lines) and
/// `max_lines`, as one text, and how many lines before them were left out.
///
/// The end of a failing step's log is where its error is, so the tail is kept. When even the
/// last line does not fit, its start is kept, cut at a character boundary.
pub(super) fn select_tail(lines: &[String], max_bytes: usize, max_lines: usize) -> (String, usize) {
    let mut taken = 0;
    let mut bytes = 0;
    for line in lines.iter().rev() {
        let cost = line.len() + usize::from(taken > 0);
        if taken >= max_lines || bytes + cost > max_bytes {
            break;
        }
        bytes += cost;
        taken += 1;
    }
    if taken == 0 {
        let Some(last) = lines.last().filter(|_| max_lines > 0 && max_bytes > 0) else {
            return (String::new(), lines.len());
        };
        let end = last.floor_char_boundary(max_bytes);
        return (last[..end].to_owned(), lines.len() - 1);
    }
    let start = lines.len() - taken;
    (lines[start..].join("\n"), start)
}
