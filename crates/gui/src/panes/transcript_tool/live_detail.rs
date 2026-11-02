//! The multi-line text of a call that has not finished: what a delegate was
//! told, the shell command, or the other arguments.
//!
//! The header stays a short identifier. This area sits below it, shows a few
//! lines at first and, once the card is expanded, scrolls inside a bounded
//! height. Expanding only changes what is displayed; it never acts on the call,
//! so it works while the call is still being composed or running.
//!
//! Everything shown here is redacted before it reaches this module: drafts are
//! redacted in the model, and a running call's accepted input is redacted as a
//! display copy by the caller. The runtime's arguments are never altered.

use std::borrow::Cow;
use std::sync::Arc;

use egui::scroll_area::ScrollSource;
use egui::{FontId, RichText, ScrollArea, Ui};
use serde_json::Value;

use crate::theme::tokens::{FONT_SMALL, R_SM, SP_2, palette};

/// Lines visible before the card is expanded.
const COLLAPSED_LINES: f32 = 8.0;
/// Height the expanded area grows to before it scrolls.
const EXPANDED_MAX_HEIGHT: f32 = 320.0;
/// Most of an accepted input that is laid out; the rest is reported as omitted.
const RUNNING_TEXT_BYTES: usize = 16 * 1024;
/// A shell command up to this long, on one line, is complete in the header
/// unless the pane is too narrow, so it is kept behind the header's expansion.
const SHELL_ONE_LINE_CHARS: usize = 60;

/// An accepted input as it may be displayed, kept across frames: a redacted
/// copy of the value and, for a running call, the text of its arguments.
pub(super) struct DisplayInput {
    hash: u64,
    pub value: Value,
    pretty: Option<(String, usize)>,
}

impl DisplayInput {
    /// The redacted copy of `input` for the card with `id`, rebuilt only when
    /// the input changes. The runtime's value is only read, never altered.
    pub(super) fn cached(
        ui: &Ui,
        id: egui::Id,
        input: Option<&Value>,
        with_text: bool,
    ) -> Option<Arc<Self>> {
        let input = input?;
        let key = id.with("display-input");
        let hash = hash_value(input);
        if let Some(cached) = ui.data(|data| data.get_temp::<Arc<Self>>(key))
            && cached.hash == hash
            && (!with_text || cached.pretty.is_some())
        {
            return Some(cached);
        }
        let value = crate::model::transcript::redacted_for_display(input);
        let pretty = with_text.then(|| {
            let pretty = super::pretty_json(&value);
            let cut = floor_boundary(&pretty, RUNNING_TEXT_BYTES);
            (pretty[..cut].to_owned(), pretty.len() - cut)
        });
        let built = Arc::new(Self {
            hash,
            value,
            pretty,
        });
        ui.data_mut(|data| data.insert_temp(key, built.clone()));
        Some(built)
    }
}

fn hash_value(value: &Value) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    fn walk(value: &Value, state: &mut DefaultHasher) {
        match value {
            Value::Null => 0u8.hash(state),
            Value::Bool(flag) => (1u8, flag).hash(state),
            Value::Number(number) => (2u8, number.to_string()).hash(state),
            Value::String(text) => (3u8, text).hash(state),
            Value::Array(items) => {
                (4u8, items.len()).hash(state);
                items.iter().for_each(|item| walk(item, state));
            }
            Value::Object(fields) => {
                (5u8, fields.len()).hash(state);
                for (key, item) in fields {
                    key.hash(state);
                    walk(item, state);
                }
            }
        }
    }
    let mut state = DefaultHasher::new();
    walk(value, &mut state);
    state.finish()
}

fn floor_boundary(text: &str, limit: usize) -> usize {
    let mut cut = text.len().min(limit);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

pub(super) struct LiveDetail<'a> {
    pub text: Cow<'a, str>,
    omitted: Omitted,
    /// A draft grows at its end, so its newest text stays in view.
    follow_tail: bool,
    /// Whether the area is drawn before the card is expanded; a command the
    /// header already shows whole waits for the reader to ask for it.
    shown_collapsed: bool,
}

enum Omitted {
    Nothing,
    /// Older text no longer fits what is kept.
    Earlier,
    /// This many bytes after the shown text are not laid out.
    Later(usize),
}

/// The detail of a call the model is still composing.
pub(super) fn drafting<'a>(
    text: &'a str,
    arguments: &'a str,
    omitted: bool,
) -> Option<LiveDetail<'a>> {
    let shown = if text.is_empty() { arguments } else { text };
    if shown.trim().is_empty() {
        return None;
    }
    Some(LiveDetail {
        text: Cow::Borrowed(shown),
        omitted: if omitted {
            Omitted::Earlier
        } else {
            Omitted::Nothing
        },
        follow_tail: true,
        shown_collapsed: true,
    })
}

/// The detail of a call the runtime accepted. `header` is the text the card's
/// header already shows, so arguments it conveys are not repeated.
pub(super) fn running<'a>(
    tool_name: &str,
    input: &'a DisplayInput,
    header: &str,
) -> Option<LiveDetail<'a>> {
    let field = |key: &str| {
        input
            .value
            .get(key)?
            .as_str()
            .filter(|text| !text.trim().is_empty())
    };
    let (text, omitted, shown_collapsed) = match tool_name {
        "delegate" if field("prompt").is_some() => {
            let (text, omitted) = bounded(field("prompt")?);
            (Cow::Borrowed(text), omitted, true)
        }
        "bash" | "shell" => {
            let command = field("command")?;
            let (text, omitted) = bounded(command);
            let long = command.contains('\n') || command.chars().count() > SHELL_ONE_LINE_CHARS;
            (Cow::Borrowed(text), omitted, long)
        }
        _ if needs_detail(&input.value, header) => {
            let (pretty, later) = input.pretty.as_ref()?;
            let omitted = if *later > 0 {
                Omitted::Later(*later)
            } else {
                Omitted::Nothing
            };
            (Cow::Borrowed(pretty.as_str()), omitted, true)
        }
        _ => return None,
    };
    Some(LiveDetail {
        text,
        omitted,
        follow_tail: false,
        shown_collapsed,
    })
}

fn bounded(text: &str) -> (&str, Omitted) {
    let cut = floor_boundary(text, RUNNING_TEXT_BYTES);
    let omitted = if cut < text.len() {
        Omitted::Later(text.len() - cut)
    } else {
        Omitted::Nothing
    };
    (&text[..cut], omitted)
}

/// Whether some argument text cannot be read in `header`: it is missing from
/// it, cut short in it, or spans lines. Arguments that are only numbers and
/// flags, or all in the header, need no detail.
fn needs_detail(input: &Value, header: &str) -> bool {
    fn uncovered(value: &Value, header: &str) -> bool {
        match value {
            Value::String(text) => {
                let text = text.trim();
                if text.is_empty() || header.contains(text) {
                    return false;
                }
                // A path may be shown relative to the repository or shortened.
                let name = text
                    .rsplit('/')
                    .next()
                    .filter(|_| !text.contains(char::is_whitespace));
                !name.is_some_and(|name| !name.is_empty() && header.contains(name))
            }
            Value::Array(items) => items.iter().any(|item| uncovered(item, header)),
            Value::Object(fields) => fields.values().any(|item| uncovered(item, header)),
            _ => false,
        }
    }
    uncovered(input, header)
}

/// Scroll offset and furthest offset of the detail area of the card with `id`.
pub(super) fn scroll_state(ctx: &egui::Context, id: egui::Id) -> Option<(f32, f32)> {
    ctx.data(|data| data.get_temp(id.with("live-detail-scroll")))
}

/// Draws the area. `expanded` is the card's display state; `id` keeps the
/// scroll position across frames and from a draft to the running call.
pub(super) fn show(ui: &mut Ui, id: egui::Id, detail: &LiveDetail<'_>, expanded: bool) {
    if !expanded && !detail.shown_collapsed {
        return;
    }
    let font = FontId::monospace(FONT_SMALL);
    let row = ui.fonts_mut(|fonts| fonts.row_height(&font));
    let max_height = if expanded {
        EXPANDED_MAX_HEIGHT
    } else {
        row * COLLAPSED_LINES
    };
    let mut clipped = false;
    egui::Frame::new()
        .fill(palette().SURFACE_RAISED)
        .corner_radius(R_SM)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            let mut area = ScrollArea::vertical()
                .id_salt(id.with("live-detail"))
                .max_height(max_height)
                .auto_shrink([false, true])
                .scroll_source(if expanded {
                    ScrollSource::default()
                } else {
                    ScrollSource::NONE
                });
            if detail.follow_tail {
                // Stops following once the reader scrolls up and resumes at the end.
                area = area.stick_to_bottom(true);
            }
            let output = area.show(ui, |ui| {
                if matches!(detail.omitted, Omitted::Earlier) {
                    note(ui, "… earlier text is not kept");
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(&*detail.text)
                            .font(font.clone())
                            .color(palette().TEXT),
                    )
                    .wrap(),
                );
                if let Omitted::Later(bytes) = detail.omitted {
                    note(ui, &format!("… {bytes} more bytes are not shown"));
                }
            });
            let overflow = (output.content_size.y - output.inner_rect.height()).max(0.0);
            clipped = overflow > 1.0;
            ui.data_mut(|data| {
                data.insert_temp(
                    id.with("live-detail-scroll"),
                    (output.state.offset.y, overflow),
                );
            });
        });
    if clipped && !expanded {
        note(ui, "Select the header to read all of it");
    }
}

fn note(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(
        RichText::new(text)
            .size(FONT_SMALL)
            .italics()
            .color(palette().TEXT_MUTED),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(value: Value) -> DisplayInput {
        DisplayInput {
            hash: 0,
            pretty: Some((super::super::pretty_json(&value), 0)),
            value,
        }
    }

    #[test]
    fn a_draft_prefers_the_decoded_text_over_the_argument_json() {
        let detail = drafting("one\ntwo", "{\"prompt\":\"one\\ntwo\"", true).unwrap();
        assert_eq!(detail.text, "one\ntwo");
        assert!(matches!(detail.omitted, Omitted::Earlier));
        assert_eq!(
            drafting("", "{\"path\":\"a", false).unwrap().text,
            "{\"path\":\"a"
        );
        assert!(drafting("", "  ", false).is_none());
    }

    #[test]
    fn a_running_delegate_shows_its_prompt_and_a_shell_its_command() {
        let delegate = input(json!({"name": "w", "prompt": "line 1\nline 2"}));
        let detail = running("delegate", &delegate, "Delegate w").unwrap();
        assert_eq!(detail.text, "line 1\nline 2");
        assert!(detail.shown_collapsed);
        let shell = input(json!({"command": "cd a &&\n  make"}));
        let detail = running("shell", &shell, "Shell cd a && make").unwrap();
        assert_eq!(detail.text, "cd a &&\n  make");
        assert!(detail.shown_collapsed);
    }

    #[test]
    fn a_short_shell_command_waits_behind_the_header_and_control_calls_have_none() {
        let short = input(json!({"command": "cargo test"}));
        let detail = running("shell", &short, "Shell cargo test").unwrap();
        assert_eq!(detail.text, "cargo test");
        assert!(!detail.shown_collapsed, "the header already holds it");
        let long = "x".repeat(SHELL_ONE_LINE_CHARS + 1);
        assert!(
            running("shell", &input(json!({"command": long})), "Shell")
                .unwrap()
                .shown_collapsed
        );
        let poll = input(json!({"action": "poll", "job_id": "job-1"}));
        assert!(running("shell", &poll, "Poll #job-1").is_none());
    }

    #[test]
    fn other_arguments_are_shown_unless_the_header_already_reads_them() {
        // A short write still hides its body from the header.
        let write = input(json!({"path": "a.txt", "content": "first\nsecond"}));
        let detail = running("write", &write, "Write a.txt").unwrap();
        assert!(detail.text.contains("first\\nsecond"), "{}", detail.text);
        // Arguments the header shows whole, even as a shortened path, add nothing.
        let read = input(json!({"path": "/repo/src/a.rs", "limit": 50}));
        assert!(running("read", &read, "Read src/a.rs").is_none());
        let grep = input(json!({"pattern": "todo", "path": "src"}));
        assert!(running("grep", &grep, "Grep \"todo\" in src").is_none());
        // A value the header cut short is not covered.
        let long = "word ".repeat(60);
        let search = input(json!({ "query": long }));
        assert!(running("web_search", &search, "Search \"word word…").is_some());
        assert!(running("get_goal", &input(json!({})), "Read goal").is_none());
    }

    #[test]
    fn a_long_accepted_input_is_cut_on_a_character_boundary_and_says_so() {
        let prompt = "あ".repeat(RUNNING_TEXT_BYTES);
        let long = input(json!({ "prompt": prompt }));
        let detail = running("delegate", &long, "Delegate").unwrap();
        assert!(detail.text.len() <= RUNNING_TEXT_BYTES);
        assert!(matches!(detail.omitted, Omitted::Later(bytes) if bytes > 0));
    }

    #[test]
    fn the_input_fingerprint_follows_every_value() {
        let base = hash_value(&json!({"a": [1, "x"], "b": null}));
        assert_eq!(base, hash_value(&json!({"a": [1, "x"], "b": null})));
        for changed in [
            json!({"a": [2, "x"], "b": null}),
            json!({"a": [1, "y"], "b": null}),
            json!({"a": [1, "x"], "b": false}),
            json!({"a": [1, "x", 0], "b": null}),
            json!({"a": [1, "x"], "c": null}),
        ] {
            assert_ne!(base, hash_value(&changed), "{changed}");
        }
    }
}
