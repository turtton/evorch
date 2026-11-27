//! Pure transcript semantics shared by live scanning and the events-only index.
//! No protocol dependency and no storage reads; callers own pending response state.
pub mod background;
#[cfg(all(test, feature = "test-support"))]
mod background_tests;
pub mod build;
pub(crate) mod index;
#[cfg(test)]
mod index_tests;
pub(crate) mod meter;
pub mod read;
#[cfg(test)]
mod read_tests;
pub mod rows;
#[cfg(test)]
mod structured_tests;
#[cfg(test)]
mod tests;

use event_bus::{EventKind, LifecycleEvent, MessageEvent};
pub use rows::{Committed, ToolInput, ToolRow};

pub type PendingParts = (usize, usize, Vec<(String, bool)>);
/// Domain-separated digest of ordered normalized part identities. Fixed size
/// keeps the next append independent of the preceding message's part count.
pub type Key = [u8; 32];

/// Validate one response locally; a new index zero restarts that response.
pub fn part_progress(
    previous: Option<(usize, usize)>,
    index: usize,
    total: usize,
) -> Option<(usize, usize)> {
    let (next, expected) = if index == 0 { (0, total) } else { previous? };
    (next == index && expected == total && (1..=4096).contains(&total))
        .then_some((index + 1, total))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
    Reasoning,
    Notice,
    Error,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Reasoning => "reasoning",
            Self::Notice => "notice",
            Self::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "tool" => Self::Tool,
            "reasoning" => Self::Reasoning,
            "notice" => Self::Notice,
            "error" => Self::Error,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub index: u64,
    pub role: Role,
    pub parts: Vec<(String, bool)>,
    pub tool: Option<ToolRow>,
}

/// Merge adjacent plain parts; a deferred body reference always stands alone.
pub fn normalize(parts: Vec<(String, bool)>) -> Vec<(String, bool)> {
    let mut normalized: Vec<(String, bool)> = Vec::new();
    for (part, tokenized) in parts {
        let deferred = tokenized && crate::BodyRef::from_token(&part).is_some();
        if !deferred && let Some((last, false)) = normalized.last_mut() {
            last.push_str(&part);
        } else {
            normalized.push((part, deferred));
        }
    }
    normalized
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Reducer {
    pub next: u64,
    pub initial_receipt: bool,
    pub first_prompt: Option<Key>,
    pub last_assistant: Option<Key>,
}

impl Reducer {
    /// `start` is the input of the `ToolStarted` that precedes a `ToolCompleted`.
    pub fn observe(
        &mut self,
        kind: &EventKind,
        tokenized: bool,
        pending: &mut Option<PendingParts>,
        start: Option<ToolInput>,
    ) -> Option<Message> {
        let mut tool = None;
        let (role, parts) =
            if let EventKind::Message(MessageEvent::MessagePart {
                text, index, total, ..
            }) = kind
            {
                if *index == 0 {
                    *pending = Some((0, *total, vec![]));
                }
                let Some((next, _)) =
                    part_progress(pending.as_ref().map(|p| (p.0, p.1)), *index, *total)
                else {
                    *pending = None;
                    return None;
                };
                let progress = pending.as_mut()?;
                progress.0 = next;
                progress.2.push((text.clone(), tokenized));
                if progress.0 != *total {
                    return None;
                }
                let parts = pending.take()?.2;
                (Role::Assistant, parts)
            } else {
                let committed = rows::committed(kind, tokenized, start)?;
                tool = committed.tool;
                (committed.role, committed.parts)
            };
        let prompt = matches!(
            kind,
            EventKind::Lifecycle(LifecycleEvent::TaskPromptPublished { .. })
        );
        self.build(
            role,
            parts,
            tool,
            prompt,
            matches!(
                kind,
                EventKind::Message(MessageEvent::FinalResultPublished { .. })
            ),
        )
    }
    pub fn message(
        &mut self,
        role: Role,
        parts: Vec<(String, bool)>,
        prompt: bool,
        finish: bool,
    ) -> Option<Message> {
        self.build(role, parts, None, prompt, finish)
    }
    /// The message of a row restored from its sources, which is never a first prompt or a final result.
    pub fn assemble(&mut self, row: Committed<'_>) -> Option<Message> {
        self.build(row.role, row.parts, row.tool, false, false)
    }
    fn build(
        &mut self,
        role: Role,
        parts: Vec<(String, bool)>,
        tool: Option<ToolRow>,
        prompt: bool,
        finish: bool,
    ) -> Option<Message> {
        use sha2::{Digest, Sha256};
        let normalized = normalize(parts);
        let mut digest = Sha256::new();
        digest.update(b"evorch/history/dedupe/v2\0");
        digest.update((normalized.len() as u64).to_be_bytes());
        let mut empty = true;
        for (part, deferred) in &normalized {
            let (hash, len): ([u8; 32], u64) = if *deferred {
                let r = crate::BodyRef::from_token(part).unwrap();
                (r.hash, r.byte_len)
            } else {
                (Sha256::digest(part.as_bytes()).into(), part.len() as u64)
            };
            digest.update(hash);
            digest.update(len.to_be_bytes());
            empty &= len == 0;
        }
        let key: Key = digest.finalize().into();
        if (matches!(role, Role::Assistant | Role::Reasoning) && empty)
            || (finish && self.last_assistant.as_ref() == Some(&key))
        {
            return None;
        }
        if prompt {
            if self.first_prompt.as_ref() == Some(&key) {
                return None;
            }
            if self.first_prompt.is_none() {
                self.first_prompt = Some(key);
                // The first accepted receipt ID of this run, replayed before its
                // first task, establishes identity without comparing bodies.
                // Later changed instructions are not that initial submission.
                if self.initial_receipt {
                    return None;
                }
            }
        }
        if role == Role::Assistant {
            self.last_assistant = Some(key);
        }
        let raw = Message {
            index: self.next,
            role,
            parts: normalized,
            tool,
        };
        self.next += 1;
        Some(raw)
    }
}
