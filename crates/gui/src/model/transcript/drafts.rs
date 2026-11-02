//! Tool calls the model is still composing.
//!
//! `ToolCallDelta` fragments are display-only: a draft is a `Tool` entry in
//! `ToolStatus::Drafting` that `ToolStarted` converts in place, so the card keeps
//! its position. Anything the accepted response never turned into a call is
//! discarded rather than left as a card that can never finish.
//!
//! Drafts are tracked by (run, call index) with a stable entry ID, so they are
//! found, confirmed and discarded however far the transcript has since grown.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};
use secret_guard::SecretRedactor;

use super::{ToolStatus, TranscriptEntry, TranscriptModel, draft_text};

/// Raw argument text kept per draft so a credential split across fragments is
/// still recognised. This is the only unredacted copy and never leaves the
/// model; it is strictly bounded, so a draft holds at most this much raw text.
pub(super) const RAW_WINDOW_BYTES: usize = 64 * 1024;
/// Redacted tail stored in the entry and handed to the renderer, once for the
/// argument JSON and once for the decoded text.
pub(super) const PREVIEW_BYTES: usize = 16 * 1024;
/// A window whose start was cut shows nothing within the longest credential
/// length of that start (see `shown_tail`). That distance, at least the
/// designed `MAX_CREDENTIAL_BYTES`, must leave room for a whole preview.
const _: () = assert!(RAW_WINDOW_BYTES - PREVIEW_BYTES >= secret_guard::MAX_CREDENTIAL_BYTES);
/// Tools whose arguments the transcript never shows as a card.
const HIDDEN_DRAFTS: &[&str] = &["finish"];

/// One streamed fragment of a call that is still being composed.
pub(super) struct ToolCallFragment<'a> {
    pub run_id: Option<&'a str>,
    pub index: usize,
    pub call_id: Option<&'a str>,
    pub tool_name: Option<&'a str>,
    pub arguments_delta: &'a str,
}

type DraftKey = (Option<String>, usize);

#[derive(Debug, Clone)]
struct Draft {
    /// Allocated once; removals never change another draft's identity.
    entry: usize,
    /// Unredacted tail of the arguments; never leaves the model.
    raw: String,
    /// The start of the arguments was dropped to stay within the window.
    trimmed: bool,
}

/// Where each live draft sits in the transcript.
#[derive(Debug, Clone, Default)]
pub(super) struct DraftIndex {
    drafts: BTreeMap<DraftKey, Draft>,
}

impl DraftIndex {
    pub(super) fn entry_removed(&mut self, id: usize) {
        self.drafts.retain(|_, draft| draft.entry != id);
    }

    fn keys_of(&self, run_id: Option<&str>) -> Vec<DraftKey> {
        self.drafts
            .keys()
            .filter(|(run, _)| run.as_deref() == run_id)
            .cloned()
            .collect()
    }
}

fn redactor() -> &'static SecretRedactor {
    static REDACTOR: OnceLock<SecretRedactor> = OnceLock::new();
    REDACTOR.get_or_init(SecretRedactor::from_env)
}

fn placeholder_call_id(index: usize) -> String {
    format!("draft:{index}")
}

/// The newest `limit` bytes of `text`, starting on a character boundary.
fn tail(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut cut = text.len() - limit;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    &text[cut..]
}

/// A copy of `value` that is safe to show: credentials are masked. The runtime
/// keeps and executes the original, which this never touches.
#[must_use]
pub fn redacted_for_display(value: &serde_json::Value) -> serde_json::Value {
    let mut copy = value.clone();
    redactor().redact_json(&mut copy);
    copy
}

/// `text` with credentials masked, for showing a value the runtime owns.
#[must_use]
pub fn redacted_text_for_display(text: &str) -> String {
    redactor().redact(text).text
}

/// What a draft card shows, all redacted.
pub(super) struct DraftView {
    pub arguments: String,
    pub text: String,
    pub omitted: bool,
}

/// The newest `PREVIEW_BYTES` of `text` redacted, and whether older text is not
/// shown.
///
/// `head_cut` says the start of `text` was cut from a longer stream, so a
/// credential may have lost its recognisable opening there. Such a fragment can
/// only lie within the longest credential length of the start, wherever
/// redaction later shortens the text, so everything up to that distance is left
/// out: the whole text is redacted first (a credential straddling the distance
/// is masked whole) and only what follows it is shown.
fn shown_tail(redactor: &SecretRedactor, text: &str, head_cut: bool) -> (String, bool) {
    let skip = if head_cut {
        redactor.max_credential_len()
    } else {
        0
    };
    let redacted = redactor.redact_streaming_after(text, skip).text;
    let shown = tail(&redacted, PREVIEW_BYTES);
    (shown.to_owned(), head_cut || shown.len() < redacted.len())
}

/// The view of a draft whose retained raw argument text is `raw`.
///
/// Both views are redacted after their JSON escapes are decoded: a credential
/// written with escapes (`ghp_…\u0042…`) is only recognisable once decoded,
/// and the decoded text is what is shown, so no escaped form can slip past.
pub(super) fn view(
    redactor: &SecretRedactor,
    tool_name: &str,
    raw: &str,
    trimmed: bool,
) -> DraftView {
    let (arguments, arguments_omitted) =
        shown_tail(redactor, &draft_text::unescape_all(raw), trimmed);
    let decoded = draft_text::text_field(tool_name)
        .and_then(|field| draft_text::field_text(raw, field, trimmed));
    match decoded {
        Some((decoded, from_start)) => {
            let (text, omitted) = shown_tail(redactor, &decoded, !from_start);
            DraftView {
                arguments,
                text,
                omitted,
            }
        }
        None => DraftView {
            arguments,
            text: String::new(),
            omitted: arguments_omitted,
        },
    }
}

/// Appends `delta`, then drops the oldest text beyond the window.
fn push_raw(draft: &mut Draft, delta: &str) {
    draft.raw.push_str(delta);
    if draft.raw.len() <= RAW_WINDOW_BYTES {
        return;
    }
    let mut cut = draft.raw.len() - RAW_WINDOW_BYTES;
    while !draft.raw.is_char_boundary(cut) {
        cut += 1;
    }
    draft.raw.drain(..cut);
    draft.trimmed = true;
}

impl TranscriptModel {
    pub(super) fn apply_tool_call_delta(&mut self, fragment: &ToolCallFragment<'_>) {
        let key: DraftKey = (fragment.run_id.map(str::to_owned), fragment.index);
        if let Some(position) = self.draft_position(&key) {
            let replaced = matches!(
                (&self.entries[position], fragment.call_id),
                (TranscriptEntry::Tool { call_id, .. }, Some(id))
                    if call_id != id && *call_id != placeholder_call_id(fragment.index)
            );
            if !replaced {
                self.extend_draft(&key, position, fragment);
                return;
            }
            // A different call took over this slot, e.g. after a provider fallback.
            self.remove_entry(position);
        }
        // Only the first fragment names the tool; later ones without a draft
        // belong to a call that is intentionally not shown.
        let Some(name) = fragment
            .tool_name
            .filter(|name| !HIDDEN_DRAFTS.contains(name))
        else {
            return;
        };
        let mut draft = Draft {
            entry: self.next_entry_id,
            raw: String::new(),
            trimmed: false,
        };
        push_raw(&mut draft, fragment.arguments_delta);
        let shown = view(redactor(), name, &draft.raw, draft.trimmed);
        let entry = draft.entry;
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
                arguments: shown.arguments,
                text: shown.text,
                omitted: shown.omitted,
                sealed: false,
            },
            reviews: Vec::new(),
            parent_call_id: None,
        });
        // A zero-capacity transcript keeps no entries and so no drafts.
        if self.next_entry_id > entry {
            self.drafts.drafts.insert(key, draft);
        }
    }

    fn extend_draft(&mut self, key: &DraftKey, position: usize, fragment: &ToolCallFragment<'_>) {
        let Some(draft) = self.drafts.drafts.get_mut(key) else {
            return;
        };
        push_raw(draft, fragment.arguments_delta);
        let name = match (&self.entries[position], fragment.tool_name) {
            (_, Some(name)) => name,
            (TranscriptEntry::Tool { tool_name, .. }, None) => tool_name,
            _ => return,
        };
        let shown = view(redactor(), name, &draft.raw, draft.trimmed);
        self.entry_meta[position].revision += 1;
        if let TranscriptEntry::Tool {
            tool_name,
            call_id,
            status:
                ToolStatus::Drafting {
                    arguments,
                    text,
                    omitted,
                    sealed,
                    ..
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
            *arguments = shown.arguments;
            *text = shown.text;
            *omitted = shown.omitted;
            *sealed = false;
        }
    }

    /// The response is final: its calls start next. Drafts sealed by an earlier
    /// response never started, so they are dropped first.
    pub(super) fn response_completed(&mut self, run_id: &str) {
        self.discard_matching(Some(run_id), |sealed| sealed);
        for key in self.drafts.keys_of(Some(run_id)) {
            if let Some(position) = self.draft_position(&key) {
                self.entry_meta[position].revision += 1;
                if let TranscriptEntry::Tool {
                    status: ToolStatus::Drafting { sealed, .. },
                    ..
                } = &mut self.entries[position]
                {
                    *sealed = true;
                }
            }
        }
    }

    /// A new response began, so drafts sealed by the previous one are stale.
    pub(super) fn discard_sealed_drafts(&mut self, run_id: Option<&str>) {
        self.discard_matching(run_id, |sealed| sealed);
    }

    /// Drops every draft of `run_id`: its stream restarted or it ended.
    pub(super) fn discard_drafts(&mut self, run_id: Option<&str>) {
        self.discard_matching(run_id, |_| true);
    }

    pub(super) fn finish_drafts(&mut self, event: &Event) {
        let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            &event.kind
        else {
            return;
        };
        match to {
            AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error => {
                self.discard_drafts(Some(run_id));
            }
            // A call that waits (a child, a reply, an answer) leaves its card running
            // while later calls of the same batch still have to start. Only a run
            // that waits with no call in flight is idle, and nothing can start the
            // calls its finished response left sealed.
            AgentRunPhase::Waiting => {
                if !self.running_tool_runs().any(|run| run == run_id) {
                    self.discard_sealed_drafts(Some(run_id));
                }
            }
            AgentRunPhase::Pending | AgentRunPhase::Running => {}
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
        let keys = self.drafts.keys_of(run_id);
        let by_id = keys.iter().find(|key| {
            self.draft_position(key).is_some_and(|position| {
                matches!(&self.entries[position],
                    TranscriptEntry::Tool { call_id: id, .. } if id == call_id)
            })
        });
        // A provider that never named the call by the ID the runtime reports is
        // matched by tool name, lowest index first.
        let by_name = || {
            keys.iter().find(|key| {
                self.draft_position(key).is_some_and(|position| {
                    matches!(&self.entries[position],
                        TranscriptEntry::Tool { call_id: id, tool_name: name, .. }
                            if name == tool_name && *id == placeholder_call_id(key.1))
                })
            })
        };
        let Some(key) = by_id.or_else(by_name).cloned() else {
            return false;
        };
        let Some(position) = self.draft_position(&key) else {
            return false;
        };
        self.drafts.drafts.remove(&key);
        let pending = self.take_pending_reviews(run_id, call_id);
        self.entry_meta[position].revision += 1;
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

    /// Current position of a draft's entry, or `None` once it stopped being a
    /// draft (an approval request reuses the card) or left the transcript.
    fn draft_position(&self, key: &DraftKey) -> Option<usize> {
        let draft = self.drafts.drafts.get(key)?;
        let position = self.entry_index(draft.entry)?;
        matches!(
            self.entries.get(position),
            Some(TranscriptEntry::Tool {
                status: ToolStatus::Drafting { index, .. },
                ..
            }) if *index == key.1
        )
        .then_some(position)
    }

    fn discard_matching(&mut self, run_id: Option<&str>, discard: impl Fn(bool) -> bool) {
        let mut doomed = Vec::new();
        for key in self.drafts.keys_of(run_id) {
            match self.draft_position(&key) {
                Some(position) => {
                    let sealed = matches!(
                        &self.entries[position],
                        TranscriptEntry::Tool {
                            status: ToolStatus::Drafting { sealed: true, .. },
                            ..
                        }
                    );
                    if discard(sealed) {
                        doomed.push(position);
                    }
                }
                None => {
                    self.drafts.drafts.remove(&key);
                }
            }
        }
        doomed.sort_unstable_by(|a, b| b.cmp(a));
        for position in doomed {
            self.remove_entry(position);
        }
    }
}
