//! Tool-call groups: a display projection of the transcript entries.
//!
//! Once an assistant turn has produced user-facing text, the tool calls and
//! reasoning that led up to it fold under one header (closed by default). The
//! projection only decides what is shown: the entries, and so what the
//! provider is sent, are untouched. It is a pure function of the current
//! entries; the one piece of state is the user's own open toggles, kept apart
//! from it (see [`OpenGroups`]).
//!
//! A *segment* is a run of consecutive [`Role::Member`] entries (tool calls and
//! reasoning) of one run. It becomes a group when it holds at least one tool
//! card, nothing in it is still running or waiting on the user, and the entry
//! right after it is a [`Role::Trigger`] (a non-empty message of the same run).
//! Every other entry is a [`Role::Boundary`]: it stays visible, splits
//! segments, and is never folded into a group.

use std::collections::HashSet;

use crate::model::transcript::{ToolStatus, TranscriptEntry};
use crate::panes::transcript_tool::RunJsNesting;

/// What an entry does to the grouping. Every [`TranscriptEntry`] variant is
/// named here, so adding one forces a decision about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Folds into the group (tool calls, reasoning).
    Member,
    /// A non-empty assistant message: ends a segment and closes its group.
    Trigger,
    /// Ends a segment without closing it and stays visible.
    Boundary,
}

fn role(entry: &TranscriptEntry) -> Role {
    match entry {
        TranscriptEntry::Tool { .. } | TranscriptEntry::Reasoning { .. } => Role::Member,
        TranscriptEntry::Message { text, .. } if !text.is_empty() => Role::Trigger,
        TranscriptEntry::Notice { .. }
        | TranscriptEntry::Message { .. }
        | TranscriptEntry::Error { .. }
        | TranscriptEntry::UserMessage { .. }
        | TranscriptEntry::Branch { .. }
        | TranscriptEntry::Diagnostic { .. }
        | TranscriptEntry::SandboxReview { .. }
        | TranscriptEntry::Compaction { .. }
        | TranscriptEntry::CompactionProgress { .. }
        | TranscriptEntry::AgentMessage { .. }
        | TranscriptEntry::Artifacts { .. }
        | TranscriptEntry::TurnEnd { .. }
        | TranscriptEntry::HarnessNotice { .. }
        | TranscriptEntry::Unreadable { .. } => Role::Boundary,
    }
}

fn run_of(entry: &TranscriptEntry) -> Option<&str> {
    match entry {
        TranscriptEntry::Tool { run_id, .. }
        | TranscriptEntry::Reasoning { run_id, .. }
        | TranscriptEntry::Message { run_id, .. } => run_id.as_deref(),
        _ => None,
    }
}

/// A call still being made or waiting for the user keeps its segment open.
fn is_active(status: &ToolStatus) -> bool {
    match status {
        ToolStatus::Running
        | ToolStatus::AwaitingApproval
        | ToolStatus::Approved
        | ToolStatus::Drafting { .. } => true,
        ToolStatus::Succeeded
        | ToolStatus::Failed
        | ToolStatus::Denied { .. }
        | ToolStatus::Interrupted => false,
    }
}

/// One group: the entries `start..end` of the visible slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToolGroup {
    pub start: usize,
    pub end: usize,
    /// Index of the first tool card, whose stable entry ID names the group.
    pub first_tool: usize,
    /// Top-level tool calls in the group (run_js children count under their
    /// parent).
    pub calls: usize,
    /// Whether any call in the group failed.
    pub failed: bool,
}

/// The groups of `entries`, in order and not overlapping.
pub(super) fn project(entries: &[TranscriptEntry], nesting: &RunJsNesting) -> Vec<ToolGroup> {
    let mut groups = Vec::new();
    let mut segment: Option<Segment> = None;
    for (index, entry) in entries.iter().enumerate() {
        match role(entry) {
            Role::Member => {
                if segment
                    .as_ref()
                    .is_some_and(|segment| segment.run != run_of(entry))
                {
                    segment = None;
                }
                segment
                    .get_or_insert_with(|| Segment::new(index, run_of(entry)))
                    .add(index, entry, nesting);
            }
            Role::Trigger => {
                if let Some(segment) = segment.take()
                    && segment.run == run_of(entry)
                    && let Some(group) = segment.close(index)
                {
                    groups.push(group);
                }
            }
            Role::Boundary => segment = None,
        }
    }
    groups
}

struct Segment<'a> {
    start: usize,
    run: Option<&'a str>,
    first_tool: Option<usize>,
    calls: usize,
    failed: bool,
    active: bool,
}

impl<'a> Segment<'a> {
    fn new(start: usize, run: Option<&'a str>) -> Self {
        Self {
            start,
            run,
            first_tool: None,
            calls: 0,
            failed: false,
            active: false,
        }
    }

    fn add(&mut self, index: usize, entry: &TranscriptEntry, nesting: &RunJsNesting) {
        let TranscriptEntry::Tool {
            status, is_error, ..
        } = entry
        else {
            return;
        };
        self.active |= is_active(status);
        self.failed |= *is_error || matches!(status, ToolStatus::Failed);
        // A child shown inside its parent's card is not a card of its own.
        if !nesting.is_nested(index) {
            self.first_tool.get_or_insert(index);
            self.calls += 1;
        }
    }

    fn close(self, end: usize) -> Option<ToolGroup> {
        let first_tool = self.first_tool?;
        (!self.active).then_some(ToolGroup {
            start: self.start,
            end,
            first_tool,
            calls: self.calls,
            failed: self.failed,
        })
    }
}

/// The header line of a group: a toggle naming how many calls it holds.
/// Returns whether the reader clicked it.
pub(super) fn header(ui: &mut egui::Ui, group: &ToolGroup, open: bool) -> bool {
    let noun = if group.calls == 1 {
        "tool call"
    } else {
        "tool calls"
    };
    let label = if group.failed {
        format!("{} {noun}, some failed", group.calls)
    } else {
        format!("{} {noun}", group.calls)
    };
    let icon = if open {
        crate::theme::icons::CARET_DOWN
    } else {
        crate::theme::icons::CARET_RIGHT
    };
    crate::theme::widgets::ghost_icon_button(ui, icon, &label).clicked()
}

/// Groups the reader opened, by the stable entry ID of each group's first tool
/// card. Closed is the default, so closing removes the ID. Nothing is
/// remembered about groups the reader never touched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct OpenGroups {
    open: HashSet<usize>,
}

impl OpenGroups {
    pub fn is_open(&self, key: usize) -> bool {
        self.open.contains(&key)
    }

    pub fn toggle(&mut self, key: usize) {
        if !self.open.remove(&key) {
            self.open.insert(key);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// Forgets groups whose first tool left the view window or the transcript,
    /// so the set cannot outgrow what is shown. A group that merely stops being
    /// a group for a moment (a call restarted) keeps its toggle.
    pub fn retain(&mut self, mut present: impl FnMut(usize) -> bool) -> bool {
        let before = self.open.len();
        self.open.retain(|key| present(*key));
        self.open.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::BodyText;

    fn tool(run: &str, call: &str, status: ToolStatus) -> TranscriptEntry {
        TranscriptEntry::Tool {
            run_id: Some(run.to_owned()),
            tool_name: "read".to_owned(),
            call_id: call.to_owned(),
            input: None,
            output: None,
            detail: None,
            is_error: false,
            status,
            reviews: Vec::new(),
            parent_call_id: None,
        }
    }

    fn child(run: &str, call: &str, parent: &str) -> TranscriptEntry {
        TranscriptEntry::Tool {
            run_id: Some(run.to_owned()),
            tool_name: "read".to_owned(),
            call_id: call.to_owned(),
            input: None,
            output: None,
            detail: None,
            is_error: false,
            status: ToolStatus::Succeeded,
            reviews: Vec::new(),
            parent_call_id: Some(parent.to_owned()),
        }
    }

    fn run_js(run: &str, call: &str) -> TranscriptEntry {
        let TranscriptEntry::Tool {
            run_id,
            call_id,
            status,
            ..
        } = tool(run, call, ToolStatus::Succeeded)
        else {
            unreachable!()
        };
        TranscriptEntry::Tool {
            run_id,
            tool_name: "run_js".to_owned(),
            call_id,
            input: None,
            output: None,
            detail: None,
            is_error: false,
            status,
            reviews: Vec::new(),
            parent_call_id: None,
        }
    }

    fn reasoning(run: &str) -> TranscriptEntry {
        TranscriptEntry::Reasoning {
            text: BodyText::from("thinking"),
            run_id: Some(run.to_owned()),
        }
    }

    fn message(run: &str, text: &str) -> TranscriptEntry {
        TranscriptEntry::Message {
            text: BodyText::from(text),
            run_id: Some(run.to_owned()),
        }
    }

    fn turn_end(run: &str) -> TranscriptEntry {
        TranscriptEntry::TurnEnd {
            run_id: run.to_owned(),
            context_len: 0,
        }
    }

    fn spans(entries: &[TranscriptEntry]) -> Vec<(usize, usize)> {
        project(entries, &RunJsNesting::new(entries))
            .iter()
            .map(|group| (group.start, group.end))
            .collect()
    }

    const DONE: ToolStatus = ToolStatus::Succeeded;

    #[test]
    fn tools_and_reasoning_before_text_form_one_group() {
        let entries = [
            tool("r", "a", DONE),
            reasoning("r"),
            tool("r", "b", DONE),
            message("r", "answer"),
        ];
        let groups = project(&entries, &RunJsNesting::new(&entries));
        assert_eq!(
            groups,
            [ToolGroup {
                start: 0,
                end: 3,
                first_tool: 0,
                calls: 2,
                failed: false
            }]
        );
        // The group opens with the first member even when that is reasoning.
        let entries = [reasoning("r"), tool("r", "a", DONE), message("r", "x")];
        let groups = project(&entries, &RunJsNesting::new(&entries));
        assert_eq!((groups[0].start, groups[0].first_tool), (0, 1));
    }

    #[test]
    fn nothing_collapses_without_a_non_empty_message_next() {
        for entries in [
            // No message yet.
            vec![tool("r", "a", DONE)],
            // Reasoning only.
            vec![reasoning("r"), message("r", "answer")],
            // An empty message does not trigger.
            vec![tool("r", "a", DONE), message("r", "")],
            // A turn end between the tools and the text splits them.
            vec![tool("r", "a", DONE), turn_end("r"), message("r", "answer")],
            // Text of another run closes nothing of this one.
            vec![tool("r", "a", DONE), message("other", "answer")],
        ] {
            assert_eq!(spans(&entries), [], "{entries:?}");
        }
    }

    #[test]
    fn every_other_entry_is_a_boundary_that_stays_outside() {
        let boundaries = [
            TranscriptEntry::Notice { text: "n".into() },
            TranscriptEntry::Error { text: "e".into() },
            TranscriptEntry::UserMessage {
                text: "u".into(),
                skills: Vec::new(),
            },
            TranscriptEntry::Diagnostic {
                severity: event_bus::DiagnosticSeverity::Info,
                text: "d".into(),
                hover: String::new(),
            },
            TranscriptEntry::SandboxReview {
                text: "s".into(),
                run_id: Some("r".into()),
                call_id: None,
            },
            TranscriptEntry::AgentMessage {
                direction: crate::model::transcript::MessageDirection::Incoming,
                peer_run_id: "p".into(),
                kind: event_bus::AgentMessageKind::Send,
                content: "c".into(),
            },
            turn_end("r"),
            message("r", ""),
        ];
        for boundary in boundaries {
            // [tool, boundary, tool, text]: only the part after the boundary
            // is folded, and a segment before one is not folded by later text.
            let entries = [
                tool("r", "a", DONE),
                boundary.clone(),
                tool("r", "b", DONE),
                message("r", "answer"),
            ];
            assert_eq!(spans(&entries), [(2, 3)], "{boundary:?}");
        }
    }

    #[test]
    fn segments_without_a_tool_card_or_with_a_live_call_stay_open() {
        assert_eq!(spans(&[reasoning("r"), message("r", "x")]), []);
        for status in [
            ToolStatus::Running,
            ToolStatus::AwaitingApproval,
            ToolStatus::Approved,
        ] {
            let entries = [
                tool("r", "a", DONE),
                tool("r", "b", status.clone()),
                message("r", "x"),
            ];
            assert_eq!(spans(&entries), [], "{status:?}");
        }
        for status in [
            ToolStatus::Failed,
            ToolStatus::Interrupted,
            ToolStatus::Denied {
                reason: "no".into(),
            },
        ] {
            let entries = [tool("r", "a", status), message("r", "x")];
            assert_eq!(spans(&entries), [(0, 1)]);
        }
    }

    #[test]
    fn runs_form_separate_segments_and_a_failure_is_reported() {
        let mut failed = tool("b", "a", ToolStatus::Failed);
        if let TranscriptEntry::Tool { is_error, .. } = &mut failed {
            *is_error = true;
        }
        // The same call ID in two runs is two calls.
        let entries = [
            tool("a", "a", DONE),
            failed,
            message("b", "x"),
            message("a", "y"),
        ];
        let groups = project(&entries, &RunJsNesting::new(&entries));
        assert_eq!(groups.len(), 1);
        assert_eq!((groups[0].start, groups[0].end), (1, 2));
        assert!(groups[0].failed);
    }

    #[test]
    fn run_js_children_belong_to_their_parent_and_orphans_stay_cards() {
        let entries = [
            run_js("r", "p"),
            child("r", "c1", "p"),
            child("r", "c2", "p"),
            message("r", "x"),
        ];
        let groups = project(&entries, &RunJsNesting::new(&entries));
        assert_eq!((groups[0].calls, groups[0].first_tool), (1, 0));
        // A child whose parent is not shown is a card of its own.
        let entries = [child("r", "c", "missing"), message("r", "x")];
        let groups = project(&entries, &RunJsNesting::new(&entries));
        assert_eq!(groups[0].calls, 1);
        // A child alone (its parent is across a boundary) is no card, so it
        // forms no group.
        let entries = [
            run_js("r", "p"),
            TranscriptEntry::Notice { text: "n".into() },
            child("r", "c", "p"),
            message("r", "x"),
        ];
        assert_eq!(spans(&entries), []);
    }

    #[test]
    fn open_toggles_flip_and_are_forgotten_with_their_tool() {
        let mut open = OpenGroups::default();
        assert!(!open.is_open(4));
        open.toggle(4);
        open.toggle(7);
        assert!(open.is_open(4) && open.is_open(7));
        open.toggle(4);
        assert!(!open.is_open(4));
        assert!(open.retain(|key| key != 7));
        assert!(open.is_empty());
        assert!(!open.retain(|_| true));
    }
}
