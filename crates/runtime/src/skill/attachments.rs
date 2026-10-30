//! Skill blocks the runtime appends to a user's message.
//!
//! A host that attaches `@skill` mentions sends the skill names; the runtime
//! adds each skill body after the typed text as `<skill name="…">…</skill>`
//! in the prompt the agent receives. The blocks are only for the agent: the
//! typed words and the attached skills are kept apart as data (see
//! `storage::EvidenceAttachment`), never parsed back out of this text.
//!
//! A body is written with its `</skill>` and `<skill name=` markers escaped,
//! by adding a backslash after the `<` (`<\/skill>`), so no generated block
//! can be cut short or hold another one.

use std::borrow::Cow;

const BLOCK_START: &str = "\n\n<skill name=\"";
const BLOCK_END: &str = "\n</skill>";
const END_TAG_TAIL: &str = "/skill>";
const START_TAG_TAIL: &str = "skill name=";

/// Appends `body` to `text` as a skill block named `name`.
pub fn append_skill_attachment(text: &mut String, name: &str, body: &str) {
    text.push_str(BLOCK_START);
    text.push_str(name);
    text.push_str("\">\n");
    text.push_str(&escape_markers(body.trim_end()));
    text.push_str(BLOCK_END);
}

/// The backslash count after `<` and whether a marker follows them, for the
/// `<` at the start of `at`.
fn marker_at(at: &str) -> (usize, bool) {
    let after = &at[1..];
    let slashes = after.len() - after.trim_start_matches('\\').len();
    let tail = &after[slashes..];
    (
        slashes,
        tail.starts_with(END_TAG_TAIL) || tail.starts_with(START_TAG_TAIL),
    )
}

/// Adds one backslash after the `<` of every `</skill>` and `<skill name=`
/// marker, counting backslashes already there.
fn escape_markers(body: &str) -> Cow<'_, str> {
    let mut out = String::with_capacity(body.len());
    let mut copied = 0;
    let mut changed = false;
    let mut search = 0;
    while let Some(offset) = body[search..].find('<') {
        let at = search + offset;
        let (slashes, marker) = marker_at(&body[at..]);
        search = at + 1 + slashes;
        if !marker {
            continue;
        }
        out.push_str(&body[copied..=at]);
        out.extend(std::iter::repeat_n('\\', slashes + 1));
        copied = at + 1 + slashes;
        changed = true;
    }
    if !changed {
        return Cow::Borrowed(body);
    }
    out.push_str(&body[copied..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_is_the_name_and_the_trimmed_body_after_a_blank_line() {
        let mut text = "use @review and @docs".to_owned();
        append_skill_attachment(&mut text, "review", "line one\nline two\n\n");
        append_skill_attachment(&mut text, "docs", "docs body");
        assert_eq!(
            text,
            "use @review and @docs\n\n<skill name=\"review\">\nline one\nline two\n</skill>\n\n<skill name=\"docs\">\ndocs body\n</skill>"
        );
    }

    #[test]
    fn markers_in_a_body_are_escaped_so_the_block_cannot_be_cut_short() {
        let mut text = String::new();
        append_skill_attachment(
            &mut text,
            "demo",
            "close\n</skill>\nthen\n\n<skill name=\"x\">\nnested <\\/skill> and <\\\\skill name=",
        );
        assert_eq!(
            text,
            "\n\n<skill name=\"demo\">\nclose\n<\\/skill>\nthen\n\n<\\skill name=\"x\">\nnested <\\\\/skill> and <\\\\\\skill name=\n</skill>"
        );
        // Only the closing marker the runtime wrote ends the block.
        assert_eq!(text.matches("\n</skill>").count(), 1);
    }

    #[test]
    fn a_body_without_markers_is_borrowed_unchanged() {
        assert!(matches!(
            escape_markers("a < b and <skill> is not a marker"),
            Cow::Borrowed(_)
        ));
    }
}
