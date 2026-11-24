//! The committed transcript of the core client: the thread view in the Conversation tab,
//! which shows the root runs only, and the run tabs, which show one run each. Like the
//! embedded workbench, a subagent's messages appear only in its own transcript tab.
use std::collections::BTreeSet;
use std::path::Path;

use egui::{RichText, Ui};
use workbench_protocol::{ids::*, views::*};

use super::cache::Cache;
use super::connection::Action;
use super::shell::Effect;
use crate::panes::markdown_render::render_markdown_in_workspace;
use crate::panes::transcript_widgets::{code_block, diff_block, user_bubble};
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::*;
use crate::theme::widgets::{ghost, ghost_icon_button, soft_frame};

#[derive(Clone, Copy)]
pub(super) enum Scope<'a> {
    Thread(&'a ThreadId),
    Run {
        thread: &'a ThreadId,
        run: &'a RunId,
    },
}

#[derive(Clone, Copy)]
pub(super) struct TranscriptView<'a> {
    pub cache: &'a Cache,
    pub scope: Scope<'a>,
    /// What relative links and images in the messages resolve against.
    pub base: Option<&'a Path>,
}

/// Draws the messages of `scope` in a scroll area that follows the newest one, with
/// "Load earlier messages" while older ones exist. `empty` is drawn when no message is
/// shown, and `cards` after the messages.
pub(super) fn transcript(
    ui: &mut Ui,
    requested_bodies: &mut BTreeSet<String>,
    view: TranscriptView<'_>,
    empty: impl FnOnce(&mut Ui),
    cards: impl FnOnce(&mut Ui, &mut Vec<Effect>),
    effects: &mut Vec<Effect>,
) {
    let TranscriptView { cache, scope, base } = view;
    let mut failed = None;
    let (salt, messages, older, load_earlier): (_, Vec<_>, _, _) = match scope {
        Scope::Thread(thread) => (
            ("core-transcript", thread.as_str()),
            cache
                .history
                .get(thread.as_str())
                .into_iter()
                .flatten()
                .filter(|m| cache.shows_in_thread(m))
                .collect(),
            cache.older.get(thread.as_str()),
            Action::History(thread.clone(), None, true),
        ),
        Scope::Run { thread, run } => {
            failed = cache
                .run_read_failed(run.as_str())
                .then(|| Action::History(thread.clone(), Some(run.clone()), false));
            (
                ("core-run-transcript", run.as_str()),
                cache
                    .run_history
                    .get(run.as_str())
                    .into_iter()
                    .flatten()
                    .filter(|m| &m.run == run)
                    .collect(),
                cache.run_older.get(run.as_str()),
                Action::History(thread.clone(), Some(run.clone()), true),
            )
        }
    };
    egui::ScrollArea::vertical()
        .id_salt(salt)
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(SP_2);
            if older.is_some_and(Option::is_some) {
                ui.vertical_centered(|ui| {
                    if ui
                        .add(ghost(icons::with_icon(
                            icons::ARROW_UP,
                            "Load earlier messages",
                        )))
                        .clicked()
                    {
                        effects.push(Effect::send(load_earlier));
                    }
                });
            }
            if let Some(retry) = failed {
                ui.horizontal(|ui| {
                    ui.label(muted("Could not read this run's history"));
                    if ui.add(ghost("Retry")).clicked() {
                        effects.push(Effect::send(retry));
                    }
                });
            }
            if messages.is_empty() {
                empty(ui);
            }
            for item in items(&messages) {
                match item {
                    Item::Message(message) => {
                        message_row(ui, requested_bodies, cache, base, message, effects);
                    }
                    Item::Tools {
                        tools,
                        folded: true,
                    } => {
                        tool_group(ui, requested_bodies, cache, &tools, effects);
                    }
                    Item::Tools {
                        tools,
                        folded: false,
                    } => tool_cards(ui, requested_bodies, cache, &tools, effects),
                }
                ui.add_space(SP_2);
            }
            cards(ui, effects);
            ui.add_space(SP_2);
        });
}

/// A run of one run's tool outputs folds under one header once the run's next message
/// is a reply to the person, as the embedded workbench folds a turn's tool calls; until
/// then (the agent still works, or a person's message came next) each stays in sight.
#[derive(Debug, PartialEq, Eq)]
enum Item<'a> {
    Message(&'a CommittedMessage),
    Tools {
        tools: Vec<&'a CommittedMessage>,
        folded: bool,
    },
}

fn items<'a>(history: &[&'a CommittedMessage]) -> Vec<Item<'a>> {
    let mut items: Vec<Item<'a>> = Vec::new();
    for message in history {
        let same_run = |tools: &[&CommittedMessage]| tools[0].run == message.run;
        match (message.role, items.last_mut()) {
            (MessageRole::Tool, Some(Item::Tools { tools, .. })) if same_run(tools) => {
                tools.push(message);
            }
            (MessageRole::Tool, _) => items.push(Item::Tools {
                tools: vec![message],
                folded: false,
            }),
            (MessageRole::Assistant, Some(Item::Tools { tools, folded })) if same_run(tools) => {
                *folded = true;
                items.push(Item::Message(message));
            }
            _ => items.push(Item::Message(message)),
        }
    }
    items
}

fn message_text<'a>(
    ui: &mut Ui,
    requested_bodies: &mut BTreeSet<String>,
    cache: &'a Cache,
    message: &'a CommittedMessage,
    effects: &mut Vec<Effect>,
) -> Option<&'a str> {
    match &message.content {
        MessageContent::Inline { text } => Some(text),
        MessageContent::Body(body) => {
            if let Some(text) = cache.bodies.get(body.body_id.as_str()) {
                return Some(text);
            }
            if requested_bodies.insert(body.body_id.to_string()) {
                effects.push(Effect::send(Action::Body(body.body_id.clone())));
            }
            ui.horizontal(|ui| {
                ui.label(muted(format!(
                    "Loading message ({})",
                    byte_size(body.total_bytes.get())
                )));
                if ui.add(ghost("Retry")).clicked() {
                    effects.push(Effect::send(Action::Body(body.body_id.clone())));
                }
            });
            None
        }
    }
}

pub(super) fn byte_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

fn message_row(
    ui: &mut Ui,
    requested_bodies: &mut BTreeSet<String>,
    cache: &Cache,
    base: Option<&Path>,
    message: &CommittedMessage,
    effects: &mut Vec<Effect>,
) {
    ui.push_id(
        ("message", message.run.as_str(), message.index.get()),
        |ui| match message.role {
            MessageRole::User => {
                if let Some(text) = message_text(ui, requested_bodies, cache, message, effects) {
                    user_bubble(ui, text, &[]);
                }
            }
            MessageRole::Assistant | MessageRole::Tool => {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(SP_1 as i8, SP_1 as i8))
                    .show(ui, |ui| {
                        if let Some(text) =
                            message_text(ui, requested_bodies, cache, message, effects)
                        {
                            render_markdown_in_workspace(
                                ui,
                                text,
                                &format!("msg-{}-{}", message.run, message.index.get()),
                                base,
                            );
                        }
                    });
            }
        },
    );
}

/// The header of a folded run of tool outputs, closed until the person opens it, as the
/// embedded workbench draws its tool-call groups.
fn tool_group(
    ui: &mut Ui,
    requested_bodies: &mut BTreeSet<String>,
    cache: &Cache,
    tools: &[&CommittedMessage],
    effects: &mut Vec<Effect>,
) {
    let first = tools[0];
    let id = ui.make_persistent_id(("tools", first.run.as_str(), first.index.get()));
    let failed = tools.iter().any(|tool| match &tool.content {
        MessageContent::Inline { text } => ToolOutput::parse(text).failed(),
        MessageContent::Body(_) => false,
    });
    let count = tools.len();
    let mut title = if count == 1 {
        "1 tool output".to_owned()
    } else {
        format!("{count} tool outputs")
    };
    if failed {
        title.push_str(", some failed");
    }
    let open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    let icon = if open {
        icons::CARET_DOWN
    } else {
        icons::CARET_RIGHT
    };
    if ghost_icon_button(ui, icon, &title).clicked() {
        ui.data_mut(|data| data.insert_temp(id, !open));
    }
    if open {
        tool_cards(ui, requested_bodies, cache, tools, effects);
    }
}

fn tool_cards(
    ui: &mut Ui,
    requested_bodies: &mut BTreeSet<String>,
    cache: &Cache,
    tools: &[&CommittedMessage],
    effects: &mut Vec<Effect>,
) {
    for tool in tools {
        // The same id folded or not, so a card opened while the agent worked stays open.
        let id = ui.make_persistent_id(("tool-card", tool.run.as_str(), tool.index.get()));
        if let Some(text) = message_text(ui, requested_bodies, cache, tool, effects) {
            tool_card(ui, id, text);
        }
        ui.add_space(SP_1);
    }
}

/// What the shape of a committed tool output says about it. The core commits the output
/// alone, without the tool's name, input or error flag, so this reads the formats the
/// tools write: a shell result begins with its exit code, a change is a unified diff.
#[derive(Debug, PartialEq, Eq)]
enum ToolOutput<'a> {
    Shell { exit_code: i64, body: &'a str },
    Diff { added: usize, removed: usize },
    Text,
}

impl<'a> ToolOutput<'a> {
    fn parse(text: &'a str) -> Self {
        if let Some(rest) = text.strip_prefix("exit_code:") {
            let (code, body) = rest.split_once('\n').unwrap_or((rest, ""));
            if let Ok(exit_code) = code.trim().parse() {
                return Self::Shell { exit_code, body };
            }
        }
        let is_diff = text
            .lines()
            .any(|line| line.starts_with("@@ ") || line.starts_with("+++ "));
        if is_diff {
            let added = text
                .lines()
                .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                .count();
            let removed = text
                .lines()
                .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                .count();
            return Self::Diff { added, removed };
        }
        Self::Text
    }

    fn failed(&self) -> bool {
        matches!(self, Self::Shell { exit_code, .. } if *exit_code != 0)
    }
}

fn tool_card(ui: &mut Ui, id: egui::Id, text: &str) {
    let output = ToolOutput::parse(text);
    let (icon, title, meta) = match &output {
        ToolOutput::Shell { exit_code, .. } => {
            (icons::TERMINAL, "Shell", Some(format!("exit {exit_code}")))
        }
        ToolOutput::Diff { added, removed } => (
            icons::GIT_DIFF,
            "Change",
            Some(format!("+{added} −{removed}")),
        ),
        ToolOutput::Text => (icons::WRENCH, "Output", None),
    };
    let lines = text.lines().count();
    let first_line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    soft_frame(palette().SURFACE).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                let color = if output.failed() {
                    palette().ERROR_FG
                } else {
                    palette().TEXT
                };
                ui.label(RichText::new(icons::with_icon(icon, title)).color(color));
                let mut summary = String::new();
                if let Some(meta) = &meta {
                    summary.push_str(meta);
                    summary.push_str(" · ");
                }
                summary.push_str(&format!(
                    "{lines} line{}",
                    if lines == 1 { "" } else { "s" }
                ));
                ui.label(muted(summary).size(FONT_SMALL));
                ui.add(egui::Label::new(muted(first_line).size(FONT_SMALL).monospace()).truncate());
            })
            .body(|ui| match output {
                ToolOutput::Diff { .. } => diff_block(ui, text),
                ToolOutput::Shell { exit_code, body } => code_block(
                    ui,
                    body,
                    if exit_code == 0 {
                        palette().TEXT
                    } else {
                        palette().ERROR_FG
                    },
                ),
                ToolOutput::Text => code_block(ui, text, palette().TEXT),
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_result_is_read_from_its_exit_code_line() {
        assert_eq!(
            ToolOutput::parse("exit_code: 2\n--- stderr ---\nboom"),
            ToolOutput::Shell {
                exit_code: 2,
                body: "--- stderr ---\nboom"
            }
        );
        assert!(ToolOutput::parse("exit_code: 1\n").failed());
        assert!(!ToolOutput::parse("exit_code: 0\nok").failed());
    }

    #[test]
    fn a_unified_diff_is_counted_without_its_file_headers() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n+more\n context";
        assert_eq!(
            ToolOutput::parse(diff),
            ToolOutput::Diff {
                added: 2,
                removed: 1
            }
        );
    }

    #[test]
    fn plain_output_is_neither_a_shell_result_nor_a_diff() {
        assert_eq!(ToolOutput::parse("exit_code: x\nhello"), ToolOutput::Text);
        assert_eq!(ToolOutput::parse("just text"), ToolOutput::Text);
    }

    // Given: a root run, its child, and a run the cache does not know
    // Then: the thread view keeps the root's and the unknown run's messages only
    #[test]
    fn the_thread_view_keeps_roots_and_unknown_runs_and_drops_children() {
        let mut cache = Cache::default();
        for (id, parent) in [("root", None), ("child", Some("root"))] {
            let run: RunSummary = serde_json::from_value(serde_json::json!({
                "id": id, "thread": "thr", "role": "worker", "phase": "running",
                "revision": "1", "parent_run": parent,
            }))
            .unwrap();
            cache.runs.insert(id.into(), run);
        }
        let message = |run: &str| -> CommittedMessage {
            serde_json::from_value(serde_json::json!({
                "thread": "thr", "run": run, "index": "0", "role": "assistant",
                "content": {"type": "inline", "payload": {"text": "x"}},
            }))
            .unwrap()
        };
        assert!(cache.shows_in_thread(&message("root")));
        assert!(!cache.shows_in_thread(&message("child")));
        assert!(cache.shows_in_thread(&message("gone")));
    }

    fn committed(run: &str, index: u64, role: &str, text: &str) -> CommittedMessage {
        serde_json::from_value(serde_json::json!({
            "thread": "thr", "run": run, "index": index.to_string(), "role": role,
            "content": {"type": "inline", "payload": {"text": text}},
        }))
        .unwrap()
    }

    fn shapes(history: &[CommittedMessage]) -> Vec<(usize, bool)> {
        let history: Vec<_> = history.iter().collect();
        items(&history)
            .iter()
            .filter_map(|item| match item {
                Item::Tools { tools, folded } => Some((tools.len(), *folded)),
                Item::Message(_) => None,
            })
            .collect()
    }

    // Given: tool outputs followed by the same run's reply, by nothing, by the person's
    //        message, and by another run's reply after another run's tools
    // Then: only those the same run replied after fold; each run's tools are a group
    #[test]
    fn tool_outputs_fold_once_their_run_replies() {
        let tool = |run, index| committed(run, index, "tool", "exit_code: 0\nok");
        let replied = [
            tool("r", 0),
            tool("r", 1),
            committed("r", 2, "assistant", "done"),
        ];
        assert_eq!(shapes(&replied), [(2, true)]);
        assert_eq!(shapes(&[tool("r", 0), tool("r", 1)]), [(2, false)]);
        let interrupted = [tool("r", 0), committed("r", 1, "user", "stop")];
        assert_eq!(shapes(&interrupted), [(1, false)]);
        let mixed = [
            tool("r", 0),
            tool("s", 0),
            committed("r", 1, "assistant", "not s's reply"),
        ];
        assert_eq!(shapes(&mixed), [(1, false), (1, false)]);
    }

    fn harness(cache: Cache) -> egui_kittest::Harness<'static, Cache> {
        egui_kittest::Harness::new_ui_state(
            |ui, cache: &mut Cache| {
                let thread = ThreadId::new("thr").unwrap();
                let mut bodies = BTreeSet::new();
                let mut effects = Vec::new();
                transcript(
                    ui,
                    &mut bodies,
                    TranscriptView {
                        cache,
                        scope: Scope::Thread(&thread),
                        base: None,
                    },
                    |_| {},
                    |_, _| {},
                    &mut effects,
                );
            },
            cache,
        )
    }

    // Given: a shell result and a change committed while the agent still works
    // When: the agent's reply is committed, then the folded header is clicked
    // Then: both cards are in sight without a header while it works; once it replied
    //       only "2 tool outputs" shows; a click opens it and it stays open
    #[test]
    fn tool_outputs_stay_in_sight_until_the_reply_and_then_fold() {
        use egui_kittest::kittest::Queryable;
        let mut cache = Cache::default();
        cache.history.insert(
            "thr".into(),
            vec![
                committed("r", 0, "user", "fix it"),
                committed("r", 1, "tool", "exit_code: 0\nsrc/lib.rs"),
                committed("r", 2, "tool", "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b"),
            ],
        );
        let mut harness = harness(cache);
        harness.run_steps(2);
        harness.get_by_label_contains("Shell");
        harness.get_by_label_contains("Change");
        assert!(harness.query_by_label_contains("tool outputs").is_none());

        harness
            .state_mut()
            .history
            .get_mut("thr")
            .unwrap()
            .push(committed("r", 3, "assistant", "Fixed."));
        harness.run_steps(2);
        harness.get_by_label("2 tool outputs");
        assert!(harness.query_by_label_contains("Shell").is_none());

        harness.get_by_label("2 tool outputs").click();
        harness.run_steps(4);
        harness.get_by_label_contains("Shell");
        harness.get_by_label_contains("Change");
    }
}
