//! The cards of tool calls and the rows of reasoning, notices and errors in a transcript.
//!
//! A closed tool card is one line made of what the call's row carries (its name, the summary
//! of its input, whether it failed), so drawing it reads no body. Opened, it shows the input,
//! the result and the details; the calls a call made sit inside it. Pressing anywhere on a
//! card opens or closes it, except where something else takes the press: a button, a link,
//! text being selected, a card inside it or a scrolled area.
use std::collections::{BTreeSet, HashMap};

use egui::{Align, Label, Layout, RichText, Sense, Ui, UiBuilder, WidgetInfo, WidgetType};
use workbench_protocol::views::*;

use super::{Cache, Effect, PullRequestChips, ViewAnchor, content_text, message_key};
use crate::panes::pull_request_widgets::candidate_offer;
use crate::panes::transcript_widgets::{code_block, diff_block};
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::*;
use crate::theme::widgets::soft_frame;

/// An input, a result or details taller than this scroll inside their card.
const SECTION_MAX_HEIGHT: f32 = 320.0;

/// What drawing needs besides the ui: the bodies asked for, the cache they are read from,
/// the pull request suggestions to offer, and where the person's presses go.
pub(super) struct Env<'a, 'b> {
    pub requested_bodies: &'b mut BTreeSet<String>,
    pub cache: &'a Cache,
    pub chips: Option<&'a PullRequestChips<'a>>,
    pub effects: &'b mut Vec<Effect>,
}

/// Draws the rows of a group of tool calls in order. A call made by another call of the group
/// is drawn inside that call's card; one whose caller is not in the group (older rows not read
/// yet, or never recorded) stands on its own.
pub(super) fn rows(
    ui: &mut Ui,
    env: &mut Env<'_, '_>,
    rows: &[&CommittedMessage],
    view: &mut ViewAnchor,
) {
    let tree = Tree::of(rows);
    for (at, row) in rows.iter().enumerate() {
        if tree.nested[at] {
            continue;
        }
        let top = ui.cursor().min.y;
        if row.role == MessageRole::Tool {
            card(ui, env, &tree, at);
        } else {
            aside(ui, env, row);
        }
        view.visit(message_key(row), top, ui.cursor().min.y, true);
        ui.add_space(SP_1);
    }
}

/// Which calls of a group a call made, by position.
struct Tree<'a> {
    rows: &'a [&'a CommittedMessage],
    children: Vec<Vec<usize>>,
    nested: Vec<bool>,
}

impl<'a> Tree<'a> {
    fn of(rows: &'a [&'a CommittedMessage]) -> Self {
        let call = |row: &CommittedMessage, call_id: &str| {
            (row.run.as_str().to_owned(), call_id.to_owned())
        };
        let positions: HashMap<_, _> = rows
            .iter()
            .enumerate()
            .filter_map(|(at, row)| Some((call(row, &row.tool.as_ref()?.call_id), at)))
            .collect();
        let mut children = vec![Vec::new(); rows.len()];
        let mut nested = vec![false; rows.len()];
        for (at, row) in rows.iter().enumerate() {
            let Some(parent) = row
                .tool
                .as_ref()
                .and_then(|tool| tool.parent_call_id.as_deref())
            else {
                continue;
            };
            // A call is committed once it ends, after the calls it made. A caller committed
            // earlier (or the call itself) is not drawn around it: nothing may vanish.
            if let Some(&parent_at) = positions.get(&call(row, parent))
                && parent_at > at
            {
                children[parent_at].push(at);
                nested[at] = true;
            }
        }
        Self {
            rows,
            children,
            nested,
        }
    }
}

fn card(ui: &mut Ui, env: &mut Env<'_, '_>, tree: &Tree<'_>, at: usize) {
    let message = tree.rows[at];
    let Some(tool) = &message.tool else {
        return;
    };
    ui.push_id(("tool-card", message.run.as_str(), &tool.call_id), |ui| {
        let id = ui.id().with("open");
        let open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
        // The scope senses presses below everything drawn in it, so buttons and the like
        // keep theirs.
        let scope = ui.scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
            soft_frame(palette().SURFACE).show(ui, |ui| {
                ui.set_width(ui.available_width());
                header(ui, open, tool);
                offers(ui, env, message, tool);
                if open {
                    body(ui, env, tree, at, tool);
                }
            });
        });
        if scope.response.clicked() {
            ui.data_mut(|data| data.insert_temp(id, !open));
        }
    });
}

fn header(ui: &mut Ui, open: bool, tool: &ToolCallView) {
    let failed = tool.status == ToolCallStatus::Failed;
    ui.horizontal(|ui| {
        let caret = if open {
            icons::CARET_DOWN
        } else {
            icons::CARET_RIGHT
        };
        ui.add(Label::new(muted(caret)).selectable(false));
        let color = if failed {
            palette().ERROR_FG
        } else {
            palette().TEXT
        };
        let name = RichText::new(icons::with_icon(icons::WRENCH, &tool.tool_name)).color(color);
        ui.add(Label::new(name).selectable(false));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (glyph, color, name) = if failed {
                (icons::X, palette().ERROR_FG, "Failed")
            } else {
                (icons::CHECK, palette().SUCCESS, "Succeeded")
            };
            let status = ui.add(Label::new(RichText::new(glyph).color(color)).selectable(false));
            status.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, name));
            if let Some(summary) = &tool.input_summary {
                let summary = muted(summary).size(FONT_SMALL).monospace();
                ui.add(Label::new(summary).truncate().selectable(false));
            }
        });
    });
}

/// The pull requests this call suggested, while they are open.
fn offers(ui: &mut Ui, env: &mut Env<'_, '_>, message: &CommittedMessage, tool: &ToolCallView) {
    let Some(chips) = env.chips else {
        return;
    };
    for (candidate, label) in chips.offers(&message.run, &tool.call_id) {
        let choice = ui
            .push_id(candidate, |ui| {
                ui.horizontal_wrapped(|ui| candidate_offer(ui, &label, chips.can_write()))
                    .inner
            })
            .inner;
        if let Some(choice) = choice {
            env.effects
                .push(Effect::PullRequestChip(chips.action(candidate, choice)));
        }
    }
}

fn body(ui: &mut Ui, env: &mut Env<'_, '_>, tree: &Tree<'_>, at: usize, tool: &ToolCallView) {
    let message = tree.rows[at];
    ui.add_space(SP_1);
    if let Some(input) = &tool.input {
        section(ui, env, "Input", input, false);
    }
    match &message.content {
        MessageContent::Inline { text } if text.is_empty() => {
            ui.add(Label::new(muted("No output").size(FONT_SMALL)).selectable(false));
        }
        output => section(ui, env, "Output", output, true),
    }
    if let Some(detail) = &tool.detail {
        section(ui, env, "Details", detail, false);
    }
    if !tree.children[at].is_empty() {
        ui.indent("calls", |ui| {
            for &child in &tree.children[at] {
                card(ui, env, tree, child);
                ui.add_space(SP_1);
            }
        });
    }
}

/// A titled text block that scrolls when it is tall. The result alone may be a change, drawn
/// as one.
fn section(
    ui: &mut Ui,
    env: &mut Env<'_, '_>,
    title: &str,
    content: &MessageContent,
    result: bool,
) {
    ui.add(Label::new(muted(title).size(FONT_SMALL)).selectable(false));
    let Some(text) = content_text(ui, env.requested_bodies, env.cache, content, env.effects) else {
        return;
    };
    egui::ScrollArea::vertical()
        .id_salt(title)
        .max_height(SECTION_MAX_HEIGHT)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            if result && looks_like_diff(text) {
                diff_block(ui, text);
            } else {
                code_block(ui, text, palette().TEXT);
            }
        });
}

fn looks_like_diff(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("@@ ") || line.starts_with("+++ "))
}

/// Reasoning is one closed line until it is pressed; a notice or an error is one muted line.
pub(super) fn aside(ui: &mut Ui, env: &mut Env<'_, '_>, message: &CommittedMessage) {
    ui.push_id(("aside", message.run.as_str(), message.index.get()), |ui| {
        if message.role == MessageRole::Reasoning {
            reasoning(ui, env, message);
        } else {
            note(ui, env, message);
        }
    });
}

fn reasoning(ui: &mut Ui, env: &mut Env<'_, '_>, message: &CommittedMessage) {
    let id = ui.id().with("open");
    let open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    let scope = ui.scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
        let caret = if open {
            icons::CARET_DOWN
        } else {
            icons::CARET_RIGHT
        };
        let title = icons::with_icon(icons::BRAIN, "Reasoning");
        ui.add(Label::new(muted(format!("{caret} {title}")).size(FONT_SMALL)).selectable(false));
        if open
            && let Some(text) = content_text(
                ui,
                env.requested_bodies,
                env.cache,
                &message.content,
                env.effects,
            )
        {
            ui.add(Label::new(muted(text).size(FONT_SMALL)).wrap());
        }
    });
    if scope.response.clicked() {
        ui.data_mut(|data| data.insert_temp(id, !open));
    }
}

fn note(ui: &mut Ui, env: &mut Env<'_, '_>, message: &CommittedMessage) {
    let (icon, color) = if message.role == MessageRole::Error {
        (icons::WARNING, palette().ERROR_FG)
    } else {
        (icons::INFO, palette().TEXT_MUTED)
    };
    let Some(text) = content_text(
        ui,
        env.requested_bodies,
        env.cache,
        &message.content,
        env.effects,
    ) else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(icon).color(color));
        ui.add(Label::new(RichText::new(text).size(FONT_SMALL).color(color)).wrap());
    });
}
