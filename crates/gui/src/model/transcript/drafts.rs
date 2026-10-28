//! Tool calls the model is still composing.
//!
//! `ToolCallDelta` fragments are display-only: a draft is a `Tool` entry in
//! `ToolStatus::Drafting` that `ToolStarted` converts in place, so the card keeps
//! its position. Anything the accepted response never turned into a call is
//! discarded rather than left as a card that can never finish.

use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};

use super::{ToolStatus, TranscriptEntry, TranscriptModel};

/// Drafts are the newest entries; bounding the search keeps each fragment cheap.
const DRAFT_SEARCH_WINDOW: usize = 256;
/// Large arguments (file contents) only ever show their tail, so the retained
/// text is trimmed from the front beyond this size.
const MAX_DRAFT_BYTES: usize = 128 * 1024;
const TRIM_TO_BYTES: usize = 64 * 1024;
/// Tools whose arguments the transcript never shows as a card.
const HIDDEN_DRAFTS: &[&str] = &["finish"];

/// One streamed fragment of a call that is still being composed.
pub(super) struct ToolCallFragment<'a> {
    pub run_id: Option<&'a str>,
    pub index: usize,
    pub call_id: Option<&'a str>,
    pub tool_name: Option<&'a str>,
    pub arguments_delta: &'a str,
    pub begins_attempt: bool,
}

fn placeholder_call_id(index: usize) -> String {
    format!("draft:{index}")
}

impl TranscriptModel {
    pub(super) fn apply_tool_call_delta(&mut self, fragment: &ToolCallFragment<'_>) {
        if fragment.begins_attempt {
            self.discard_drafts(fragment.run_id);
        }
        if let Some(position) = self.draft_position(fragment.run_id, fragment.index) {
            if let TranscriptEntry::Tool {
                tool_name,
                call_id,
                status:
                    ToolStatus::Drafting {
                        arguments, sealed, ..
                    },
                ..
            } = &mut self.entries[position]
            {
                if let Some(name) = fragment.tool_name {
                    name.clone_into(tool_name);
                }
                if let Some(id) = fragment.call_id {
                    id.clone_into(call_id);
                }
                arguments.push_str(fragment.arguments_delta);
                trim_front(arguments);
                *sealed = false;
            }
            return;
        }
        // Only the first fragment names the tool; later ones without a draft
        // belong to a call that is intentionally not shown.
        let Some(name) = fragment
            .tool_name
            .filter(|name| !HIDDEN_DRAFTS.contains(name))
        else {
            return;
        };
        let mut arguments = fragment.arguments_delta.to_owned();
        trim_front(&mut arguments);
        self.push(TranscriptEntry::Tool {
            run_id: fragment.run_id.map(str::to_owned),
            tool_name: name.to_owned(),
            call_id: fragment
                .call_id
                .map_or_else(|| placeholder_call_id(fragment.index), str::to_owned),
            input: None,
            output: None,
            detail: None,
            is_error: false,
            status: ToolStatus::Drafting {
                index: fragment.index,
                arguments,
                sealed: false,
            },
            reviews: Vec::new(),
        });
    }

    /// The response is final: its calls start next, and any draft that does not
    /// become one is dropped when the following response begins.
    pub(super) fn seal_drafts(&mut self, run_id: &str) {
        for entry in self.recent_entries_mut() {
            if let TranscriptEntry::Tool {
                run_id: Some(run),
                status: ToolStatus::Drafting { sealed, .. },
                ..
            } = entry
                && run == run_id
            {
                *sealed = true;
            }
        }
    }

    /// A new response means sealed drafts were never started as calls.
    pub(super) fn discard_sealed_drafts(&mut self, run_id: Option<&str>) {
        self.retain_drafts(run_id, |sealed| !sealed);
    }

    /// Drops every draft of `run_id`, e.g. when its stream restarts or it ends.
    pub(super) fn discard_drafts(&mut self, run_id: Option<&str>) {
        self.retain_drafts(run_id, |_| false);
    }

    pub(super) fn finish_drafts(&mut self, event: &Event) {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error,
            ..
        }) = &event.kind
        {
            self.discard_drafts(Some(run_id));
        }
    }

    /// Turns the draft that `ToolStarted` confirms into a running call, keeping
    /// its place in the transcript. Returns whether a draft was claimed.
    pub(super) fn claim_draft(
        &mut self,
        run_id: Option<&str>,
        tool_name: &str,
        call_id: &str,
        input: Option<&serde_json::Value>,
    ) -> bool {
        let matches_run = |run: &Option<String>| run.as_deref() == run_id;
        let by_id = self.recent_position(|entry| {
            matches!(entry, TranscriptEntry::Tool {
                run_id: run, call_id: id, status: ToolStatus::Drafting { .. }, ..
            } if matches_run(run) && id == call_id)
        });
        // A provider that never named the call by the ID the runtime reports is
        // matched by tool name, oldest draft first.
        let position = by_id.or_else(|| {
            let start = self.entries.len().saturating_sub(DRAFT_SEARCH_WINDOW);
            self.entries[start..]
                .iter()
                .position(|entry| {
                    matches!(entry, TranscriptEntry::Tool {
                        run_id: run, call_id: id, tool_name: name,
                        status: ToolStatus::Drafting { index, .. }, ..
                    } if matches_run(run) && name == tool_name && *id == placeholder_call_id(*index))
                })
                .map(|offset| start + offset)
        });
        let Some(position) = position else {
            return false;
        };
        let pending = self.take_pending_reviews(run_id, call_id);
        if let TranscriptEntry::Tool {
            tool_name: name,
            call_id: id,
            input: current_input,
            status,
            reviews,
            ..
        } = &mut self.entries[position]
        {
            tool_name.clone_into(name);
            call_id.clone_into(id);
            *current_input = input.cloned();
            *status = ToolStatus::Running;
            reviews.extend(pending);
        }
        true
    }

    fn draft_position(&self, run_id: Option<&str>, index: usize) -> Option<usize> {
        self.recent_position(|entry| {
            matches!(entry, TranscriptEntry::Tool {
                run_id: run,
                status: ToolStatus::Drafting { index: draft, .. },
                ..
            } if run.as_deref() == run_id && *draft == index)
        })
    }

    fn recent_position(&self, matches: impl Fn(&TranscriptEntry) -> bool) -> Option<usize> {
        let start = self.entries.len().saturating_sub(DRAFT_SEARCH_WINDOW);
        self.entries[start..]
            .iter()
            .rposition(matches)
            .map(|offset| start + offset)
    }

    fn recent_entries_mut(&mut self) -> impl Iterator<Item = &mut TranscriptEntry> {
        let start = self.entries.len().saturating_sub(DRAFT_SEARCH_WINDOW);
        self.entries[start..].iter_mut()
    }

    fn retain_drafts(&mut self, run_id: Option<&str>, keep: impl Fn(bool) -> bool) {
        let start = self.entries.len().saturating_sub(DRAFT_SEARCH_WINDOW);
        let doomed: Vec<usize> = self.entries[start..]
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                matches!(entry, TranscriptEntry::Tool {
                    run_id: run,
                    status: ToolStatus::Drafting { sealed, .. },
                    ..
                } if run.as_deref() == run_id && !keep(*sealed))
            })
            .map(|(offset, _)| start + offset)
            .collect();
        for index in doomed.into_iter().rev() {
            self.remove_entry(index);
        }
    }
}

fn trim_front(arguments: &mut String) {
    if arguments.len() <= MAX_DRAFT_BYTES {
        return;
    }
    let mut cut = arguments.len() - TRIM_TO_BYTES;
    while !arguments.is_char_boundary(cut) {
        cut += 1;
    }
    arguments.drain(..cut);
}
