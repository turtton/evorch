use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use event_bus::{AgentRunPhase, EventBus, EventKind, EventMeta, ToolEvent};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunStore, RuntimeError,
};
use serde_json::Value;
use storage::{StorageConfig, StorageHandle};

use super::corpus::{Case, HitSet, hash};
use super::{DEADLINE, Result, require};

pub struct Sample {
    pub pages_us: Vec<u64>,
    pub drain_us: u64,
    pub partial_pages: usize,
    pub max_bytes: usize,
    pub hash: String,
    pub started: Instant,
    pub finished: Instant,
    hits: HitSet,
    pub boundary_us: Vec<(u64, u64, u64)>,
    boundaries: Vec<std::time::Duration>,
    pub page_shapes: Vec<Value>,
    pub page_threads: Vec<Vec<String>>,
}

struct Session {
    args: Value,
    started: Option<Instant>,
    page_start: Option<Instant>,
    pages_us: Vec<u64>,
    hits: HitSet,
    cursors: BTreeSet<String>,
    output_hashes: Vec<String>,
    partial_pages: usize,
    max_bytes: usize,
    done: mpsc::Sender<Result<Sample>>,
    trigger: Option<mpsc::Sender<Instant>>,
    boundaries: Vec<std::time::Duration>,
    page_shapes: Vec<Value>,
    page_threads: Vec<Vec<String>>,
}

#[derive(Default)]
struct Script {
    session: Mutex<Option<Session>>,
    trigger: Mutex<Option<mpsc::Sender<Instant>>>,
}

fn response(block: ContentBlock, reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content: vec![block],
        },
        finish_reason: reason,
        usage: Usage::default(),
    }
}

fn stop() -> ChatResponse {
    response(
        ContentBlock::Text {
            text: "benchmark complete".into(),
        },
        FinishReason::Stop,
    )
}

fn next(session: &mut Session) -> ChatResponse {
    let response = response(
        ContentBlock::ToolUse {
            id: format!("s8-page-{}", session.pages_us.len()),
            name: "thread_search".into(),
            input: session.args.clone(),
        },
        FinishReason::ToolUse,
    );
    let now = Instant::now();
    session.boundaries.push(EventMeta::now().monotonic);
    if session.started.is_none() {
        session.started = Some(now);
        if let Some(trigger) = session.trigger.take() {
            let _ = trigger.send(now);
        }
    }
    session.page_start = Some(now);
    response
}

fn consume(
    session: &mut Session,
    messages: &[Message],
    end: Instant,
    end_meta: std::time::Duration,
) -> Result<Option<Sample>> {
    session.boundaries.push(end_meta);
    session.pages_us.push(
        end.duration_since(session.page_start.take().ok_or("missing_timer")?)
            .as_micros() as u64,
    );
    let (parts, is_error) = messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter().rev())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content, *is_error)),
            _ => None,
        })
        .ok_or("missing_tool_result")?;
    require(!is_error, "runtime_tool_error")?;
    let text = parts
        .iter()
        .map(|part| part.text_or_image_note().into_owned())
        .collect::<String>();
    require(text.len() <= 16_384, "output_bound_exceeded")?;
    require(
        secret_guard::SecretRedactor::from_env().redact(&text).text == text,
        "output_not_redacted",
    )?;
    session.max_bytes = session.max_bytes.max(text.len());
    session.output_hashes.push(hash(text.as_bytes()));
    let value: Value = serde_json::from_str(&text).map_err(|_| "invalid_tool_json")?;
    require(value["note"].is_string(), "missing_untrusted_note")?;
    let threads = value["threads"].as_array().ok_or("missing_threads")?;
    session.page_shapes.push(serde_json::json!({
        "threads_returned":threads.len(),"hits_returned":threads.iter()
            .filter_map(|thread|thread["hit_count"].as_u64()).sum::<u64>(),
        "index":value["index"],"partial":value["partial"],
        "has_cursor":value["next_cursor"].is_string(),"output_bytes":text.len(),
    }));
    session.page_threads.push(
        threads
            .iter()
            .filter_map(|thread| thread["thread_id"].as_str().map(str::to_owned))
            .collect(),
    );
    for thread in threads {
        let id = thread["thread_id"].as_str().ok_or("missing_thread_id")?;
        let count = thread["hit_count"].as_u64().ok_or("missing_hit_count")?;
        *session.hits.entry(id.to_owned()).or_default() += count;
    }
    let cursor = value["next_cursor"].as_str().map(str::to_owned);
    if value["partial"] == true {
        session.partial_pages += 1;
        require(cursor.is_some(), "partial_without_cursor")?;
    }
    if let Some(cursor) = cursor {
        require(session.cursors.insert(cursor.clone()), "cursor_cycle")?;
        require(session.pages_us.len() < 2_000, "cursor_drain_limit")?;
        session.args["cursor"] = Value::String(cursor);
        return Ok(None);
    }
    let started = session.started.ok_or("missing_drain_timer")?;
    Ok(Some(Sample {
        pages_us: std::mem::take(&mut session.pages_us),
        drain_us: end.duration_since(started).as_micros() as u64,
        partial_pages: session.partial_pages,
        max_bytes: session.max_bytes,
        hash: hash(&serde_json::to_vec(&session.output_hashes)?),
        started,
        finished: end,
        hits: std::mem::take(&mut session.hits),
        boundary_us: Vec::new(),
        boundaries: std::mem::take(&mut session.boundaries),
        page_shapes: std::mem::take(&mut session.page_shapes),
        page_threads: std::mem::take(&mut session.page_threads),
    }))
}

#[async_trait::async_trait]
impl AgentModel for Script {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> std::result::Result<ChatResponse, RuntimeError> {
        // Capture before parsing/assertions; no model/network wait is involved.
        let end = Instant::now();
        let end_meta = EventMeta::now().monotonic;
        let mut guard = self.session.lock().unwrap();
        let Some(session) = guard.as_mut() else {
            return Ok(stop());
        };
        if session.page_start.is_none() {
            return Ok(next(session));
        }
        match consume(session, messages, end, end_meta) {
            Ok(None) => Ok(next(session)),
            Ok(Some(sample)) => {
                let session = guard.take().unwrap();
                let _ = session.done.send(Ok(sample));
                Ok(stop())
            }
            Err(error) => {
                let session = guard.take().unwrap();
                let _ = session.done.send(Err(error));
                Ok(stop())
            }
        }
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "s8-zero-wait-script".into()
    }
}

pub struct Harness {
    runtime: AgentRuntime,
    script: Arc<Script>,
    events: Mutex<Option<event_bus::EventReceiver>>,
}

impl Harness {
    pub fn new(config: &StorageConfig, handle: StorageHandle) -> Result<Self> {
        let bus = Arc::new(EventBus::new(8192));
        let script = Arc::new(Script::default());
        let events = Mutex::new(Some(bus.subscribe()));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            script.clone(),
        )
        .with_run_store(RunStore::open(config, handle)?);
        Ok(Self {
            runtime,
            script,
            events,
        })
    }

    pub fn arm_backfill(&self, trigger: mpsc::Sender<Instant>) {
        *self.script.trigger.lock().unwrap() = Some(trigger);
    }

    pub async fn search(&self, project: &Path, case: &Case, expected: &HitSet) -> Result<Sample> {
        let (done, receive) = mpsc::channel();
        *self.script.session.lock().unwrap() = Some(Session {
            args: case.args(),
            started: None,
            page_start: None,
            pages_us: Vec::new(),
            hits: HitSet::new(),
            cursors: BTreeSet::new(),
            output_hashes: Vec::new(),
            partial_pages: 0,
            max_bytes: 0,
            done,
            trigger: self.script.trigger.lock().unwrap().take(),
            boundaries: Vec::new(),
            page_shapes: Vec::new(),
            page_threads: Vec::new(),
        });
        // A normal unbound Worker root has trusted project authority but is not
        // a conversation thread, so its saved benchmark history is never a hit.
        let run = self.runtime.delegate_background(
            Role::Worker,
            "Run the benchmark search.".into(),
            RunConfig {
                name: Some("s8-unbound-caller".into()),
                project_root: Some(project.to_owned()),
                ..Default::default()
            },
        );
        let phase = tokio::time::timeout(DEADLINE, self.runtime.wait(run)).await;
        if phase.is_err() {
            self.runtime.cancel_subtree(run)?;
            return Err("runtime_deadline".into());
        }
        require(phase.unwrap()? == AgentRunPhase::Done, "runtime_not_done")?;
        let mut sample = receive
            .recv_timeout(DEADLINE)
            .map_err(|_| "missing_sample")??;
        // Check identities and hit counts, not just matching timing or nonempty
        // snippets. The independent oracle never reads conversation index rows.
        require(&sample.hits == expected, "canonical_hit_set_mismatch")?;
        let mut starts = std::collections::BTreeMap::new();
        let mut ends = std::collections::BTreeMap::new();
        // Only timestamps survive; event input/output payloads are discarded.
        let mut receiver = self
            .events
            .lock()
            .unwrap()
            .take()
            .ok_or("event_receiver_busy")?;
        while let Ok(Ok(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(1), receiver.recv()).await
        {
            match event.kind {
                EventKind::Tool(ToolEvent::ToolStarted {
                    call_id, tool_name, ..
                }) if tool_name == "thread_search" => {
                    starts.insert(call_id, event.meta.monotonic);
                }
                EventKind::Tool(ToolEvent::ToolCompleted {
                    call_id, tool_name, ..
                }) if tool_name == "thread_search" => {
                    ends.insert(call_id, event.meta.monotonic);
                }
                _ => {}
            }
        }
        *self.events.lock().unwrap() = Some(receiver);
        // Runtime lifecycle timestamps isolate dispatch from the inclusive E2E
        // timer without moving its boundary or disabling persistence.
        for page in 0..sample.pages_us.len() {
            let id = format!("s8-page-{page}");
            if let (Some(start), Some(end)) = (starts.get(&id), ends.get(&id)) {
                let dispatch = end.saturating_sub(*start).as_micros() as u64;
                let before = start
                    .saturating_sub(sample.boundaries[page * 2])
                    .as_micros() as u64;
                let after = sample.boundaries[page * 2 + 1]
                    .saturating_sub(*end)
                    .as_micros() as u64;
                sample.boundary_us.push((before, dispatch, after));
            }
        }
        Ok(sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::ToolResultContent;
    use serde_json::json;

    fn session() -> Session {
        let (done, _) = mpsc::channel();
        let now = Instant::now();
        Session {
            args: json!({}),
            started: Some(now),
            page_start: Some(now),
            pages_us: vec![],
            hits: HitSet::new(),
            cursors: BTreeSet::new(),
            output_hashes: vec![],
            partial_pages: 0,
            max_bytes: 0,
            done,
            trigger: None,
            boundaries: vec![EventMeta::now().monotonic],
            page_shapes: Vec::new(),
            page_threads: Vec::new(),
        }
    }

    fn result(value: Value) -> Vec<Message> {
        vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "fixture".into(),
                is_error: false,
                content: vec![ToolResultContent::Text {
                    text: value.to_string(),
                }],
            }],
        }]
    }

    #[test]
    fn partial_without_cursor_and_cursor_cycles_fail_closed() {
        let mut state = session();
        let bad = result(json!({"note":"untrusted","threads":[],"partial":true}));
        assert!(consume(&mut state, &bad, Instant::now(), EventMeta::now().monotonic).is_err());
        let mut state = session();
        state.cursors.insert("fixturecursor".into());
        let bad = result(json!({"note":"untrusted","threads":[],"partial":true,
            "next_cursor":"fixturecursor"}));
        assert!(consume(&mut state, &bad, Instant::now(), EventMeta::now().monotonic).is_err());
    }
}
