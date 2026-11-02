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
/// A shell command up to this long, on one line, is complete in the header.
const SHELL_ONE_LINE_CHARS: usize = 60;
/// Arguments that fit this on one line already read fine in the header.
const GENERIC_MIN_BYTES: usize = 120;

pub(super) struct LiveDetail {
    pub text: String,
    omitted: Omitted,
    /// A draft grows at its end, so its newest text stays in view.
    follow_tail: bool,
}

enum Omitted {
    Nothing,
    /// Older text no longer fits what is kept.
    Earlier,
    /// This many bytes after the shown text are not laid out.
    Later(usize),
}

/// The detail of a call the model is still composing.
pub(super) fn drafting(text: &str, arguments: &str, omitted: bool) -> Option<LiveDetail> {
    let shown = if text.is_empty() { arguments } else { text };
    (!shown.trim().is_empty()).then(|| LiveDetail {
        text: shown.to_owned(),
        omitted: if omitted {
            Omitted::Earlier
        } else {
            Omitted::Nothing
        },
        follow_tail: true,
    })
}

/// The detail of a call the runtime accepted. `input` must already be redacted.
pub(super) fn running(tool_name: &str, input: &Value) -> Option<LiveDetail> {
    let field = |key: &str| {
        input
            .get(key)?
            .as_str()
            .filter(|text| !text.trim().is_empty())
    };
    let text = match tool_name {
        "delegate" => field("prompt").map(str::to_owned),
        // The header already holds a short one-line command.
        "bash" | "shell" => field("command")
            .filter(|command| {
                command.contains('\n') || command.chars().count() > SHELL_ONE_LINE_CHARS
            })
            .map(str::to_owned),
        _ => None,
    };
    let text = text.or_else(|| {
        let compact = input.to_string();
        (compact.len() > GENERIC_MIN_BYTES && !matches!(tool_name, "bash" | "shell"))
            .then(|| super::pretty_json(input))
    })?;
    let mut shown = text;
    let mut cut = shown.len().min(RUNNING_TEXT_BYTES);
    while !shown.is_char_boundary(cut) {
        cut -= 1;
    }
    let omitted = if cut < shown.len() {
        Omitted::Later(shown.len() - cut)
    } else {
        Omitted::Nothing
    };
    shown.truncate(cut);
    Some(LiveDetail {
        text: shown,
        omitted,
        follow_tail: false,
    })
}

/// Draws the area. `expanded` is the card's display state; `id` keeps the
/// scroll position across frames and from a draft to the running call.
pub(super) fn show(ui: &mut Ui, id: egui::Id, detail: &LiveDetail, expanded: bool) {
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
                        RichText::new(&detail.text)
                            .font(font.clone())
                            .color(palette().TEXT),
                    )
                    .wrap(),
                );
                if let Omitted::Later(bytes) = detail.omitted {
                    note(ui, &format!("… {bytes} more bytes are not shown"));
                }
            });
            clipped = output.content_size.y > output.inner_rect.height() + 1.0;
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
        let delegate = json!({"name": "w", "prompt": "line 1\nline 2"});
        assert_eq!(
            running("delegate", &delegate).unwrap().text,
            "line 1\nline 2"
        );
        let shell = json!({"command": "cd a &&\n  make"});
        assert_eq!(running("shell", &shell).unwrap().text, "cd a &&\n  make");
        assert!(running("shell", &json!({"command": "cargo test"})).is_none());
        let long = "x".repeat(SHELL_ONE_LINE_CHARS + 1);
        assert_eq!(
            running("shell", &json!({"command": long})).unwrap().text,
            long
        );
        // Short arguments already read in the header; control calls have none.
        assert!(running("read", &json!({"path": "a.rs"})).is_none());
        assert!(
            running(
                "shell",
                &json!({"action": "poll", "job_id": "x".repeat(200)})
            )
            .is_none()
        );
    }

    #[test]
    fn a_long_accepted_input_is_cut_on_a_character_boundary_and_says_so() {
        let prompt = "あ".repeat(RUNNING_TEXT_BYTES);
        let detail = running("delegate", &json!({ "prompt": prompt })).unwrap();
        assert!(detail.text.len() <= RUNNING_TEXT_BYTES);
        assert!(matches!(detail.omitted, Omitted::Later(bytes) if bytes > 0));
    }
}
