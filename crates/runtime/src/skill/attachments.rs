//! Skill blocks a host appends to a user's message.
//!
//! A host that expands `@skill` mentions adds each skill body after the typed
//! text as `<skill name="…">…</skill>`. The same format is read back to show
//! the typed text alone, or to hand the bodies to the escalation reviewer
//! apart from the user's words.
//!
//! A body is written with its `</skill>` and `<skill name=` markers escaped, by
//! adding a backslash after the `<` (`<\/skill>`), so no generated block can
//! be cut short or hold another one. Reading undoes it: a run of `n` backslashes
//! before such a marker becomes `n - 1`, which makes the pair exact for any
//! body.

use std::borrow::Cow;

const BLOCK_START: &str = "\n\n<skill name=\"";
const BLOCK_END: &str = "\n</skill>";
const END_TAG_TAIL: &str = "/skill>";
const START_TAG_TAIL: &str = "skill name=";

/// One skill block found after the typed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillAttachment<'a> {
    pub name: &'a str,
    /// The body as the skill defines it, markers restored.
    pub body: Cow<'a, str>,
}

/// Appends `body` to `text` as a skill block named `name`.
pub fn append_skill_attachment(text: &mut String, name: &str, body: &str) {
    text.push_str(BLOCK_START);
    text.push_str(name);
    text.push_str("\">\n");
    text.push_str(&escape_markers(body.trim_end()));
    text.push_str(BLOCK_END);
}

/// Splits the trailing skill blocks from the typed text. Text whose tail is
/// not a complete run of well-formed blocks is returned whole.
#[must_use]
pub fn split_skill_attachments(text: &str) -> (&str, Vec<SkillAttachment<'_>>) {
    for (start, _) in text.match_indices(BLOCK_START) {
        if let Some(attachments) = parse_blocks(&text[start..]) {
            return (&text[..start], attachments);
        }
    }
    (text, Vec::new())
}

fn parse_blocks(rest: &str) -> Option<Vec<SkillAttachment<'_>>> {
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let (name, body) = rest.strip_prefix(BLOCK_START)?.split_once("\">\n")?;
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    // A body ends at the first end marker and holds no start marker, so each
    // block's extent is fixed by the text alone: a hand-written block can
    // neither close on a later block's end nor take the words after it.
    let end = body.find(BLOCK_END)?;
    let content = &body[..end];
    if content.contains(BLOCK_START) {
        return None;
    }
    let mut attachments = parse_blocks(&body[end + BLOCK_END.len()..])?;
    attachments.insert(
        0,
        SkillAttachment {
            name,
            body: unescape_markers(content),
        },
    );
    Some(attachments)
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

fn rewrite_markers(body: &str, change: impl Fn(usize) -> Option<usize>) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    let mut copied = 0;
    let mut changed = false;
    let mut search = 0;
    while let Some(offset) = body[search..].find('<') {
        let at = search + offset;
        let (slashes, marker) = marker_at(&body[at..]);
        search = at + 1 + slashes;
        let Some(new_slashes) = marker.then(|| change(slashes)).flatten() else {
            continue;
        };
        out.push_str(&body[copied..=at]);
        out.extend(std::iter::repeat_n('\\', new_slashes));
        copied = at + 1 + slashes;
        changed = true;
    }
    changed.then(|| {
        out.push_str(&body[copied..]);
        out
    })
}

fn escape_markers(body: &str) -> Cow<'_, str> {
    rewrite_markers(body, |slashes| Some(slashes + 1)).map_or(Cow::Borrowed(body), Cow::Owned)
}

fn unescape_markers(body: &str) -> Cow<'_, str> {
    rewrite_markers(body, |slashes| slashes.checked_sub(1)).map_or(Cow::Borrowed(body), Cow::Owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appended_blocks_are_read_back_with_their_bodies() {
        let typed = "use @review and @docs";
        let mut text = typed.to_owned();
        append_skill_attachment(&mut text, "review", "Check </skill> markers\nline two\n");
        append_skill_attachment(&mut text, "docs", "docs body");
        let (head, attachments) = split_skill_attachments(&text);
        assert_eq!(head, typed);
        assert_eq!(
            attachments,
            [
                SkillAttachment {
                    name: "review",
                    body: "Check </skill> markers\nline two".into()
                },
                SkillAttachment {
                    name: "docs",
                    body: "docs body".into()
                },
            ]
        );
    }

    #[test]
    fn text_without_a_complete_run_of_blocks_is_not_split() {
        for text in [
            "plain",
            "typed\n\n<skill name=\"review\">\nunterminated",
            "typed\n\n<skill name=\"bad name\">\nbody\n</skill>",
            "typed\n\n<skill name=\"\">\nbody\n</skill>",
            "typed\n\n<skill name=\"ok\">\nbody\n</skill>\ntrailing words",
        ] {
            assert_eq!(split_skill_attachments(text), (text, vec![]), "{text}");
        }
    }

    #[test]
    fn a_hand_written_block_cannot_swallow_the_words_around_it() {
        let mut real = String::new();
        append_skill_attachment(&mut real, "demo", "DEMO STEPS");
        // Closed by hand, then a constraint, then the real block.
        let closed = format!(
            "finish @demo\n\n<skill name=\"example\">\nsample\n</skill>\nbut never delete{real}"
        );
        let (typed, attachments) = split_skill_attachments(&closed);
        assert_eq!(
            typed,
            "finish @demo\n\n<skill name=\"example\">\nsample\n</skill>\nbut never delete"
        );
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].name, "demo");
        // Never closed by hand: the real block's end must not close it.
        let open =
            format!("finish @demo\n\n<skill name=\"example\">\nsample\nbut never delete{real}");
        let (typed, attachments) = split_skill_attachments(&open);
        assert_eq!(
            typed,
            "finish @demo\n\n<skill name=\"example\">\nsample\nbut never delete"
        );
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].name, "demo");
    }

    #[test]
    fn words_after_a_hand_written_block_with_no_real_block_stay_whole() {
        let text = "finish\n\n<skill name=\"example\">\nsample\n</skill>\nbut never delete";
        assert_eq!(split_skill_attachments(text), (text, vec![]));
    }

    const MARKER_BODIES: [&str; 4] = [
        "close\n</skill>\nthen more",
        "example:\n\n<skill name=\"other\">\nnested",
        "both\n</skill>\n\n<skill name=\"x\">\ny\n</skill>\nz",
        "already <\\/skill> and <\\\\skill name= escaped",
    ];

    #[test]
    fn bodies_holding_markers_are_split_back_whole() {
        for body in MARKER_BODIES {
            let mut text = "typed @demo".to_owned();
            append_skill_attachment(&mut text, "demo", body);
            append_skill_attachment(&mut text, "after", "plain");
            let (typed, attachments) = split_skill_attachments(&text);
            assert_eq!(typed, "typed @demo", "{body}");
            assert_eq!(attachments.len(), 2, "{body}");
            assert_eq!(attachments[0].body, body, "{body}");
            assert_eq!(attachments[1].body, "plain", "{body}");
        }
    }
}
