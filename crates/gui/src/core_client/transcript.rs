//! The committed transcript of the core client: the thread view in the Conversation tab,
//! which shows the root runs only, and the run tabs, which show one run each. Like the
//! embedded workbench, a subagent's messages appear only in its own transcript tab.
use std::collections::BTreeSet;
use std::path::Path;

use egui::Ui;
use workbench_protocol::{ids::*, views::*};

use super::cache::Cache;
use super::connection::Action;
use super::features::pull_requests::PullRequestChips;
use super::shell::Effect;
use super::tool_activity::LiveTool;
use crate::panes::markdown_render::render_markdown_in_workspace;
use crate::panes::transcript_widgets::user_bubble;
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::*;
use crate::theme::widgets::{ghost, ghost_icon_button};

mod cards;

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
    /// The open pull request suggestions, offered on the cards of the calls that made them.
    pub chips: Option<&'a PullRequestChips<'a>>,
}

/// How often a running call's time is redrawn while it is in view.
const LIVE_TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// Height of the row above the first message that stands for the older ones.
const OLDER_ROW_HEIGHT: f32 = 28.0;

/// Draws the messages of `scope` in a scroll area that follows the newest one. While
/// older ones exist, the row above the first message reads the next older page once it
/// scrolls into view, with a spinner while that page is read. `empty` is drawn when no
/// message is shown, and `cards` after the messages.
pub(super) fn transcript(
    ui: &mut Ui,
    requested_bodies: &mut BTreeSet<String>,
    view: TranscriptView<'_>,
    empty: impl FnOnce(&mut Ui),
    cards: impl FnOnce(&mut Ui, &mut Vec<Effect>),
    effects: &mut Vec<Effect>,
) {
    let TranscriptView {
        cache,
        scope,
        base,
        chips,
    } = view;
    let chips = chips.filter(|chips| {
        chips.thread
            == match scope {
                Scope::Thread(thread) | Scope::Run { thread, .. } => thread,
            }
    });
    let mut failed = None;
    let (salt, messages, older, earlier): (_, Vec<_>, _, _) = match scope {
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
            (thread.clone(), None),
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
                (thread.clone(), Some(run.clone())),
            )
        }
    };
    let load_earlier = || Effect::send(Action::History(earlier.0.clone(), earlier.1.clone(), true));
    let older_id = match scope {
        Scope::Thread(thread) => thread.as_str(),
        Scope::Run { run, .. } => run.as_str(),
    };
    let loading = cache.older_loading(older_id);
    let older_failed = cache.older_read_failed(older_id);
    // The first message that starts in view last frame (else the one that spans the top
    // of the view) and where it started in the content. When the data shown changed
    // since then (older messages put in front, a body that arrives for a message above)
    // the content above it may have changed height and moved it, and the offset takes
    // that up so the messages in view stay where they are. A frame that changed nothing
    // but the view (a group opened, a scroll, a resize) is left to egui. A message that
    // spans the top is the second choice: it may grow itself.
    let memory = ui.make_persistent_id(salt).with("view-anchor");
    let before: Option<(Option<(String, f32)>, u64)> = ui.data(|data| data.get_temp(memory));
    // The calls running, unless their committed rows are among the messages: the row is the
    // record, and a card that outlived it would be drawn twice.
    let recorded: std::collections::HashSet<_> = messages
        .iter()
        .filter_map(|m| Some((m.run.as_str(), m.tool.as_ref()?.call_id.as_str())))
        .collect();
    let live = cache.live_tools.shown(|live| {
        let tool = &live.tool;
        let in_scope = match scope {
            Scope::Thread(thread) => {
                &tool.thread == thread
                    && cache
                        .runs
                        .get(tool.run.as_str())
                        .is_none_or(|run| run.parent_run.is_none())
            }
            Scope::Run { run, .. } => &tool.run == run,
        };
        in_scope && !recorded.contains(&(tool.run.as_str(), tool.call_id.as_str()))
    });
    let shown = data_fingerprint(
        cache,
        &messages,
        &live,
        (older.is_some_and(Option::is_some), older_failed),
    );
    let mut view = ViewAnchor {
        previous: before.clone().and_then(|(anchor, _)| anchor),
        ..ViewAnchor::default()
    };
    let data_changed = before.as_ref().is_some_and(|(_, was)| *was != shown);
    let mut near_top = false;
    let mut ticking = false;
    let output = egui::ScrollArea::vertical()
        .id_salt(salt)
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            view.origin = ui.cursor().min.y;
            view.top = ui.clip_rect().min.y;
            ui.add_space(SP_2);
            if older.is_some_and(Option::is_some) {
                if older_failed {
                    ui.horizontal(|ui| {
                        ui.label(muted("Could not read earlier messages"));
                        if ui.add(ghost("Retry")).clicked() {
                            effects.push(load_earlier());
                        }
                    });
                } else {
                    let (row, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), OLDER_ROW_HEIGHT),
                        egui::Sense::hover(),
                    );
                    near_top = ui.is_rect_visible(row);
                    if loading {
                        let spinner =
                            egui::Rect::from_center_size(row.center(), egui::vec2(16.0, 16.0));
                        ui.put(spinner, egui::Spinner::new().size(16.0));
                    }
                }
            }
            if let Some(retry) = failed {
                ui.horizontal(|ui| {
                    ui.label(muted("Could not read this run's history"));
                    if ui.add(ghost("Retry")).clicked() {
                        effects.push(Effect::send(retry));
                    }
                });
            }
            let items = items(&messages);
            if items.is_empty() {
                empty(ui);
            }
            for item in items {
                match item {
                    Item::Message(message) => {
                        let top = ui.cursor().min.y;
                        message_row(ui, requested_bodies, cache, base, chips, message, effects);
                        view.visit(message_key(message), top, ui.cursor().min.y, true);
                    }
                    Item::Tools { rows, folded } => {
                        let mut env = cards::Env {
                            requested_bodies,
                            cache,
                            chips,
                            effects,
                        };
                        if folded {
                            tool_group(ui, &mut env, &rows, &mut view);
                        } else {
                            cards::rows(ui, &mut env, &rows, &mut view);
                        }
                    }
                }
                ui.add_space(SP_2);
            }
            if !live.is_empty() {
                let mut env = cards::Env {
                    requested_bodies,
                    cache,
                    chips,
                    effects,
                };
                let now = cache.live_tools.now();
                ticking = cards::live(ui, &mut env, &live, now, &mut view);
                ui.add_space(SP_2);
            }
            cards(ui, effects);
            ui.add_space(SP_2);
        });
    if ticking {
        ui.ctx().request_repaint_after(LIVE_TICK);
    }
    let shift = match (&view.previous, view.moved_to) {
        (Some((_, was)), Some(now)) if data_changed && (now - was).abs() > 0.5 => now - was,
        _ => 0.0,
    };
    let shifted = shift != 0.0;
    if shifted {
        let mut state = output.state;
        state.offset.y = (state.offset.y + shift).max(0.0);
        state.store(ui.ctx(), output.id);
        ui.ctx()
            .request_discard("keep the messages in view in place");
    }
    ui.data_mut(|data| data.insert_temp(memory, (view.first.or(view.spanning), shown)));
    // The first frame of an area is laid out at the top before it follows the newest
    // message, so it only measures; a page is read from the next frame on.
    if before.is_none() || shifted {
        ui.ctx().request_repaint();
    } else if near_top && !loading && !older_failed {
        effects.push(load_earlier());
    }
}

/// A run of one run's tool calls folds under one header once the run's next reply to the
/// person is committed, as the embedded workbench folds a turn's tool calls; until then (the
/// agent still works, or a person's message came next) each stays in sight. Reasoning,
/// notices and errors between two calls belong to the run, and are not replies.
#[derive(Debug, PartialEq, Eq)]
enum Item<'a> {
    Message(&'a CommittedMessage),
    Tools {
        /// Tool calls, and the asides committed between them.
        rows: Vec<&'a CommittedMessage>,
        folded: bool,
    },
}

/// What the messages shown and the row above them are made of: it differs between two
/// frames when messages came or went, or a body was read.
fn data_fingerprint(
    cache: &Cache,
    messages: &[&CommittedMessage],
    live: &[&LiveTool],
    older_row: (bool, bool),
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    older_row.hash(&mut hasher);
    // A call that finishes closes and one that is recorded is replaced: both change heights.
    for live in live {
        cards::live_key(live).hash(&mut hasher);
        live.running().hash(&mut hasher);
        if let Some(MessageContent::Body(body)) = &live.tool.input {
            cache
                .bodies
                .get(body.body_id.as_str())
                .map(String::len)
                .hash(&mut hasher);
        }
    }
    for message in messages {
        message.run.as_str().hash(&mut hasher);
        message.index.get().hash(&mut hasher);
        let contents = [Some(&message.content)].into_iter().chain(
            message
                .tool
                .iter()
                .flat_map(|tool| [tool.input.as_ref(), tool.detail.as_ref()]),
        );
        for content in contents.flatten() {
            match content {
                MessageContent::Inline { text } => text.len().hash(&mut hasher),
                MessageContent::Body(body) => cache
                    .bodies
                    .get(body.body_id.as_str())
                    .map(String::len)
                    .hash(&mut hasher),
            }
        }
    }
    hasher.finish()
}

fn message_key(message: &CommittedMessage) -> String {
    format!("{}/{:?}", message.run, message.index)
}

/// Finds, frame by frame, the message the view is anchored on: where the one anchored
/// last frame sits in the content now, and which one to anchor on next.
#[derive(Default)]
struct ViewAnchor {
    /// The content's top, and the top of the view, in screen coordinates.
    origin: f32,
    top: f32,
    previous: Option<(String, f32)>,
    moved_to: Option<f32>,
    first: Option<(String, f32)>,
    spanning: Option<(String, f32)>,
}

impl ViewAnchor {
    /// A message drawn from `top` to `bottom`, or the header of a group of tool outputs.
    /// An open group's header is only looked for, not chosen: what is read is below it.
    fn visit(&mut self, key: String, top: f32, bottom: f32, chosen: bool) {
        let y = top - self.origin;
        if self.previous.as_ref().is_some_and(|(was, _)| *was == key) {
            self.moved_to = Some(y);
        }
        if !chosen {
            return;
        }
        if self.first.is_none() && top >= self.top {
            self.first = Some((key, y));
        } else if top < self.top && bottom > self.top {
            self.spanning = Some((key, y));
        }
    }
}

/// Whether this view draws `message`: a tool call always has a card, and a note without text
/// is nothing to show.
fn drawn(message: &CommittedMessage) -> bool {
    match message.role {
        MessageRole::User | MessageRole::Assistant => true,
        MessageRole::Tool => message.tool.is_some(),
        MessageRole::Reasoning | MessageRole::Notice | MessageRole::Error => {
            !matches!(&message.content, MessageContent::Inline { text } if text.is_empty())
        }
    }
}

fn is_aside(message: &CommittedMessage) -> bool {
    matches!(
        message.role,
        MessageRole::Reasoning | MessageRole::Notice | MessageRole::Error
    )
}

fn items<'a>(history: &[&'a CommittedMessage]) -> Vec<Item<'a>> {
    let mut items: Vec<Item<'a>> = Vec::new();
    // Asides after a run's last call: they join its group when another call follows, and
    // stand on their own after it otherwise.
    let mut trailing: Vec<&'a CommittedMessage> = Vec::new();
    for message in history.iter().copied().filter(|message| drawn(message)) {
        let open_run =
            matches!(items.last(), Some(Item::Tools { rows, .. }) if rows[0].run == message.run);
        match message.role {
            MessageRole::Tool if open_run => {
                if let Some(Item::Tools { rows, .. }) = items.last_mut() {
                    rows.append(&mut trailing);
                    rows.push(message);
                }
            }
            MessageRole::Tool => {
                items.extend(trailing.drain(..).map(Item::Message));
                items.push(Item::Tools {
                    rows: vec![message],
                    folded: false,
                });
            }
            _ if is_aside(message) && open_run => trailing.push(message),
            MessageRole::Assistant => {
                let tools_at = items.len().checked_sub(1).filter(|_| open_run);
                items.extend(trailing.drain(..).map(Item::Message));
                if let Some(Item::Tools { folded, .. }) = tools_at.and_then(|at| items.get_mut(at))
                {
                    *folded = true;
                }
                items.push(Item::Message(message));
            }
            _ => {
                items.extend(trailing.drain(..).map(Item::Message));
                items.push(Item::Message(message));
            }
        }
    }
    items.extend(trailing.drain(..).map(Item::Message));
    items
}

fn message_text<'a>(
    ui: &mut Ui,
    requested_bodies: &mut BTreeSet<String>,
    cache: &'a Cache,
    message: &'a CommittedMessage,
    effects: &mut Vec<Effect>,
) -> Option<&'a str> {
    content_text(ui, requested_bodies, cache, &message.content, effects)
}

/// The text of `content`: inline, or its body once read. Reading a body is asked for at
/// the first frame that needs the text, so only what is drawn is read.
fn content_text<'a>(
    ui: &mut Ui,
    requested_bodies: &mut BTreeSet<String>,
    cache: &'a Cache,
    content: &'a MessageContent,
    effects: &mut Vec<Effect>,
) -> Option<&'a str> {
    match content {
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
    chips: Option<&PullRequestChips<'_>>,
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
            MessageRole::Assistant => {
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
            // A tool call is always drawn as part of its group.
            MessageRole::Tool => {}
            MessageRole::Reasoning | MessageRole::Notice | MessageRole::Error => {
                let mut env = cards::Env {
                    requested_bodies,
                    cache,
                    chips,
                    effects,
                };
                cards::aside(ui, &mut env, message);
            }
        },
    );
}

/// The header of a folded run of tool calls, closed until the person opens it, as the
/// embedded workbench draws its tool-call groups.
fn tool_group(
    ui: &mut Ui,
    env: &mut cards::Env<'_, '_>,
    rows: &[&CommittedMessage],
    view: &mut ViewAnchor,
) {
    // Keyed by the last one: older rows put in front do not change it.
    let last = rows[rows.len() - 1];
    let id = ui.make_persistent_id(("tools", last.run.as_str(), last.index.get()));
    let tools = || rows.iter().filter_map(|row| row.tool.as_ref());
    let count = tools().count();
    let mut title = if count == 1 {
        "1 tool call".to_owned()
    } else {
        format!("{count} tool calls")
    };
    if tools().any(|tool| tool.status == ToolCallStatus::Failed) {
        title.push_str(", some failed");
    }
    let open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    let icon = if open {
        icons::CARET_DOWN
    } else {
        icons::CARET_RIGHT
    };
    let top = ui.cursor().min.y;
    if ghost_icon_button(ui, icon, &title).clicked() {
        ui.data_mut(|data| data.insert_temp(id, !open));
    }
    // The header is not the card of the last call: opening the group must not move it.
    let header = format!("tools {}", message_key(last));
    view.visit(header, top, ui.cursor().min.y, !open);
    if open {
        cards::rows(ui, env, rows, view);
    }
}

#[cfg(test)]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;

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

    pub(super) fn body(id: &str) -> serde_json::Value {
        serde_json::json!({"type": "body", "payload": {"body_id": id, "total_bytes": "2048"}})
    }

    pub(super) fn inline(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "inline", "payload": {"text": text}})
    }

    /// A tool row with the given call fields on top of a successful `run_js` call.
    pub(super) fn tool_row(
        run: &str,
        index: u64,
        call: serde_json::Value,
        content: serde_json::Value,
    ) -> CommittedMessage {
        let mut tool = serde_json::json!({
            "tool_name": "run_js", "call_id": format!("call-{index}"),
            "input_summary": format!("summary {index}"), "status": "succeeded",
        });
        tool.as_object_mut()
            .unwrap()
            .extend(call.as_object().unwrap().clone());
        serde_json::from_value(serde_json::json!({
            "thread": "thr", "run": run, "index": index.to_string(), "role": "tool",
            "content": content, "tool": tool,
        }))
        .unwrap()
    }

    fn call(run: &str, index: u64) -> CommittedMessage {
        tool_row(run, index, serde_json::json!({}), inline("done"))
    }

    fn shapes(history: &[CommittedMessage]) -> Vec<(usize, bool)> {
        let history: Vec<_> = history.iter().collect();
        items(&history)
            .iter()
            .filter_map(|item| match item {
                Item::Tools { rows, folded } => Some((rows.len(), *folded)),
                Item::Message(_) => None,
            })
            .collect()
    }

    // Given: tool calls followed by the same run's reply, by nothing, by the person's
    //        message, and by another run's reply after another run's calls
    // Then: only those the same run replied after fold; each run's calls are a group
    #[test]
    fn tool_calls_fold_once_their_run_replies() {
        let replied = [
            call("r", 0),
            call("r", 1),
            committed("r", 2, "assistant", "done"),
        ];
        assert_eq!(shapes(&replied), [(2, true)]);
        assert_eq!(shapes(&[call("r", 0), call("r", 1)]), [(2, false)]);
        let interrupted = [call("r", 0), committed("r", 1, "user", "stop")];
        assert_eq!(shapes(&interrupted), [(1, false)]);
        let mixed = [
            call("r", 0),
            call("s", 0),
            committed("r", 1, "assistant", "not s's reply"),
        ];
        assert_eq!(shapes(&mixed), [(1, false), (1, false)]);
    }

    // Given: reasoning, a notice and an error around and between a run's tool calls
    // Then: they are drawn, those between two calls inside the group and the others beside
    //       it; none counts as the run's reply, and a real reply afterwards still folds
    #[test]
    fn reasoning_does_not_close_a_tool_group() {
        let aside = |index, role| committed("r", index, role, "aside");
        let history = [
            aside(0, "reasoning"),
            call("r", 1),
            aside(2, "notice"),
            call("r", 3),
            aside(4, "error"),
        ];
        assert_eq!(shapes(&history), [(3, false)]);
        let refs: Vec<_> = history.iter().collect();
        let all = items(&refs);
        assert_eq!(all.len(), 3);
        assert!(matches!(&all[0], Item::Message(m) if m.role == MessageRole::Reasoning));
        assert!(matches!(&all[2], Item::Message(m) if m.role == MessageRole::Error));

        let mut replied = history.to_vec();
        replied.push(committed("r", 5, "assistant", "done"));
        assert_eq!(shapes(&replied), [(3, true)]);
        let refs: Vec<_> = replied.iter().collect();
        let all = items(&refs);
        assert!(
            matches!(&all[2], Item::Message(m) if m.role == MessageRole::Error),
            "the error stays in sight after the fold"
        );
        assert!(
            items(&[&aside(0, "notice")][..]).len() == 1,
            "an aside alone is drawn"
        );
        assert!(items(&[&committed("r", 0, "notice", "")][..]).is_empty());
    }

    pub(super) struct State {
        pub(super) cache: Cache,
        pub(super) requested: BTreeSet<String>,
        pub(super) effects: Vec<Effect>,
    }

    fn harness(cache: Cache) -> egui_kittest::Harness<'static, State> {
        let state = State {
            cache,
            requested: BTreeSet::new(),
            effects: Vec::new(),
        };
        egui_kittest::Harness::new_ui_state(
            |ui, state: &mut State| {
                let thread = ThreadId::new("thr").unwrap();
                transcript(
                    ui,
                    &mut state.requested,
                    TranscriptView {
                        cache: &state.cache,
                        scope: Scope::Thread(&thread),
                        base: None,
                        chips: None,
                    },
                    |_| {},
                    |_, _| {},
                    &mut state.effects,
                );
            },
            state,
        )
    }

    pub(super) fn history(rows: Vec<CommittedMessage>) -> Cache {
        let mut cache = Cache::default();
        cache.history.insert("thr".into(), rows);
        cache
    }

    // Given: a failed call whose input, result and details are all bodies not read yet
    // When: it is drawn closed, then opened
    // Then: closed, its name, summary and failure show and nothing is asked for; opened,
    //       each body is asked for once
    #[test]
    fn tool_header_uses_metadata_without_loading_body() {
        use egui_kittest::kittest::Queryable;
        let failing = tool_row(
            "r",
            0,
            serde_json::json!({
                "status": "failed", "input": body("in"), "detail": body("detail"),
            }),
            body("out"),
        );
        let mut cache = Cache::default();
        cache.history.insert("thr".into(), vec![failing]);
        let mut harness = harness(cache);
        harness.run_steps(3);
        harness.get_by_label_contains("run_js");
        harness.get_by_label("summary 0");
        harness.get_by_label("Failed");
        assert!(harness.state().requested.is_empty());
        assert!(harness.state().effects.is_empty());

        harness.get_by_label_contains("run_js").click();
        harness.run_steps(3);
        let wanted: BTreeSet<_> = ["in", "out", "detail"].map(String::from).into();
        assert_eq!(harness.state().requested, wanted);
    }

    // Given: a closed card with its input and result, and a card that made another call
    // When: the card is pressed on its name, on its padding, and a card inside another is
    //       pressed
    // Then: each press opens or closes the card it was on; one inside another leaves the
    //       outer card as it was, and an opened card stays open over later frames
    #[test]
    fn whole_card_click_toggles_without_stealing_actions() {
        use egui_kittest::kittest::Queryable;
        let single = tool_row(
            "r",
            0,
            serde_json::json!({"input": inline("const x = 1;")}),
            inline("the result"),
        );
        let mut harness = harness(history(vec![single]));
        harness.run_steps(3);
        assert!(harness.query_by_label("Input").is_none());
        harness.get_by_label_contains("run_js").click();
        harness.run_steps(3);
        harness.get_by_label("Input");
        harness.get_by_label("Output");
        harness.run_steps(3);
        harness.get_by_label("Input");

        let status = harness.get_by_label("Succeeded").rect();
        let padding = egui::pos2(status.right() + 3.0, status.center().y);
        harness.hover_at(padding);
        harness.run_steps(2);
        harness.drag_at(padding);
        harness.drop_at(padding);
        harness.run_steps(3);
        assert!(harness.query_by_label("Input").is_none());

        let nested = [
            tool_row(
                "r",
                0,
                serde_json::json!({"parent_call_id": "call-1"}),
                inline("inner"),
            ),
            tool_row(
                "r",
                1,
                serde_json::json!({"input": inline("outer input")}),
                inline("outer"),
            ),
        ];
        let mut harness = self::harness(history(nested.to_vec()));
        harness.run_steps(3);
        assert!(
            harness.query_by_label("summary 0").is_none(),
            "nested until opened"
        );
        harness.get_by_label("summary 1").click();
        harness.run_steps(3);
        harness.get_by_label("summary 0");
        harness.get_by_label("outer input");
        harness.get_by_label("summary 0").click();
        harness.run_steps(3);
        harness.get_by_label("inner");
        harness.get_by_label("outer input");
    }

    // Given: a call made by a call that is not in the rows, and a call inside a caller
    // Then: the first is drawn on its own and the second only inside its caller
    #[test]
    fn orphan_children_remain_visible() {
        use egui_kittest::kittest::Queryable;
        let child = |index, parent: &str| {
            tool_row(
                "r",
                index,
                serde_json::json!({"parent_call_id": parent}),
                inline("x"),
            )
        };
        let rows = vec![child(0, "call-gone"), child(1, "call-2"), call("r", 2)];
        let mut harness = harness(history(rows));
        harness.run_steps(3);
        harness.get_by_label("summary 0");
        harness.get_by_label("summary 2");
        assert!(harness.query_by_label("summary 1").is_none());
    }

    // Given: another run's call with the same call id as a call that is the caller of a child
    // Then: the child is drawn inside the caller of its own run only
    #[test]
    fn a_caller_is_found_in_the_childs_own_run() {
        use egui_kittest::kittest::Queryable;
        let rows = vec![
            tool_row(
                "s",
                0,
                serde_json::json!({"call_id": "shared"}),
                inline("x"),
            ),
            tool_row(
                "r",
                1,
                serde_json::json!({"parent_call_id": "shared"}),
                inline("x"),
            ),
        ];
        let mut harness = harness(history(rows));
        harness.run_steps(3);
        harness.get_by_label("summary 0");
        harness.get_by_label("summary 1");
    }

    // Given: a folded run of calls, and the card of one opened by the person
    // When: the run's group is folded and unfolded
    // Then: the card is still open
    #[test]
    fn an_opened_card_keeps_its_state_when_its_group_folds() {
        use egui_kittest::kittest::Queryable;
        let mut harness = harness(history(vec![
            tool_row(
                "r",
                0,
                serde_json::json!({"input": inline("code")}),
                inline("out"),
            ),
            tool_row(
                "r",
                1,
                serde_json::json!({"input": inline("code2")}),
                inline("out"),
            ),
        ]));
        harness.run_steps(3);
        harness.get_by_label("summary 0").click();
        harness.run_steps(3);
        harness.get_by_label("code");

        harness
            .state_mut()
            .cache
            .history
            .get_mut("thr")
            .unwrap()
            .push(committed("r", 2, "assistant", "Done."));
        harness.run_steps(3);
        harness.get_by_label("2 tool calls");
        assert!(harness.query_by_label("code").is_none());
        harness.get_by_label("2 tool calls").click();
        harness.run_steps(4);
        harness.get_by_label("code");
        assert!(
            harness.query_by_label("code2").is_none(),
            "the other stays closed"
        );
    }

    // Given: a shell result and a change committed while the agent still works
    // When: the agent's reply is committed, then the folded header is clicked
    // Then: both cards are in sight without a header while it works; once it replied
    //       only "2 tool calls" shows; a click opens it and it stays open
    #[test]
    fn tool_calls_stay_in_sight_until_the_reply_and_then_fold() {
        use egui_kittest::kittest::Queryable;
        let shell = tool_row(
            "r",
            1,
            serde_json::json!({"tool_name": "shell", "input_summary": "ls src"}),
            inline("lib.rs"),
        );
        let edit = tool_row(
            "r",
            2,
            serde_json::json!({"tool_name": "edit"}),
            inline("ok"),
        );
        let mut harness = harness(history(vec![
            committed("r", 0, "user", "fix it"),
            shell,
            edit,
        ]));
        harness.run_steps(2);
        harness.get_by_label_contains("shell");
        harness.get_by_label_contains("edit");
        assert!(harness.query_by_label_contains("tool calls").is_none());

        harness
            .state_mut()
            .cache
            .history
            .get_mut("thr")
            .unwrap()
            .push(committed("r", 3, "assistant", "Fixed."));
        harness.run_steps(2);
        harness.get_by_label("2 tool calls");
        assert!(harness.query_by_label_contains("shell").is_none());

        harness.get_by_label("2 tool calls").click();
        harness.run_steps(4);
        harness.get_by_label_contains("shell");
        harness.get_by_label_contains("edit");
    }

    // Given: reasoning, a notice and an error of a run
    // Then: reasoning is one closed line whose text is not asked for until it is pressed,
    //       and the notice and the error show their text
    #[test]
    fn reasoning_is_closed_and_notices_show() {
        use egui_kittest::kittest::Queryable;
        let reasoning: CommittedMessage = serde_json::from_value(serde_json::json!({
            "thread": "thr", "run": "r", "index": "0", "role": "reasoning", "content": body("why"),
        }))
        .unwrap();
        let mut harness = harness(history(vec![
            reasoning,
            committed("r", 1, "notice", "Switched to the fallback model"),
            committed("r", 2, "error", "The request failed"),
        ]));
        harness.run_steps(3);
        harness.get_by_label_contains("Reasoning");
        harness.get_by_label("Switched to the fallback model");
        harness.get_by_label("The request failed");
        assert!(harness.state().requested.is_empty());
        harness.get_by_label_contains("Reasoning").click();
        harness.run_steps(3);
        assert!(harness.state().requested.contains("why"));
    }

    // Given: thr's first messages, scrolled to the first, and an older page of opened tool calls
    // When: the page arrives, and the bodies of its calls arrive in a later frame
    // Then: the message stays where it was on screen after each
    #[test]
    fn earlier_tool_details_preserve_scroll_anchor() {
        use egui_kittest::kittest::Queryable;
        let mut cache = Cache::default();
        let questions = (0..60)
            .map(|index| committed("r", 100 + index, "user", &format!("Question {index}")))
            .collect();
        cache.history.insert("thr".into(), questions);
        cache
            .older
            .insert("thr".into(), Some(PageCursor::new("c1").unwrap()));
        let mut harness = harness(cache);
        let run = RunId::new("r").unwrap();
        harness.ctx.data_mut(|data| {
            for index in 0..20 {
                data.insert_temp(cards::open_id(&run, &format!("call-{index}")), true);
            }
        });
        harness.run_steps(3);
        harness.get_by_label("You: Question 0").scroll_to_me();
        harness.run_steps(3);
        let before = harness.get_by_label("You: Question 0").rect();

        let calls = (0..20)
            .map(|index| {
                tool_row(
                    "r",
                    index,
                    serde_json::json!({"input": body(&format!("in-{index}"))}),
                    body(&format!("out-{index}")),
                )
            })
            .collect();
        harness.state_mut().cache.history_page(
            "thr",
            Page {
                items: calls,
                next_cursor: None,
                snapshot_revision: workbench_protocol::wire::Revision(1),
            },
            true,
        );
        harness.run_steps(4);
        let after_page = harness.get_by_label("You: Question 0").rect();
        assert!(
            (after_page.min.y - before.min.y).abs() < 1.0,
            "the message moved from {before:?} to {after_page:?} with the page"
        );

        let state = harness.state_mut();
        let long = (0..30)
            .map(|l| format!("line {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        for index in 0..20 {
            state
                .cache
                .bodies
                .insert(format!("in-{index}"), long.clone());
            state
                .cache
                .bodies
                .insert(format!("out-{index}"), long.clone());
        }
        harness.run_steps(4);
        let after_bodies = harness.get_by_label("You: Question 0").rect();
        assert!(
            (after_bodies.min.y - before.min.y).abs() < 1.0,
            "the message moved from {before:?} to {after_bodies:?} with the bodies"
        );
    }

    // Given: a card opened while the call that made it was not committed yet
    // When: that caller is committed, and opened
    // Then: the card inside it is still open
    #[test]
    fn a_card_stays_open_when_its_caller_arrives_later() {
        use egui_kittest::kittest::Queryable;
        let child = tool_row(
            "r",
            0,
            serde_json::json!({"parent_call_id": "call-1", "input": inline("child code")}),
            inline("x"),
        );
        let mut harness = harness(history(vec![child]));
        harness.run_steps(3);
        harness.get_by_label("summary 0").click();
        harness.run_steps(3);
        harness.get_by_label("child code");

        harness
            .state_mut()
            .cache
            .history
            .get_mut("thr")
            .unwrap()
            .push(call("r", 1));
        harness.run_steps(3);
        assert!(
            harness.query_by_label("summary 0").is_none(),
            "inside its caller now"
        );
        harness.get_by_label("summary 1").click();
        harness.run_steps(3);
        harness.get_by_label("summary 0");
        harness.get_by_label("child code");
    }

    // Given: a caller with fourteen opened calls whose inputs are bodies not read yet, scrolled to
    //        the seventh
    // When: the bodies of the first three, out of view, arrive
    // Then: the seventh stays where it was on screen
    #[test]
    fn a_body_arriving_inside_a_caller_keeps_the_view_in_place() {
        use egui_kittest::kittest::Queryable;
        let mut rows: Vec<_> = (0..14)
            .map(|index| {
                tool_row(
                    "r",
                    index,
                    serde_json::json!({
                        "parent_call_id": "call-14", "input": body(&format!("in-{index}")),
                    }),
                    inline("x"),
                )
            })
            .collect();
        rows.push(call("r", 14));
        let mut harness = harness(history(rows));
        let run = RunId::new("r").unwrap();
        harness.ctx.data_mut(|data| {
            for index in 0..=14 {
                data.insert_temp(cards::open_id(&run, &format!("call-{index}")), true);
            }
        });
        harness.run_steps(4);
        harness.get_by_label("summary 6").scroll_to_me();
        harness.run_steps(4);
        let before = harness.get_by_label("summary 6").rect();

        let long = (0..30)
            .map(|l| format!("line {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        for index in 0..3 {
            harness
                .state_mut()
                .cache
                .bodies
                .insert(format!("in-{index}"), long.clone());
        }
        harness.run_steps(4);
        let after = harness.get_by_label("summary 6").rect();
        assert!(
            (after.min.y - before.min.y).abs() < 1.0,
            "the call moved from {before:?} to {after:?}"
        );
    }

    // Given: an opened card whose input is text
    // When: the text is dragged over to select it
    // Then: the card stays open
    #[test]
    fn selecting_text_in_a_card_does_not_close_it() {
        use egui_kittest::kittest::Queryable;
        let row = tool_row(
            "r",
            0,
            serde_json::json!({"input": inline("first line\nsecond line")}),
            inline("done"),
        );
        let mut harness = harness(history(vec![row]));
        harness.run_steps(3);
        harness.get_by_label_contains("run_js").click();
        harness.run_steps(3);
        let text = harness.get_by_label("first line\nsecond line").rect();
        harness.hover_at(text.left_top() + egui::vec2(2.0, 2.0));
        harness.run_steps(2);
        harness.drag_at(text.left_top() + egui::vec2(2.0, 2.0));
        harness.run_steps(1);
        harness.hover_at(text.right_bottom() - egui::vec2(2.0, 2.0));
        harness.run_steps(1);
        harness.drop_at(text.right_bottom() - egui::vec2(2.0, 2.0));
        harness.run_steps(3);
        harness.get_by_label("Input");
    }

    // Given: a call whose input and detail are bodies
    // When: those bodies are read
    // Then: what the transcript is made of differs, so a view anchored below them is moved back
    #[test]
    fn a_body_of_a_calls_input_or_detail_changes_the_data_shown() {
        let row = tool_row(
            "r",
            0,
            serde_json::json!({"input": body("in"), "detail": body("detail")}),
            inline("out"),
        );
        let mut cache = Cache::default();
        let fingerprint = |cache: &Cache| data_fingerprint(cache, &[&row], &[], (false, false));
        let unread = fingerprint(&cache);
        cache.bodies.insert("in".into(), "code".into());
        let input_read = fingerprint(&cache);
        cache.bodies.insert("detail".into(), "more".into());
        let all_read = fingerprint(&cache);
        assert!(unread != input_read && input_read != all_read);
    }
}
