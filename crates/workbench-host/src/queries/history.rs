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
        let content = if raw.parts.len() == 1 {
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
        scanner: &mut CommitScanner,
        after: &mut i64,
        mut visit: impl FnMut(CommittedMessage),
    ) -> Result<(), WorkbenchError> {
        runtime
            .with_storage_reader(|db| {
                loop {
                    let page = db.deferred_events_after(*after, 256, None)?;
                    for row in page.rows {
                        if matches!(row, storage::DeferredRow::Failed(_)) {
                            return Err(storage::StorageError::Io("unreadable durable log".into()));
                        }
                        if let storage::DeferredRow::Event(row) = row
                            && let Some(raw) = scanner.observe(
                                &row.event.kind,
                                row.format == storage::StoredFormat::Tokenized,
                            )
                        {
                            let thread = db
                                .thread_of_run(&raw.run)?
                                .and_then(|r| ThreadId::new(r.thread_id).ok());
                            if let Some(thread) = thread
                                && db.thread(thread.as_str())?.is_some()
                                && let Some(message) = self.message(&thread, raw)
                            {
                                visit(message);
                            }
                        }
                    }
                    *after = page.last_id.max(*after);
                    if page.scanned < 256 {
                        break;
                    }
                }
                Ok(())
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
            .with_storage_reader(|db| {
                let mut raw = Vec::new();
                let mut scanner = CommitScanner::default();
                let mut failed = false;
                db.for_each_event_for_runs_deferred(&runs, 0, high_water, |row| match row {
                    storage::DeferredRow::Failed(_) => failed = true,
                    storage::DeferredRow::Event(row) => {
                        if let Some(message) = scanner.observe(
                            &row.event.kind,
                            row.format == storage::StoredFormat::Tokenized,
                        ) {
                            raw.push(message);
                        }
                    }
                })?;
                if failed {
                    return Err(storage::StorageError::Io(
                        "unreadable committed history".into(),
                    ));
                }
                Ok(raw)
            })
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
        EventKind::Message(MessageEvent::MessageCompleted { run_id, text }) => {
            Some((run_id, MessageRole::Assistant, text))
        }
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
            Self::Stored(_) => 0,
            Self::Inline(s) => s.len(),
            Self::Parts(parts) => parts.iter().map(Self::retained_bytes).sum(),
        }
    }
    fn body_bytes(&self) -> u64 {
        match self {
            Self::Stored(r) => r.byte_len,
            Self::Inline(s) => s.len() as u64,
            Self::Parts(parts) => parts.iter().map(Self::body_bytes).sum(),
        }
    }
    fn resolve(&self, db: &storage::Database) -> Result<Option<String>, storage::StorageError> {
        match self {
            Self::Inline(s) => Ok(Some(s.clone())),
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
}
type PendingParts = (usize, usize, Vec<(String, bool)>);
#[derive(Default)]
pub(crate) struct CommitScanner {
    positions: std::collections::BTreeMap<String, u64>,
    parts: std::collections::BTreeMap<(String, String), PendingParts>,
}
impl CommitScanner {
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
        let index = self.positions.entry(run.to_owned()).or_default();
        let raw = RawMessage {
            run: run.to_owned(),
            index: *index,
            role,
            parts,
        };
        *index += 1;
        Some(raw)
    }
}
