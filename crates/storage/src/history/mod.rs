//! Pure transcript semantics shared by live scanning and the events-only index.
//! No protocol dependency and no storage reads; callers own pending response state.
pub mod build;
pub(crate) mod index;
#[cfg(test)]
mod index_tests;
pub(crate) mod meter;
pub mod read;
#[cfg(test)]
mod read_tests;
#[cfg(test)]
mod tests;

use event_bus::{EventKind, LifecycleEvent, MessageEvent};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub index: u64,
    pub role: Role,
    pub parts: Vec<(String, bool)>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Reducer {
    pub next: u64,
    pub initial_receipt: bool,
    pub first_prompt: Option<Key>,
    pub last_assistant: Option<Key>,
}

pub fn committed_text(kind: &EventKind) -> Option<(&str, Role, &str)> {
    match kind {
        EventKind::Message(
            MessageEvent::MessageCompleted { run_id, text }
            | MessageEvent::FinalResultPublished { run_id, text },
        ) => Some((run_id, Role::Assistant, text)),
        EventKind::Lifecycle(LifecycleEvent::TaskPromptPublished {
            run_id,
            prompt,
            user_text,
            ..
        }) => Some((run_id, Role::User, user_text.as_deref().unwrap_or(prompt))),
        EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
            run_id: Some(run),
            output: Some(output),
            ..
        }) => Some((run, Role::Tool, output)),
        _ => None,
    }
}
impl Reducer {
    pub fn observe(
        &mut self,
        kind: &EventKind,
        tokenized: bool,
        pending: &mut Option<PendingParts>,
    ) -> Option<Message> {
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
                let (_, role, body) = committed_text(kind)?;
                (role, vec![(body.to_owned(), tokenized)])
            };
        let prompt = matches!(
            kind,
            EventKind::Lifecycle(LifecycleEvent::TaskPromptPublished { .. })
        );
        self.message(
            role,
            parts,
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
        use sha2::{Digest, Sha256};
        let mut normalized: Vec<(String, bool)> = Vec::new();
        for (part, tokenized) in parts {
            let deferred = tokenized && crate::BodyRef::from_token(&part).is_some();
            if !deferred && let Some((last, false)) = normalized.last_mut() {
                last.push_str(&part);
            } else {
                normalized.push((part, deferred));
            }
        }
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
        if (role == Role::Assistant && empty)
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
        };
        self.next += 1;
        Some(raw)
    }
}
