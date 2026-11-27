use super::{Pager, error, storage_error, text};
use event_bus::{EventKind, MessageEvent};
use runtime::AgentRuntime;
use std::{collections::VecDeque, sync::Mutex};
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    ids::*,
    limits::{MAX_NAME_BYTES, MAX_TITLE_BYTES},
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
    #[cfg(any(test, feature = "test-support"))]
    history_route: Option<super::HistoryRoute>,
}
#[derive(Default)]
pub(crate) struct Bodies(Mutex<Retained>);
impl Bodies {
    #[cfg(any(test, feature = "test-support"))]
    pub fn retained(&self) -> usize {
        self.0.lock().unwrap().entries.len()
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn history_route(&self) -> Option<super::HistoryRoute> {
        self.0.lock().unwrap().history_route.clone()
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
    fn parts_content(
        &self,
        thread: &ThreadId,
        run: &RunId,
        parts: &[(String, bool)],
    ) -> MessageContent {
        match parts {
            [] => MessageContent::Inline {
                text: String::new(),
            },
            [(value, tokenized)] => self.content(thread, run, value, *tokenized),
            _ => {
                let parts: Vec<_> = parts
                    .iter()
                    .map(|(value, tokenized)| {
                        if *tokenized && let Some(reference) = storage::BodyRef::from_token(value) {
                            return Source::Stored(reference);
                        }
                        Source::Inline(text(value, usize::MAX))
                    })
                    .collect();
                let bytes = parts.iter().map(Source::body_bytes).sum();
                self.register(thread, run, Source::Parts(parts), bytes)
            }
        }
    }
    fn tool(
        &self,
        thread: &ThreadId,
        run: &RunId,
        tool: storage::history::ToolRow,
    ) -> Result<ToolCallView, WorkbenchError> {
        // An id joins a call to its parent and to other records, so it is never shortened.
        let id = |value: &str| {
            if value.is_empty() || value.len() > MAX_NAME_BYTES {
                return Err(error(
                    ErrorCode::LimitExceeded,
                    "a tool call id is empty or longer than the protocol allows",
                ));
            }
            Ok(value.to_owned())
        };
        let name = |value: &str| text(value, MAX_NAME_BYTES);
        Ok(ToolCallView {
            tool_name: Some(name(&tool.tool_name))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "unknown".to_owned()),
            call_id: id(&tool.call_id)?,
            parent_call_id: tool.parent_call_id.as_deref().map(id).transpose()?,
            input_summary: tool
                .input
                .as_ref()
                .and_then(|input| input.summary.as_deref())
                .map(|summary| text(summary, MAX_TITLE_BYTES))
                .filter(|summary| !summary.is_empty()),
            input: tool
                .input
                .as_ref()
                .map(|input| self.parts_content(thread, run, &input.parts)),
            status: if tool.is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Succeeded
            },
            detail: tool
                .detail
                .as_ref()
                .map(|detail| self.parts_content(thread, run, detail)),
        })
    }
    /// `None` for a message of a run the protocol cannot name; an error for a tool call
    /// whose ids it cannot carry.
    fn message(
        &self,
        thread: &ThreadId,
        raw: RawMessage,
    ) -> Result<Option<CommittedMessage>, WorkbenchError> {
        let Ok(run) = RunId::new(raw.run) else {
            return Ok(None);
        };
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
        } else {
            self.parts_content(thread, &run, &raw.parts)
        };
        let tool = raw
            .tool
            .map(|tool| self.tool(thread, &run, tool))
            .transpose()?;
        Ok(Some(CommittedMessage {
            thread: thread.clone(),
            run,
            index: Sequence(raw.index),
            role: raw.role,
            content,
            tool,
        }))
    }
    pub fn changes(
        &self,
        db: &storage::Database,
        scanner: &mut CommitScanner,
        runs: &[String],
        mut visit: impl FnMut(CommittedMessage),
    ) -> Result<bool, storage::StorageError> {
        let mut step = scanner.advance(db, runs)?;
        let mut unrepresentable = false;
        if !step.rewritten {
            for (_, message) in std::mem::take(&mut step.fresh) {
                if let Some(thread) = db.thread_of_run(&message.run)?
                    && db.thread(&thread.thread_id)?.is_some()
                    && let Ok(thread) = ThreadId::new(thread.thread_id)
                {
                    match self.message(&thread, message) {
                        Ok(Some(message)) => visit(message),
                        Ok(None) => {}
                        // The stream cannot carry the row, so clients read the history again
                        // and get the error there instead of a silent gap.
                        Err(_) => unrepresentable = true,
                    }
                }
            }
        }
        Ok(scanner.commit(step) || unrepresentable)
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
        let key = format!(
            "history:{:?}:{}:{:?}",
            q.direction,
            q.thread.as_str(),
            q.run
        );
        let read = runtime
            .with_storage_reader(|db| Ok(read_history(db, pager, q, &key)))
            .map_err(storage_error)?
            .ok_or_else(|| error(ErrorCode::NotFound, "history is unavailable"))??;
        let (page, reason) = match read {
            HistoryRead::Indexed {
                page,
                high_water,
                freeze,
            } => {
                let next_cursor = page
                    .next
                    .map(|boundary| pager.history_next(&key, high_water, boundary, freeze));
                (
                    Page {
                        items: page.items.into_iter().map(RawMessage::from).collect(),
                        next_cursor,
                        snapshot_revision: Revision(high_water as u64),
                    },
                    None,
                )
            }
            HistoryRead::Legacy {
                runs,
                high_water,
                key,
                reason,
            } => {
                let raw = runtime
                    .with_storage_reader(|db| committed_messages(db, &runs, high_water))
                    .map_err(storage_error)?
                    .ok_or_else(|| error(ErrorCode::NotFound, "history is unavailable"))?;
                let key = format!("{key}#{}", super::revision(&raw).get());
                let page = match q.direction {
                    workbench_protocol::queries::HistoryDirection::Forward => {
                        pager.page(&key, &raw, &q.page, Revision(high_water as u64))?
                    }
                    workbench_protocol::queries::HistoryDirection::Backward => {
                        pager.page_backward(&key, &raw, &q.page, Revision(high_water as u64))?
                    }
                };
                (page, Some(reason))
            }
        };
        // The read snapshot is closed before selected bodies enter the capability cache.
        let result = Page {
            items: page
                .items
                .into_iter()
                .filter_map(|raw| self.message(&q.thread, raw).transpose())
                .collect::<Result<_, _>>()?,
            next_cursor: page.next_cursor,
            snapshot_revision: page.snapshot_revision,
        };
        #[cfg(any(test, feature = "test-support"))]
        {
            self.0.lock().unwrap().history_route = Some(match reason {
                None => super::HistoryRoute::Indexed,
                Some(reason) => super::HistoryRoute::Legacy(reason),
            });
        }
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = reason;
        Ok(result)
    }
}
enum HistoryRead {
    Indexed {
        page: storage::history::read::HistoryPage,
        high_water: i64,
        freeze: [u8; 32],
    },
    Legacy {
        runs: Vec<String>,
        high_water: i64,
        key: String,
        reason: &'static str,
    },
}

fn read_history(
    db: &storage::Database,
    pager: &Pager,
    q: &ReadCommittedHistory,
    key: &str,
) -> Result<HistoryRead, WorkbenchError> {
    use super::HistoryCursor;
    use storage::history::read::{Availability, Direction};
    let snapshot = db
        .history_snapshot(q.thread.as_str(), q.run.as_ref().map(|r| r.as_str()))
        .map_err(storage_error)?
        .ok_or_else(|| error(ErrorCode::NotFound, "unknown thread or run"))?;
    let cursor = pager.history_cursor(key, &q.page)?;
    let legacy_key = format!("{key}:{}", super::revision(&snapshot.runs).get());
    let high_water = match &cursor {
        Some(HistoryCursor::Legacy(revision)) => {
            pager.cursor_revision(&legacy_key, &q.page)?;
            revision.get() as i64
        }
        Some(HistoryCursor::Indexed { high_water, .. }) => *high_water,
        None => snapshot.latest,
    };
    q.page.validate()?;
    if matches!(cursor, Some(HistoryCursor::Legacy(_))) {
        return Ok(HistoryRead::Legacy {
            runs: snapshot.runs.clone(),
            high_water,
            key: legacy_key,
            reason: "legacy_cursor",
        });
    }
    let direction = match q.direction {
        workbench_protocol::queries::HistoryDirection::Forward => Direction::Forward,
        workbench_protocol::queries::HistoryDirection::Backward => Direction::Backward,
    };
    let prepared = snapshot
        .prepare(high_water, direction)
        .map_err(storage_error)?;
    let prepared = match prepared {
        Availability::Indexed(prepared) => prepared,
        Availability::Legacy(reason) => {
            if cursor.is_some() {
                return Err(error(
                    ErrorCode::Conflict,
                    "history index is no longer available",
                ));
            }
            return Ok(HistoryRead::Legacy {
                runs: snapshot.runs.clone(),
                high_water,
                key: legacy_key,
                reason,
            });
        }
    };
    let boundary = match &cursor {
        Some(HistoryCursor::Indexed {
            boundary, freeze, ..
        }) => {
            if *freeze != prepared.freeze {
                return Err(error(ErrorCode::Conflict, "history snapshot changed"));
            }
            Some(boundary)
        }
        _ => None,
    };
    let page = prepared
        .page(
            direction,
            boundary,
            q.page.limit as usize,
            super::PAGE_BYTES,
        )
        .map_err(storage_error)?;
    if page.oversized {
        return Err(error(
            ErrorCode::InvalidRequest,
            "one view exceeds page byte limit",
        ));
    }
    Ok(HistoryRead::Indexed {
        page,
        high_water,
        freeze: prepared.freeze,
    })
}

#[cfg(test)]
fn committed_row(
    kind: &EventKind,
    tokenized: bool,
    start: Option<storage::history::ToolInput>,
) -> Option<storage::history::Committed<'_>> {
    storage::history::rows::committed(kind, tokenized, start)
}

fn message_role(role: storage::history::Role) -> MessageRole {
    use storage::history::Role;
    match role {
        Role::User => MessageRole::User,
        Role::Assistant => MessageRole::Assistant,
        Role::Tool => MessageRole::Tool,
        Role::Reasoning => MessageRole::Reasoning,
        Role::Notice => MessageRole::Notice,
        Role::Error => MessageRole::Error,
    }
}

fn storage_role(role: MessageRole) -> storage::history::Role {
    use storage::history::Role;
    match role {
        MessageRole::User => Role::User,
        MessageRole::Assistant => Role::Assistant,
        MessageRole::Tool => Role::Tool,
        MessageRole::Reasoning => Role::Reasoning,
        MessageRole::Notice => Role::Notice,
        MessageRole::Error => Role::Error,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<storage::history::ToolRow>,
}
#[derive(Clone, serde::Serialize)]
struct InputBody {
    id: String,
    bytes: u64,
}
impl From<storage::history::read::Descriptor> for RawMessage {
    fn from(value: storage::history::read::Descriptor) -> Self {
        Self {
            run: value.run,
            index: value.index,
            role: message_role(value.role),
            parts: value.parts,
            input: value.input.map(|i| InputBody {
                id: i.id,
                bytes: i.bytes,
            }),
            tool: value.tool,
        }
    }
}
type PendingParts = storage::history::PendingParts;
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
    reducer: storage::history::Reducer,
    parts: std::collections::BTreeMap<String, PendingParts>,
    merge_max: i64,
    last_id: i64,
    consumed: std::collections::BTreeMap<String, (i64, bool)>,
    /// Tool calls started and not yet completed, with the id of the event that started them;
    /// the input is read from that event when the completion is projected.
    starts: std::collections::BTreeMap<String, i64>,
    chain: Digest,
}
impl RunFold {
    fn new(run: &str) -> Self {
        Self {
            run: run.to_owned(),
            reducer: Default::default(),
            parts: Default::default(),
            merge_max: i64::MIN,
            last_id: 0,
            consumed: Default::default(),
            starts: Default::default(),
            chain: [0; 32],
        }
    }
    /// An input goes ahead of this run's first committed row stamped at or after its
    /// acceptance, ties broken by prompt id.
    fn row(
        &mut self,
        db: &storage::Database,
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
            db,
            row.id,
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
    fn raw(&self, message: storage::history::Message) -> RawMessage {
        RawMessage {
            run: self.run.clone(),
            index: message.index,
            role: message_role(message.role),
            parts: message.parts,
            input: None,
            tool: message.tool,
        }
    }
    fn observe(
        &mut self,
        db: &storage::Database,
        id: i64,
        kind: &EventKind,
        tokenized: bool,
    ) -> Option<RawMessage> {
        let response = match kind {
            EventKind::Message(MessageEvent::MessagePart { response_id, .. }) => {
                Some(response_id.clone())
            }
            _ => None,
        };
        let mut pending = response.as_ref().and_then(|id| self.parts.remove(id));
        let mut start = None;
        if let Some((_, call_id, input)) = storage::history::rows::tool_start(kind, tokenized) {
            // Only a start with arguments can give a completion its input.
            if input.is_some() {
                self.starts.insert(call_id.to_owned(), id);
            } else {
                self.starts.remove(call_id);
            }
        } else if let EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
            call_id,
            run_id: Some(_),
            ..
        }) = kind
        {
            // A start that is gone or unreadable leaves the input unknown.
            start = self
                .starts
                .remove(call_id)
                .and_then(|id| db.tool_start_input(id).ok().flatten());
        }
        let message = self.reducer.observe(kind, tokenized, &mut pending, start);
        if let Some((id, pending)) = response.zip(pending) {
            self.parts.insert(id, pending);
        }
        message.map(|message| self.raw(message))
    }
    fn input(&mut self, input: storage::host_input::DeliveredHostInput) -> Option<RawMessage> {
        if input.first_in_run {
            self.reducer.initial_receipt = true;
        }
        if let Some(value) = input.text {
            return self.message(MessageRole::User, vec![(value, false)], false, false);
        }
        let raw = RawMessage {
            run: self.run.clone(),
            index: self.reducer.next,
            role: MessageRole::User,
            parts: vec![],
            input: Some(InputBody {
                id: input.prompt_id,
                bytes: input.text_bytes,
            }),
            tool: None,
        };
        self.reducer.next += 1;
        Some(raw)
    }
    fn message(
        &mut self,
        role: MessageRole,
        parts: Vec<(String, bool)>,
        prompt: bool,
        finish: bool,
    ) -> Option<RawMessage> {
        let role = storage_role(role);
        self.reducer
            .message(role, parts, prompt, finish)
            .map(|message| self.raw(message))
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
        storage::DeferredRow::Event(row) => fold.row(db, &row, &mut inputs, &mut emit),
    })?;
    match failure {
        Some(e) => Err(e),
        None => Ok((fold, inputs)),
    }
}

const ANCHORS: usize = 8;
const INLINE_INPUT_BYTES: u64 = 16 * 1024;

type Header = storage::host_input::DeliveredHostInputHeader;
type Input = storage::host_input::DeliveredHostInput;
/// A message count and the prefix digest after that many messages.
type Shown = (u64, Digest);

// Keep the scanner's run state inline instead of adding an allocation on resync.
#[allow(clippy::large_enum_variant)]
enum Slot {
    Cold(Vec<Header>),
    Hot { fold: RunFold, published: Shown },
}

#[allow(clippy::large_enum_variant)]
enum Next {
    Hot(Option<RunFold>, Shown),
    Cold(Option<Vec<Header>>),
}

/// Only fold state, counts and a chained prefix digest are retained, not the historical bodies.
#[derive(Default)]
pub(crate) struct CommitScanner {
    cut: Option<i64>,
    // While a committed row is stored unchanged every later row has a larger id.
    anchors: VecDeque<(i64, storage::EventStamp)>,
    slots: std::collections::BTreeMap<String, Slot>,
}

struct Step {
    cut: i64,
    rewritten: bool,
    anchors: VecDeque<(i64, storage::EventStamp)>,
    slots: std::collections::BTreeMap<String, Next>,
    fresh: Vec<(Order, RawMessage)>,
}

#[derive(Default)]
struct Collector {
    published: std::collections::BTreeMap<String, Shown>,
    prefixes: std::collections::BTreeMap<String, Digest>,
    fresh: Vec<(Order, RawMessage)>,
}
impl Collector {
    fn resume(&mut self, run: &str, published: Shown, start: Shown) {
        self.published.insert(run.to_owned(), published);
        if published.0 == start.0 {
            self.prefixes.insert(run.to_owned(), start.1);
        }
    }
    fn emit(&mut self, order: Order, message: RawMessage, chain: &Digest) {
        let count = self.published.get(&message.run).map_or(0, |(n, _)| *n);
        if message.index + 1 == count {
            self.prefixes.insert(message.run.clone(), *chain);
        }
        if message.index >= count {
            self.fresh.push((order, message));
        }
    }
}

fn with_texts<'a>(
    db: &storage::Database,
    headers: std::collections::BTreeMap<&'a str, Vec<Header>>,
) -> Result<std::collections::BTreeMap<&'a str, VecDeque<Input>>, storage::StorageError> {
    let wanted: Vec<String> = headers
        .values()
        .flatten()
        .filter(|h| h.text_bytes <= INLINE_INPUT_BYTES)
        .map(|h| h.prompt_id.clone())
        .collect();
    let mut texts: std::collections::HashMap<_, _> = if wanted.is_empty() {
        Default::default()
    } else {
        db.delivered_host_input_texts(&wanted)?
            .into_iter()
            .collect()
    };
    Ok(headers
        .into_iter()
        .map(|(run, inputs)| {
            let inputs = inputs.into_iter().map(|h| Input {
                text: if h.text_bytes <= INLINE_INPUT_BYTES {
                    texts.remove(&h.prompt_id).flatten()
                } else {
                    None
                },
                prompt_id: h.prompt_id,
                run_id: h.run_id,
                text_bytes: h.text_bytes,
                accepted_at_ns: h.accepted_at_ns,
                first_in_run: h.first_in_run,
            });
            (run, inputs.collect())
        })
        .collect())
}

fn tentative(
    fold: &RunFold,
    rest: VecDeque<Input>,
    emit: &mut impl FnMut(Order, RawMessage, &Digest),
) -> Shown {
    if rest.is_empty() {
        return (fold.reducer.next, fold.chain);
    }
    let mut tail = fold.clone();
    tail.trailing(rest, emit);
    (tail.reducer.next, tail.chain)
}

/// Emits nothing: clients read what precedes the cut through ReadCommittedHistory.
fn hydrate(
    db: &storage::Database,
    run: &str,
    cut: i64,
    baseline: VecDeque<Input>,
) -> Result<(RunFold, VecDeque<Input>, Shown), storage::StorageError> {
    let (fold, rest) = fold_run(db, run, cut, baseline.into(), |_, _, _| {})?;
    let published = tentative(&fold, rest.clone(), &mut |_, _, _| {});
    Ok((fold, rest, published))
}

impl CommitScanner {
    pub(crate) fn initialized(&self) -> bool {
        self.cut.is_some()
    }

    fn live_anchors(
        &self,
        db: &storage::Database,
    ) -> Result<VecDeque<(i64, storage::EventStamp)>, storage::StorageError> {
        if self.anchors.is_empty() {
            return Ok(VecDeque::new());
        }
        let ids: Vec<i64> = self.anchors.iter().map(|(id, _)| *id).collect();
        let stored: std::collections::HashMap<_, _> = db.event_stamps(&ids)?.into_iter().collect();
        Ok(self
            .anchors
            .iter()
            .filter(|(id, stamp)| stored.get(id) == Some(stamp))
            .cloned()
            .collect())
    }

    fn hot(&self, run: &str) -> &RunFold {
        match &self.slots[run] {
            Slot::Hot { fold, .. } => fold,
            Slot::Cold(_) => unreachable!("a Cold run is hydrated before it is folded"),
        }
    }

    fn baseline(&self, run: &str) -> &[Header] {
        match &self.slots[run] {
            Slot::Cold(baseline) => baseline,
            Slot::Hot { .. } => unreachable!("only a Cold run has a baseline"),
        }
    }

    fn advance(
        &self,
        db: &storage::Database,
        runs: &[String],
    ) -> Result<Step, storage::StorageError> {
        use std::collections::{BTreeMap, BTreeSet, HashSet};
        let through = db.last_event_id()?;
        let anchors = self.live_anchors(db)?;
        // With every anchor gone a row may have been written below the last window:
        // cut again and let clients resynchronize.
        let recut = self.cut.is_none() || (anchors.is_empty() && !self.anchors.is_empty());
        // Cut before reading receipts, so a delivery in between joins the baseline.
        let cut_row = if recut { db.max_committed_id()? } else { None };
        let mut headers = BTreeMap::<String, Vec<Header>>::new();
        for header in db.delivered_host_input_headers()? {
            headers
                .entry(header.run_id.clone())
                .or_default()
                .push(header);
        }
        for inputs in headers.values_mut() {
            inputs.sort_by(|a, b| {
                (a.accepted_at_ns, &a.prompt_id).cmp(&(b.accepted_at_ns, &b.prompt_id))
            });
        }
        if recut {
            return Ok(Step {
                cut: cut_row.as_ref().map_or(0, |(id, _)| *id),
                rewritten: self.cut.is_some(),
                anchors: cut_row.into_iter().collect(),
                slots: runs
                    .iter()
                    .map(|run| {
                        let baseline = headers.remove(run).unwrap_or_default();
                        (run.clone(), Next::Cold(Some(baseline)))
                    })
                    .collect(),
                fresh: Vec::new(),
            });
        }
        let cut = self.cut.unwrap_or_default();
        let floor = anchors.back().map_or(0, |(id, _)| *id);

        let mut cold = BTreeSet::<&str>::new();
        let mut thawing = BTreeMap::<&str, Vec<Header>>::new();
        let mut listed = BTreeMap::<&str, Vec<Header>>::new();
        for run in runs {
            let inputs = headers.remove(run).unwrap_or_default();
            if let Some(Slot::Cold(baseline)) = self.slots.get(run) {
                if *baseline == inputs {
                    cold.insert(run);
                    continue;
                }
                thawing.insert(run, baseline.clone());
            }
            listed.insert(run, inputs);
        }
        let mut thawed = BTreeMap::<&str, (RunFold, Shown)>::new();
        for (run, baseline) in with_texts(db, thawing)? {
            let (fold, _, published) = hydrate(db, run, cut, baseline)?;
            thawed.insert(run, (fold, published));
        }

        let mut collector = Collector::default();
        let mut rebuild = BTreeSet::<&str>::new();
        let mut pending = BTreeMap::<&str, Vec<Header>>::new();
        for (run, mut inputs) in listed {
            let slot = match self.slots.get(run) {
                Some(Slot::Hot { fold, published }) => Some((fold, *published)),
                _ => thawed.get(run).map(|(fold, published)| (fold, *published)),
            };
            let shown = slot.map_or((0, [0; 32]), |(_, published)| published);
            let start = match slot {
                // A delivery at or before rows already folded, or a change to one already
                // merged, can only be placed by folding the run again.
                Some((fold, _))
                    if inputs
                        .iter()
                        .filter(|h| h.accepted_at_ns <= fold.merge_max)
                        .map(|h| (h.prompt_id.clone(), (h.accepted_at_ns, h.first_in_run)))
                        .collect::<BTreeMap<_, _>>()
                        == fold.consumed =>
                {
                    inputs.retain(|h| h.accepted_at_ns > fold.merge_max);
                    (fold.reducer.next, fold.chain)
                }
                _ => {
                    rebuild.insert(run);
                    (0, [0; 32])
                }
            };
            collector.resume(run, shown, start);
            pending.insert(run, inputs);
        }
        let mut pending = with_texts(db, pending)?;

        let ranged: HashSet<&str> = runs
            .iter()
            .map(String::as_str)
            .filter(|run| floor == 0 || !rebuild.contains(run))
            .collect();
        let mut staged = BTreeMap::<&str, RunFold>::new();
        for (run, (fold, _)) in thawed {
            if !rebuild.contains(run) {
                staged.insert(run, fold);
            }
        }
        for run in &ranged {
            if rebuild.contains(run) {
                staged.insert(run, RunFold::new(run));
            }
        }
        let mut woken = BTreeMap::<&str, Vec<Box<storage::DeferredStoredEvent>>>::new();
        let mut failure = None;
        let newest =
            db.for_each_committed_event_after(&ranged, floor, through, |row| match row {
                storage::DeferredRow::Failed(_) => failure = Some(unreadable()),
                storage::DeferredRow::Event(row) => {
                    let Some(run) = row
                        .run_id
                        .as_deref()
                        .or_else(|| row.event.kind.primary_run_id())
                        .and_then(|run| ranged.get(run).copied())
                    else {
                        return;
                    };
                    if cold.contains(run) {
                        woken.entry(run).or_default().push(row);
                        return;
                    }
                    let folded = staged
                        .get(run)
                        .map_or_else(|| self.hot(run).last_id, |fold| fold.last_id);
                    if row.id <= folded {
                        return;
                    }
                    let fold = staged.entry(run).or_insert_with(|| self.hot(run).clone());
                    fold.row(db, &row, pending.get_mut(run).unwrap(), &mut |o, m, c| {
                        collector.emit(o, m, c)
                    });
                }
            })?;
        if let Some(e) = failure {
            return Err(e);
        }
        // Every window since the cut started at or above it, so a Cold run has no rows
        // between the cut and this window: fold it up to the cut, then on.
        let baselines = woken
            .keys()
            .map(|run| (*run, self.baseline(run).to_vec()))
            .collect();
        for (run, baseline) in with_texts(db, baselines)? {
            let (mut fold, mut rest, published) = hydrate(db, run, cut, baseline)?;
            collector.resume(run, published, (fold.reducer.next, fold.chain));
            for row in woken.remove(run).unwrap_or_default() {
                if row.id > fold.last_id {
                    fold.row(db, &row, &mut rest, &mut |o, m, c| collector.emit(o, m, c));
                }
            }
            cold.remove(run);
            staged.insert(run, fold);
            pending.insert(run, rest);
        }
        for run in rebuild.iter().filter(|run| !ranged.contains(*run)) {
            let inputs = pending.remove(run).unwrap_or_default().into();
            let (fold, rest) = fold_run(db, run, through, inputs, |o, m, c: &Digest| {
                collector.emit(o, m, c)
            })?;
            staged.insert(run, fold);
            pending.insert(run, rest);
        }
        let mut slots = BTreeMap::new();
        let mut rewritten = false;
        for run in runs {
            if cold.contains(run.as_str()) {
                slots.insert(run.clone(), Next::Cold(None));
                continue;
            }
            let base = staged.get(run.as_str()).unwrap_or_else(|| self.hot(run));
            let rest = pending.remove(run.as_str()).unwrap_or_default();
            let shown = tentative(base, rest, &mut |o, m, c| collector.emit(o, m, c));
            // Delivered may arrive after messages following its acceptance time were
            // published. Never silently renumber their append-only indices.
            let (count, chain) = collector.published[run];
            rewritten |= shown.0 < count || collector.prefixes.get(run) != Some(&chain);
            slots.insert(run.clone(), Next::Hot(staged.remove(run.as_str()), shown));
        }
        let mut fresh = collector.fresh;
        fresh.sort_by(|a, b| a.0.cmp(&b.0));
        let mut anchors = anchors;
        if let Some(newest) = newest {
            anchors.push_back(newest);
            while anchors.len() > ANCHORS {
                anchors.pop_front();
            }
        }
        Ok(Step {
            cut,
            rewritten,
            anchors,
            slots,
            fresh,
        })
    }

    fn commit(&mut self, step: Step) -> bool {
        let Step {
            cut,
            rewritten,
            anchors,
            slots,
            ..
        } = step;
        let mut old = std::mem::take(&mut self.slots);
        self.slots = slots
            .into_iter()
            .map(|(run, next)| {
                let slot = match (next, old.remove(&run)) {
                    (Next::Hot(Some(fold), published), _)
                    | (Next::Hot(None, published), Some(Slot::Hot { fold, .. })) => {
                        Slot::Hot { fold, published }
                    }
                    (Next::Cold(Some(baseline)), _)
                    | (Next::Cold(None), Some(Slot::Cold(baseline))) => Slot::Cold(baseline),
                    _ => unreachable!("every listed run is staged or retained"),
                };
                (run, slot)
            })
            .collect();
        self.cut = Some(cut);
        self.anchors = anchors;
        rewritten
    }
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
mod paging_tests;
#[cfg(test)]
mod tests;
