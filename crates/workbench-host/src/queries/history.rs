use super::{Pager, error, storage_error, text};
use event_bus::{EventKind, LifecycleEvent, MessageEvent};
use runtime::AgentRuntime;
use std::{collections::VecDeque, sync::Mutex};
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    ids::*,
    queries::{ReadBody, ReadCommittedHistory},
    views::*,
    wire::*,
};

#[derive(Clone)]
enum Source {
    Stored(storage::BodyRef),
    HostInput { id: String, run: String, bytes: u64 },
    Inline(String),
    Parts(Vec<Source>),
}
#[derive(Clone)]
struct Entry {
    id: BodyId,
    thread: ThreadId,
    run: RunId,
    source: Source,
}
#[derive(Default)]
pub(crate) struct Bodies(Mutex<VecDeque<Entry>>);
impl Bodies {
    fn register(
        &self,
        thread: &ThreadId,
        run: &RunId,
        source: Source,
        bytes: u64,
    ) -> MessageContent {
        let id = BodyId::new(uuid::Uuid::new_v4().to_string()).unwrap();
        let mut entries = self.0.lock().unwrap();
        entries.push_back(Entry {
            id: id.clone(),
            thread: thread.clone(),
            run: run.clone(),
            source,
        });
        // Inline bodies retained for live events are bounded independently of blobs.
        while entries.len() > 4096
            || entries
                .iter()
                .map(|e| e.source.retained_bytes())
                .sum::<usize>()
                > 8 * 1024 * 1024
        {
            entries.pop_front();
        }
        MessageContent::Body(BodyRef {
            body_id: id,
            total_bytes: ByteOffset(bytes),
        })
    }
    pub fn content(
        &self,
        thread: &ThreadId,
        run: &RunId,
        value: &str,
        tokenized: bool,
    ) -> MessageContent {
        if tokenized && let Some(reference) = storage::BodyRef::from_token(value) {
            return self.register(thread, run, Source::Stored(reference), reference.byte_len);
        }
        let value = text(value, usize::MAX);
        if value.len() <= 16 * 1024 {
            MessageContent::Inline { text: value }
        } else {
            let bytes = value.len() as u64;
            self.register(thread, run, Source::Inline(value), bytes)
        }
    }
    fn message(&self, thread: &ThreadId, raw: RawMessage) -> Option<CommittedMessage> {
        let run = RunId::new(raw.run).ok()?;
        let content = if let Some(input) = raw.input {
            self.register(
                thread,
                &run,
                Source::HostInput {
                    id: input.id,
                    run: run.as_str().to_owned(),
                    bytes: input.bytes,
                },
                input.bytes,
            )
        } else if raw.parts.len() == 1 {
            self.content(thread, &run, &raw.parts[0].0, raw.parts[0].1)
        } else {
            let parts: Vec<_> = raw
                .parts
                .iter()
                .map(|(value, tokenized)| {
                    if *tokenized && let Some(reference) = storage::BodyRef::from_token(value) {
                        return Source::Stored(reference);
                    }
                    Source::Inline(text(value, usize::MAX))
                })
                .collect();
            let bytes = parts.iter().map(Source::body_bytes).sum();
            self.register(thread, &run, Source::Parts(parts), bytes)
        };
        Some(CommittedMessage {
            thread: thread.clone(),
            run,
            index: Sequence(raw.index),
            role: raw.role,
            content,
        })
    }
    pub fn changes(
        &self,
        runtime: &AgentRuntime,
        published: &mut CommitScanner,
        runs: &[String],
        mut visit: impl FnMut(CommittedMessage),
    ) -> Result<bool, WorkbenchError> {
        runtime
            .with_storage_reader(|db| {
                let raw = committed_messages(db, runs, db.last_event_id()?)?;
                let mut grouped = std::collections::BTreeMap::<String, Vec<&RawMessage>>::new();
                for message in &raw {
                    grouped
                        .entry(message.run.clone())
                        .or_default()
                        .push(message);
                }
                // Delivered may arrive after messages following its acceptance time
                // were published. Never silently renumber their append-only indices.
                let rewritten = grouped.iter().any(|(run, messages)| {
                    published.runs.get(run).is_some_and(|(count, digest)| {
                        messages.len() < *count || super::revision(&&messages[..*count]) != *digest
                    })
                }) || published.runs.iter().any(|(run, (count, _))| {
                    runs.contains(run) && *count > 0 && !grouped.contains_key(run)
                });
                if !rewritten {
                    for message in &raw {
                        let count = published.runs.get(&message.run).map_or(0, |(n, _)| *n);
                        if message.index >= count as u64
                            && let Some(thread) = db.thread_of_run(&message.run)?
                            && db.thread(&thread.thread_id)?.is_some()
                            && let Ok(thread) = ThreadId::new(thread.thread_id)
                            && let Some(message) = self.message(&thread, message.clone())
                        {
                            visit(message);
                        }
                    }
                }
                published.runs = grouped
                    .into_iter()
                    .map(|(run, rows)| (run, (rows.len(), super::revision(&rows))))
                    .collect();
                Ok(rewritten)
            })
            .map_err(storage_error)
            .map(Option::unwrap_or_default)
    }
    pub fn read(&self, runtime: &AgentRuntime, q: &ReadBody) -> Result<BodyChunk, WorkbenchError> {
        if !(4..=64 * 1024).contains(&q.max_bytes) {
            return Err(error(
                ErrorCode::InvalidRequest,
                "body chunk must be 4..=65536 bytes",
            ));
        }
        let entry = self
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == q.body_id)
            .cloned()
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown or expired body"))?;
        let body = runtime
            .with_storage_reader(|db| {
                if db.thread(entry.thread.as_str())?.is_none()
                    || !db
                        .tombstoned_runs(&[entry.run.as_str().to_owned()])?
                        .is_empty()
                {
                    return Ok(None);
                }
                entry.source.resolve(db)
            })
            .map_err(storage_error)?
            .flatten()
            .ok_or_else(|| error(ErrorCode::NotFound, "body is no longer available"))?;
        Ok(BodyChunk::slice(
            q.body_id.clone(),
            &body,
            q.offset,
            q.max_bytes,
        )?)
    }
    pub fn history(
        &self,
        runtime: &AgentRuntime,
        pager: &Pager,
        q: &ReadCommittedHistory,
    ) -> Result<Page<CommittedMessage>, WorkbenchError> {
        let (runs, latest) = runtime
            .with_storage_reader(|db| {
                if db.thread(q.thread.as_str())?.is_none() {
                    return Ok(None);
                }
                let runs: Vec<_> = db
                    .runs_of_thread(q.thread.as_str())?
                    .into_iter()
                    .map(|r| r.run_id)
                    .filter(|r| q.run.as_ref().is_none_or(|id| id.as_str() == r))
                    .collect();
                if q.run.is_some() && runs.is_empty() {
                    return Ok(None);
                }
                Ok(Some((runs, db.last_event_id()?)))
            })
            .map_err(storage_error)?
            .flatten()
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown thread or run"))?;
        // Bind the frozen event cut to its live run membership too: deletion
        // must revoke a cursor rather than shift the remaining message offsets.
        let key = format!(
            "history:{}:{:?}:{}",
            q.thread.as_str(),
            q.run,
            super::revision(&runs).get()
        );
        let high_water = pager
            .cursor_revision(&key, &q.page)?
            .map_or(latest, |r| r.get() as i64);
        let raw = runtime
            .with_storage_reader(|db| committed_messages(db, &runs, high_water))
            .map_err(storage_error)?
            .ok_or_else(|| error(ErrorCode::NotFound, "history is unavailable"))?;
        // Retention can remove rows below the frozen cut. Bind their deferred
        // descriptors as well, without ever loading the referenced blobs.
        let key = format!("{key}#{}", super::revision(&raw).get());
        // Page the tokenized records before resolving or registering any bodies.
        let page = pager.page(&key, &raw, &q.page, Revision(high_water as u64))?;
        Ok(Page {
            items: page
                .items
                .into_iter()
                .filter_map(|raw| self.message(&q.thread, raw))
                .collect(),
            next_cursor: page.next_cursor,
            snapshot_revision: page.snapshot_revision,
        })
    }
}
pub(crate) fn committed_text(kind: &EventKind) -> Option<(&str, MessageRole, &str)> {
    match kind {
        EventKind::Message(
            MessageEvent::MessageCompleted { run_id, text }
            | MessageEvent::FinalResultPublished { run_id, text },
        ) => Some((run_id, MessageRole::Assistant, text)),
        EventKind::Lifecycle(LifecycleEvent::TaskPromptPublished {
            run_id,
            prompt,
            user_text,
            ..
        }) => Some((
            run_id,
            MessageRole::User,
            user_text.as_deref().unwrap_or(prompt),
        )),
        EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
            run_id: Some(run),
            output: Some(output),
            ..
        }) => Some((run, MessageRole::Tool, output)),
        _ => None,
    }
}
impl Source {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Stored(_) | Self::HostInput { .. } => 0,
            Self::Inline(s) => s.len(),
            Self::Parts(parts) => parts.iter().map(Self::retained_bytes).sum(),
        }
    }
    fn body_bytes(&self) -> u64 {
        match self {
            Self::Stored(r) => r.byte_len,
            Self::HostInput { bytes, .. } => *bytes,
            Self::Inline(s) => s.len() as u64,
            Self::Parts(parts) => parts.iter().map(Self::body_bytes).sum(),
        }
    }
    fn resolve(&self, db: &storage::Database) -> Result<Option<String>, storage::StorageError> {
        match self {
            Self::Inline(s) => Ok(Some(s.clone())),
            Self::HostInput { id, run, .. } => Ok(db
                .host_input_receipts(std::slice::from_ref(id))?
                .into_iter()
                .find(|r| r.run_id == *run && r.status == storage::HostInputStatus::Delivered)
                .map(|r| text(&r.text, usize::MAX))),
            Self::Stored(r) => Ok(db
                .resolve_bodies(&[*r])?
                .pop()
                .and_then(Result::ok)
                .map(|s| text(&s, usize::MAX))),
            Self::Parts(parts) => {
                let mut body = String::new();
                for part in parts {
                    match part.resolve(db)? {
                        Some(text) => body.push_str(&text),
                        None => return Ok(None),
                    }
                }
                Ok(Some(text(&body, usize::MAX)))
            }
        }
    }
}
#[derive(Clone, serde::Serialize)]
struct RawMessage {
    run: String,
    index: u64,
    role: MessageRole,
    parts: Vec<(String, bool)>,
    input: Option<InputBody>,
}
#[derive(Clone, serde::Serialize)]
struct InputBody {
    id: String,
    bytes: u64,
}
type PendingParts = (usize, usize, Vec<(String, bool)>);
#[derive(Default)]
struct Scanner {
    positions: std::collections::BTreeMap<String, u64>,
    parts: std::collections::BTreeMap<(String, String), PendingParts>,
    first_user: std::collections::BTreeMap<String, Vec<([u8; 32], u64)>>,
    last_assistant: std::collections::BTreeMap<String, Vec<([u8; 32], u64)>>,
}
impl Scanner {
    fn observe(&mut self, kind: &EventKind, tokenized: bool) -> Option<RawMessage> {
        let (run, role, parts) = if let EventKind::Message(MessageEvent::MessagePart {
            run_id,
            response_id,
            text,
            index,
            total,
        }) = kind
        {
            let key = (run_id.clone(), response_id.clone());
            if *index == 0 {
                self.parts.insert(key.clone(), (0, *total, vec![]));
            }
            let pending = self.parts.get_mut(&key)?;
            if pending.0 != *index || pending.1 != *total || *total == 0 || *total > 4096 {
                self.parts.remove(&key);
                return None;
            }
            pending.0 += 1;
            pending.2.push((text.clone(), tokenized));
            if pending.0 != *total {
                return None;
            }
            let parts = self.parts.remove(&key)?.2;
            (run_id.as_str(), MessageRole::Assistant, parts)
        } else {
            let (run, role, body) = committed_text(kind)?;
            (run, role, vec![(body.to_owned(), tokenized)])
        };
        self.message(
            run,
            role,
            parts,
            matches!(
                kind,
                EventKind::Lifecycle(LifecycleEvent::TaskPromptPublished { .. })
            ),
            matches!(
                kind,
                EventKind::Message(MessageEvent::FinalResultPublished { .. })
            ),
        )
    }
    fn input(
        &mut self,
        db: &storage::Database,
        input: storage::host_input::DeliveredHostInput,
    ) -> Result<Option<RawMessage>, storage::StorageError> {
        if let Some(value) = input.text {
            return Ok(self.message(
                &input.run_id,
                MessageRole::User,
                vec![(value, false)],
                false,
                false,
            ));
        }
        if !self.first_user.contains_key(&input.run_id) {
            // Only the first instruction needs equality against TaskPromptPublished.
            // Read that one selected receipt, never every large followup body.
            use sha2::{Digest, Sha256};
            if let Some(receipt) = db
                .host_input_receipts(std::slice::from_ref(&input.prompt_id))?
                .first()
            {
                self.first_user.insert(
                    input.run_id.clone(),
                    vec![(
                        Sha256::digest(receipt.text.as_bytes()).into(),
                        receipt.text.len() as u64,
                    )],
                );
            }
        }
        let index = self.positions.entry(input.run_id.clone()).or_default();
        let raw = RawMessage {
            run: input.run_id,
            index: *index,
            role: MessageRole::User,
            parts: vec![],
            input: Some(InputBody {
                id: input.prompt_id,
                bytes: input.text_bytes,
            }),
        };
        *index += 1;
        Ok(Some(raw))
    }
    fn message(
        &mut self,
        run: &str,
        role: MessageRole,
        parts: Vec<(String, bool)>,
        prompt: bool,
        finish: bool,
    ) -> Option<RawMessage> {
        use sha2::{Digest, Sha256};
        let mut normalized: Vec<(String, bool)> = Vec::new();
        for (part, tokenized) in parts {
            let deferred = tokenized && storage::BodyRef::from_token(&part).is_some();
            if !deferred && let Some((last, false)) = normalized.last_mut() {
                last.push_str(&part);
            } else {
                normalized.push((part, deferred));
            }
        }
        let key: Vec<_> = normalized
            .iter()
            .map(|(part, deferred)| {
                if *deferred {
                    let r = storage::BodyRef::from_token(part).unwrap();
                    (r.hash, r.byte_len)
                } else {
                    (Sha256::digest(part.as_bytes()).into(), part.len() as u64)
                }
            })
            .collect();
        if (role == MessageRole::Assistant && key.iter().all(|(_, n)| *n == 0))
            || (finish && self.last_assistant.get(run) == Some(&key))
        {
            return None;
        }
        if role == MessageRole::User {
            if prompt && self.first_user.get(run) == Some(&key) {
                return None;
            }
            self.first_user.entry(run.to_owned()).or_insert(key.clone());
        }
        if role == MessageRole::Assistant {
            self.last_assistant.insert(run.to_owned(), key);
        }
        let parts = normalized;
        let index = self.positions.entry(run.to_owned()).or_default();
        let raw = RawMessage {
            run: run.to_owned(),
            index: *index,
            role,
            parts,
            input: None,
        };
        *index += 1;
        Some(raw)
    }
}

/// Only counts and a prefix digest are retained, not the historical bodies.
#[derive(Default)]
pub(crate) struct CommitScanner {
    runs: std::collections::BTreeMap<String, (usize, Revision)>,
}

fn committed_messages(
    db: &storage::Database,
    runs: &[String],
    high_water: i64,
) -> Result<Vec<RawMessage>, storage::StorageError> {
    let mut inputs = Vec::new();
    for run in runs {
        inputs.extend(db.delivered_host_input_history(run)?);
    }
    inputs.sort_by(|a, b| (a.accepted_at_ns, &a.prompt_id).cmp(&(b.accepted_at_ns, &b.prompt_id)));
    let mut inputs = inputs.into_iter().peekable();
    let mut raw = Vec::new();
    let mut scanner = Scanner::default();
    let mut failure = None;
    db.for_each_event_for_runs_deferred(runs, 0, high_water, |row| match row {
        storage::DeferredRow::Failed(_) => {
            failure = Some(storage::StorageError::Io(
                "unreadable committed history".into(),
            ))
        }
        storage::DeferredRow::Event(row) => {
            let at = match row
                .event
                .meta
                .wall_clock
                .duration_since(std::time::UNIX_EPOCH)
            {
                Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
                Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
            };
            // Match the GUI's durable replay: accepted user inputs precede the
            // first event at or after their acceptance timestamp (ID breaks ties).
            while inputs.peek().is_some_and(|r| r.accepted_at_ns <= at) {
                match scanner.input(db, inputs.next().unwrap()) {
                    Ok(Some(message)) => raw.push(message),
                    Ok(None) => {}
                    Err(e) => failure = Some(e),
                }
            }
            // A multipart assistant and a single-token finish have different
            // descriptor shapes even when their logical text is identical. Only
            // this ambiguous equality reads the selected assistant parts, one at
            // a time; ordinary history stays entirely deferred.
            match duplicates_multipart_finish(db, &raw, &row.event.kind, row.format) {
                Ok(true) => return,
                Ok(false) => {}
                Err(e) => {
                    failure = Some(e);
                    return;
                }
            }
            if let Some(message) = scanner.observe(
                &row.event.kind,
                row.format == storage::StoredFormat::Tokenized,
            ) {
                raw.push(message);
            }
        }
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    // A receipt is authoritative even if its notification has not been stored.
    for input in inputs {
        if let Some(message) = scanner.input(db, input)? {
            raw.push(message);
        }
    }
    Ok(raw)
}

fn duplicates_multipart_finish(
    db: &storage::Database,
    history: &[RawMessage],
    kind: &EventKind,
    format: storage::StoredFormat,
) -> Result<bool, storage::StorageError> {
    use sha2::{Digest, Sha256};
    let EventKind::Message(MessageEvent::FinalResultPublished { run_id, text }) = kind else {
        return Ok(false);
    };
    let Some(last) = history
        .iter()
        .rev()
        .find(|m| m.run == *run_id && m.role == MessageRole::Assistant)
        .filter(|m| m.parts.len() > 1)
    else {
        return Ok(false);
    };
    let finish = (format == storage::StoredFormat::Tokenized)
        .then(|| storage::BodyRef::from_token(text))
        .flatten();
    let bytes = finish.map_or(text.len() as u64, |r| r.byte_len);
    let part_bytes: u64 = last
        .parts
        .iter()
        .map(|(part, deferred)| {
            if *deferred {
                storage::BodyRef::from_token(part).unwrap().byte_len
            } else {
                part.len() as u64
            }
        })
        .sum();
    if bytes != part_bytes {
        return Ok(false);
    }
    let expected: [u8; 32] =
        finish.map_or_else(|| Sha256::digest(text.as_bytes()).into(), |r| r.hash);
    let mut joined = Sha256::new();
    for (part, deferred) in &last.parts {
        if *deferred {
            let body = db
                .resolve_bodies(&[storage::BodyRef::from_token(part).unwrap()])?
                .pop()
                .ok_or_else(|| {
                    storage::StorageError::Io("multipart equality body is unavailable".into())
                })?
                .map_err(|_| {
                    storage::StorageError::Io("multipart equality body is unreadable".into())
                })?;
            joined.update(body.as_bytes());
        } else {
            joined.update(part.as_bytes());
        }
    }
    Ok(<[u8; 32]>::from(joined.finalize()) == expected)
}
