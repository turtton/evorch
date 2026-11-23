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
struct Retained {
    entries: VecDeque<Entry>,
    bytes: usize,
}
#[derive(Default)]
pub(crate) struct Bodies(Mutex<Retained>);
impl Bodies {
    #[cfg(any(test, feature = "test-support"))]
    pub fn retained(&self) -> usize {
        self.0.lock().unwrap().entries.len()
    }
    fn register(
        &self,
        thread: &ThreadId,
        run: &RunId,
        source: Source,
        bytes: u64,
    ) -> MessageContent {
        let id = BodyId::new(uuid::Uuid::new_v4().to_string()).unwrap();
        let mut retained = self.0.lock().unwrap();
        retained.bytes += source.retained_bytes();
        retained.entries.push_back(Entry {
            id: id.clone(),
            thread: thread.clone(),
            run: run.clone(),
            source,
        });
        // Inline bodies retained for live events are bounded independently of blobs.
        while retained.entries.len() > 4096 || retained.bytes > 8 * 1024 * 1024 {
            let Some(evicted) = retained.entries.pop_front() else {
                break;
            };
            retained.bytes -= evicted.source.retained_bytes();
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
        publish: bool,
        mut visit: impl FnMut(CommittedMessage),
    ) -> Result<bool, WorkbenchError> {
        runtime
            .with_storage_reader(|db| {
                let mut prefixes = std::collections::BTreeMap::<String, Digest>::new();
                let mut fresh = Vec::new();
                let mut record = |order: Order, message: RawMessage, chain: &Digest| {
                    let count = published
                        .runs
                        .get(&message.run)
                        .map_or(0, |(n, _)| *n as u64);
                    if message.index + 1 == count {
                        prefixes.insert(message.run.clone(), *chain);
                    }
                    // Before the first publication every message is history: counting
                    // it is enough, resolving its thread or body would be discarded.
                    if publish && message.index >= count {
                        fresh.push((order, message));
                    }
                };
                let folds = fold_runs(db, runs, db.last_event_id()?, &mut record)?;
                // Delivered may arrive after messages following its acceptance time
                // were published. Never silently renumber their append-only indices.
                let rewritten = folds.iter().any(|(run, fold)| {
                    published.runs.get(*run).is_some_and(|(count, chain)| {
                        fold.next < *count as u64
                            || (*count > 0 && prefixes.get(*run) != Some(chain))
                    })
                });
                if !rewritten {
                    fresh.sort_by(|a, b| a.0.cmp(&b.0));
                    for (_, message) in fresh {
                        if let Some(thread) = db.thread_of_run(&message.run)?
                            && db.thread(&thread.thread_id)?.is_some()
                            && let Ok(thread) = ThreadId::new(thread.thread_id)
                            && let Some(message) = self.message(&thread, message)
                        {
                            visit(message);
                        }
                    }
                }
                published.runs = folds
                    .into_iter()
                    .map(|(run, fold)| (run.to_owned(), (fold.next as usize, fold.chain)))
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
            .entries
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
            "history:{:?}:{}:{:?}:{}",
            q.direction,
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
        let page = match q.direction {
            workbench_protocol::queries::HistoryDirection::Forward => {
                pager.page(&key, &raw, &q.page, Revision(high_water as u64))?
            }
            workbench_protocol::queries::HistoryDirection::Backward => {
                pager.page_backward(&key, &raw, &q.page, Revision(high_water as u64))?
            }
        };
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
type Key = Vec<([u8; 32], u64)>;
type Digest = [u8; 32];

/// Where a message sits among the messages of every run: the committed row that emitted
/// it, or the one its input was merged ahead of (`i64::MAX` for trailing inputs).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Order(i64, bool, i64, String);

fn wall_clock_ns(at: std::time::SystemTime) -> i64 {
    match at.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

fn extend(chain: &Digest, raw: &RawMessage) -> Digest {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    hash.update(chain);
    hash.update(serde_json::to_vec(raw).expect("serializable message"));
    hash.finalize().into()
}

fn by_acceptance(
    mut inputs: Vec<storage::host_input::DeliveredHostInput>,
) -> std::collections::VecDeque<storage::host_input::DeliveredHostInput> {
    inputs.sort_by(|a, b| (a.accepted_at_ns, &a.prompt_id).cmp(&(b.accepted_at_ns, &b.prompt_id)));
    inputs.into()
}

/// The transcript of one run, folded from its committed rows in id order and its
/// delivered inputs. No other run can change it.
#[derive(Clone)]
struct RunFold {
    run: String,
    next: u64,
    parts: std::collections::BTreeMap<String, PendingParts>,
    initial_receipt: bool,
    first_prompt: Option<Key>,
    last_assistant: Option<Key>,
    merge_max: i64,
    last_id: i64,
    consumed: std::collections::BTreeMap<String, (i64, bool)>,
    chain: Digest,
}
impl RunFold {
    fn new(run: &str) -> Self {
        Self {
            run: run.to_owned(),
            next: 0,
            parts: Default::default(),
            initial_receipt: false,
            first_prompt: None,
            last_assistant: None,
            merge_max: i64::MIN,
            last_id: 0,
            consumed: Default::default(),
            chain: [0; 32],
        }
    }
    /// An input goes ahead of this run's first committed row stamped at or after its
    /// acceptance, ties broken by prompt id.
    fn row(
        &mut self,
        row: &storage::DeferredStoredEvent,
        inputs: &mut std::collections::VecDeque<storage::host_input::DeliveredHostInput>,
        emit: &mut impl FnMut(Order, RawMessage, &Digest),
    ) {
        let at = wall_clock_ns(row.event.meta.wall_clock);
        while inputs.front().is_some_and(|r| r.accepted_at_ns <= at) {
            let input = inputs.pop_front().unwrap();
            self.consumed.insert(
                input.prompt_id.clone(),
                (input.accepted_at_ns, input.first_in_run),
            );
            let order = Order(row.id, false, input.accepted_at_ns, input.prompt_id.clone());
            if let Some(message) = self.input(input) {
                self.push(order, message, emit);
            }
        }
        if let Some(message) = self.observe(
            &row.event.kind,
            row.format == storage::StoredFormat::Tokenized,
        ) {
            self.push(Order(row.id, true, 0, String::new()), message, emit);
        }
        self.merge_max = self.merge_max.max(at);
        self.last_id = row.id;
    }
    /// A receipt is authoritative even if its notification has not been stored.
    fn trailing(
        &mut self,
        inputs: std::collections::VecDeque<storage::host_input::DeliveredHostInput>,
        emit: &mut impl FnMut(Order, RawMessage, &Digest),
    ) {
        for input in inputs {
            let order = Order(
                i64::MAX,
                false,
                input.accepted_at_ns,
                input.prompt_id.clone(),
            );
            if let Some(message) = self.input(input) {
                self.push(order, message, emit);
            }
        }
    }
    fn push(
        &mut self,
        order: Order,
        message: RawMessage,
        emit: &mut impl FnMut(Order, RawMessage, &Digest),
    ) {
        self.chain = extend(&self.chain, &message);
        emit(order, message, &self.chain);
    }
    fn observe(&mut self, kind: &EventKind, tokenized: bool) -> Option<RawMessage> {
        let (role, parts) = if let EventKind::Message(MessageEvent::MessagePart {
            response_id,
            text,
            index,
            total,
            ..
        }) = kind
        {
            if *index == 0 {
                self.parts.insert(response_id.clone(), (0, *total, vec![]));
            }
            let pending = self.parts.get_mut(response_id)?;
            if pending.0 != *index || pending.1 != *total || *total == 0 || *total > 4096 {
                self.parts.remove(response_id);
                return None;
            }
            pending.0 += 1;
            pending.2.push((text.clone(), tokenized));
            if pending.0 != *total {
                return None;
            }
            let parts = self.parts.remove(response_id)?.2;
            (MessageRole::Assistant, parts)
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
    fn input(&mut self, input: storage::host_input::DeliveredHostInput) -> Option<RawMessage> {
        if input.first_in_run {
            self.initial_receipt = true;
        }
        if let Some(value) = input.text {
            return self.message(MessageRole::User, vec![(value, false)], false, false);
        }
        let raw = RawMessage {
            run: self.run.clone(),
            index: self.next,
            role: MessageRole::User,
            parts: vec![],
            input: Some(InputBody {
                id: input.prompt_id,
                bytes: input.text_bytes,
            }),
        };
        self.next += 1;
        Some(raw)
    }
    fn message(
        &mut self,
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
        let key: Key = normalized
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
            || (finish && self.last_assistant.as_ref() == Some(&key))
        {
            return None;
        }
        if prompt {
            if self.first_prompt.as_ref() == Some(&key) {
                return None;
            }
            if self.first_prompt.is_none() {
                self.first_prompt = Some(key.clone());
                // The first accepted receipt ID of this run, replayed before its
                // first task, establishes identity without comparing bodies.
                // Later changed instructions are not that initial submission.
                if self.initial_receipt {
                    return None;
                }
            }
        }
        if role == MessageRole::Assistant {
            self.last_assistant = Some(key);
        }
        let raw = RawMessage {
            run: self.run.clone(),
            index: self.next,
            role,
            parts: normalized,
            input: None,
        };
        self.next += 1;
        Some(raw)
    }
}

fn unreadable() -> storage::StorageError {
    storage::StorageError::Io("unreadable committed history".into())
}

/// Folds `run` through `through` and returns the inputs no committed row has taken yet.
fn fold_run(
    db: &storage::Database,
    run: &str,
    through: i64,
    inputs: Vec<storage::host_input::DeliveredHostInput>,
    mut emit: impl FnMut(Order, RawMessage, &Digest),
) -> Result<
    (
        RunFold,
        std::collections::VecDeque<storage::host_input::DeliveredHostInput>,
    ),
    storage::StorageError,
> {
    let mut fold = RunFold::new(run);
    let mut inputs = by_acceptance(inputs);
    let mut failure = None;
    db.for_each_committed_event_of_run(run, through, |row| match row {
        storage::DeferredRow::Failed(_) => failure = Some(unreadable()),
        storage::DeferredRow::Event(row) => fold.row(&row, &mut inputs, &mut emit),
    })?;
    match failure {
        Some(e) => Err(e),
        None => Ok((fold, inputs)),
    }
}

fn fold_runs<'a>(
    db: &storage::Database,
    runs: &'a [String],
    through: i64,
    mut emit: impl FnMut(Order, RawMessage, &Digest),
) -> Result<std::collections::BTreeMap<&'a str, RunFold>, storage::StorageError> {
    let mut folds = std::collections::BTreeMap::new();
    for run in runs {
        let inputs = by_acceptance(db.delivered_host_input_history(run)?);
        folds.insert(run.as_str(), (RunFold::new(run), inputs));
    }
    let listed = folds.keys().copied().collect();
    let mut failure = None;
    db.for_each_committed_event_after(&listed, 0, through, |row| match row {
        storage::DeferredRow::Failed(_) => failure = Some(unreadable()),
        storage::DeferredRow::Event(row) => {
            let run = row
                .run_id
                .as_deref()
                .or_else(|| row.event.kind.primary_run_id());
            if let Some((fold, inputs)) = run.and_then(|run| folds.get_mut(run)) {
                fold.row(&row, inputs, &mut emit);
            }
        }
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    Ok(folds
        .into_iter()
        .map(|(run, (mut fold, inputs))| {
            fold.trailing(inputs, &mut emit);
            (run, fold)
        })
        .collect())
}

/// Only counts and a chained prefix digest are retained, not the historical bodies.
#[derive(Default)]
pub(crate) struct CommitScanner {
    runs: std::collections::BTreeMap<String, (usize, Digest)>,
}

fn committed_messages(
    db: &storage::Database,
    runs: &[String],
    high_water: i64,
) -> Result<Vec<RawMessage>, storage::StorageError> {
    let mut ordered = Vec::new();
    let mut emit = |order, message, _: &Digest| ordered.push((order, message));
    for run in runs {
        let inputs = db.delivered_host_input_history(run)?;
        let (mut fold, rest) = fold_run(db, run, high_water, inputs, &mut emit)?;
        fold.trailing(rest, &mut emit);
    }
    // Each run's messages are already in order: this is their k-way merge.
    ordered.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(ordered.into_iter().map(|(_, message)| message).collect())
}

#[cfg(test)]
mod tests;
