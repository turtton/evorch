//! The local title derived from a thread's first text, shared by every producer of
//! a thread title that must not wait for a model: the GUI's auto-title fallback and
//! the title stored when a lane is opened.

/// Longest title, in characters.
pub const THREAD_TITLE_MAX_CHARS: usize = 100;

/// Title used when the text has no visible first line (for example an image-only
/// message).
pub const EMPTY_TITLE: &str = "Image attachment";

/// The first line of `text` with whitespace runs collapsed to single spaces, cut to
/// [`THREAD_TITLE_MAX_CHARS`] characters; [`EMPTY_TITLE`] when `text` is blank.
#[must_use]
pub fn fallback_title(text: &str) -> String {
    text.trim()
        .lines()
        .next()
        .unwrap_or(EMPTY_TITLE)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(THREAD_TITLE_MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_first_line_and_collapses_whitespace() {
        assert_eq!(
            fallback_title("  調査して   報告する\n二行目は使わない"),
            "調査して 報告する"
        );
    }

    #[test]
    fn cuts_to_the_character_limit_not_the_byte_limit() {
        let title = fallback_title(&"あ".repeat(THREAD_TITLE_MAX_CHARS + 20));
        assert_eq!(title.chars().count(), THREAD_TITLE_MAX_CHARS);
    }

    #[test]
    fn blank_text_has_the_placeholder_title() {
        assert_eq!(fallback_title(" \n\t "), EMPTY_TITLE);
    }
}
