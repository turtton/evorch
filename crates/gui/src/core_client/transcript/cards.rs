//! The cards of tool calls and the rows of reasoning, notices and errors in a transcript.
//!
//! A closed tool card is one line made of what the call's row carries (its name, the summary
//! of its input, whether it failed), so drawing it reads no body. Opened, it shows the input,
//! the result and the details; the calls a call made sit inside it. Pressing anywhere on a
//! card opens or closes it, except where something else takes the press: a button, a link,
//! text being selected, a card inside it or a scrolled area.
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use egui::{Align, Label, Layout, RichText, Sense, Ui, UiBuilder, WidgetInfo, WidgetType};
use workbench_protocol::{ids::RunId, namespaces::execution::ToolActivityStatus, views::*};

use super::{
    Cache, Effect, PullRequestChips, ViewAnchor, content_text, content_text_when, message_key,
};
use crate::core_client::tool_activity::{LiveTool, Outcome, format_time, is_timed};
use crate::panes::pull_request_widgets::candidate_offer;
use crate::panes::transcript_widgets::{code_block, diff_block};
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::*;
use crate::theme::widgets::soft_frame;

/// Where whether a call's card is open is kept: by the call alone, so that the card keeps its
/// state wherever it is drawn (on its own until its caller is committed, then inside it).
pub(super) fn open_id(run: &RunId, call_id: &str) -> egui::Id {
    egui::Id::new(("tool-card-open", run.as_str(), call_id))
}

/// What a card's header says, whichever record the card is drawn from.
struct Head<'a> {
    name: &'a str,
    summary: Option<&'a str>,
    outcome: Outcome,
    /// Shown only for the tools whose time is shown at all.
    time: Option<Duration>,
}

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
            card(ui, env, view, &tree, at);
        } else {
            aside(ui, env, row);
        }
        visit_row(view, row, top, ui.cursor().min.y);
        ui.add_space(SP_1);
    }
}

/// A row drawn from `top` to `bottom`. A call's row answers to the key of the live card it
/// replaces, so a view anchored on that card stays on the call when it is committed.
fn visit_row(view: &mut ViewAnchor, row: &CommittedMessage, top: f32, bottom: f32) {
    let live = row
        .tool
        .as_ref()
        .map(|tool| live_key_of(&row.run, &tool.call_id));
    view.visit_replacing(message_key(row), live, top, bottom, true);
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

fn card(ui: &mut Ui, env: &mut Env<'_, '_>, view: &mut ViewAnchor, tree: &Tree<'_>, at: usize) {
    let message = tree.rows[at];
    let Some(tool) = &message.tool else {
        return;
    };
    ui.push_id(("tool-card", message.run.as_str(), &tool.call_id), |ui| {
        let id = open_id(&message.run, &tool.call_id);
        let open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
        // The scope senses presses below everything drawn in it, so buttons and the like
        // keep theirs.
        let scope = ui.scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
            soft_frame(palette().SURFACE).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let outcome = Outcome::Finished(match tool.status {
                    ToolCallStatus::Succeeded => ToolActivityStatus::Succeeded,
                    ToolCallStatus::Failed => ToolActivityStatus::Failed,
                });
                let head = Head {
                    name: &tool.tool_name,
                    summary: tool.input_summary.as_deref(),
                    outcome,
                    time: tool
                        .duration_ms
                        .filter(|_| is_timed(&tool.tool_name))
                        .map(Duration::from_millis),
                };
                header(ui, open, &head);
                offers(ui, env, message, tool);
                if open {
                    body(ui, env, view, tree, at, tool);
                }
            });
        });
        if scope.response.clicked() {
            ui.data_mut(|data| data.insert_temp(id, !open));
        }
    });
}

fn header(ui: &mut Ui, open: bool, head: &Head<'_>) {
    let failed = head.outcome == Outcome::Finished(ToolActivityStatus::Failed);
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
        let name = RichText::new(icons::with_icon(icons::WRENCH, head.name)).color(color);
        ui.add(Label::new(name).selectable(false));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (glyph, color, name) = match head.outcome {
                Outcome::Running => (icons::CIRCLE_NOTCH, palette().TEXT_MUTED, "Running"),
                Outcome::Finished(ToolActivityStatus::Succeeded) => {
                    (icons::CHECK, palette().SUCCESS, "Succeeded")
                }
                Outcome::Finished(ToolActivityStatus::Failed) => {
                    (icons::X, palette().ERROR_FG, "Failed")
                }
                Outcome::Finished(ToolActivityStatus::Interrupted) => {
                    (icons::WARNING, palette().WARNING_FG, "Interrupted")
                }
            };
            let status = ui.add(Label::new(RichText::new(glyph).color(color)).selectable(false));
            status.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, name));
            if let Some(time) = head.time {
                let time = muted(format_time(time)).size(FONT_SMALL);
                ui.add(Label::new(time).selectable(false));
            }
            if let Some(summary) = head.summary {
                let summary = muted(summary).size(FONT_SMALL).monospace();
                ui.add(Label::new(summary).truncate().selectable(false));
            }
        });
    });
}

/// Draws the calls the core says are running (and those just finished whose rows are not
/// here yet), after the committed ones. A running card is open until it is pressed, a
/// finished one closed; a card the person pressed keeps its state either way, and the same
/// state carries over to the committed card that replaces it. True while a running call's
/// time is in view and has to be redrawn as it advances.
pub(super) fn live(
    ui: &mut Ui,
    env: &mut Env<'_, '_>,
    tools: &[&LiveTool],
    now: Instant,
    view: &mut ViewAnchor,
) -> bool {
    let mut ticking = false;
    for live in tools {
        let top = ui.cursor().min.y;
        ticking |= live_card(ui, env, live, now);
        view.visit(live_key(live), top, ui.cursor().min.y, true);
        ui.add_space(SP_1);
    }
    ticking
}

pub(super) fn live_key(live: &LiveTool) -> String {
    live_key_of(&live.tool.run, &live.tool.call_id)
}

fn live_key_of(run: &RunId, call_id: &str) -> String {
    format!("live {run}/{call_id}")
}

fn live_card(ui: &mut Ui, env: &mut Env<'_, '_>, live: &LiveTool, now: Instant) -> bool {
    let tool = &live.tool;
    let time = live.time(now);
    let mut ticking = false;
    ui.push_id(("tool-card", tool.run.as_str(), &tool.call_id), |ui| {
        let id = open_id(&tool.run, &tool.call_id);
        let open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(live.running()));
        let scope = ui.scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
            soft_frame(palette().SURFACE).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let head = Head {
                    name: &tool.tool_name,
                    summary: tool.input_summary.as_deref(),
                    outcome: live.outcome(),
                    time,
                };
                header(ui, open, &head);
                if open && let Some(input) = &tool.input {
                    ui.add_space(SP_1);
                    // Read only once the line standing for the unread input is in view.
                    section(ui, env, "Input", input, false, true);
                }
            });
        });
        if scope.response.clicked() {
            ui.data_mut(|data| data.insert_temp(id, !open));
        }
        ticking = live.running() && time.is_some() && ui.is_rect_visible(scope.response.rect);
    });
    ticking
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

fn body(
    ui: &mut Ui,
    env: &mut Env<'_, '_>,
    view: &mut ViewAnchor,
    tree: &Tree<'_>,
    at: usize,
    tool: &ToolCallView,
) {
    let message = tree.rows[at];
    ui.add_space(SP_1);
    if let Some(input) = &tool.input {
        section(ui, env, "Input", input, false, false);
    }
    match &message.content {
        MessageContent::Inline { text } if text.is_empty() => {
            ui.add(Label::new(muted("No output").size(FONT_SMALL)).selectable(false));
        }
        output => section(ui, env, "Output", output, true, false),
    }
    if let Some(detail) = &tool.detail {
        section(ui, env, "Details", detail, false, false);
    }
    if !tree.children[at].is_empty() {
        ui.indent("calls", |ui| {
            for &child in &tree.children[at] {
                let top = ui.cursor().min.y;
                card(ui, env, view, tree, child);
                // A call inside a call is an anchor too: its own body may arrive later.
                visit_row(view, tree.rows[child], top, ui.cursor().min.y);
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
    in_view_only: bool,
) {
    ui.add(Label::new(muted(title).size(FONT_SMALL)).selectable(false));
    let Some(text) = content_text_when(
        ui,
        env.requested_bodies,
        env.cache,
        content,
        env.effects,
        in_view_only,
    ) else {
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
    // Keyed by the row, not by where it is drawn: it moves into a group when a call follows.
    let id = egui::Id::new(("reasoning-open", message.run.as_str(), message.index.get()));
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
