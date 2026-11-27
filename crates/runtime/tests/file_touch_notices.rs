//! A run that read a file hears, at its next loop head, that another thread changed it.
//! The notice never starts a request by itself, never replaces a final result, and
//! leaves the turn and the already-sent history alone.

mod support;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::{
    AgentMessageEvent, AgentMessageKind, AgentRunPhase, DeliveryDisposition, Event, EventBus,
    EventKind, EventReceiver, LifecycleEvent, MessageEvent,
};
use providers::{ChatResponse, FinishReason, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RuntimeError,
};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::sync::Notify;
use tools::ToolExecutor;

const NOTICE_MARKER: &str = "id=file-touch-";

/// Holds one model response until the test releases it.
#[derive(Default)]
struct Gate {
    arrived: Notify,
    release: Notify,
}

enum Step {
    Respond(ChatResponse),
    Gated(Arc<Gate>, ChatResponse),
}

fn say(text: &str) -> Step {
    Step::Respond(support::text_response(text, FinishReason::Stop))
}

fn call(id: &str, name: &str, input: serde_json::Value) -> Step {
    Step::Respond(support::tool_response(id, name, input))
}

fn read(id: &str, path: &Path) -> Step {
    call(id, "read", json!({"path": path}))
}

fn edit(id: &str, path: &Path, old: &str, new: &str) -> Step {
    call(
        id,
        "edit",
        json!({"path": path, "old_string": old, "new_string": new}),
    )
}

fn gated(gate: &Arc<Gate>, step: Step) -> Step {
    match step {
        Step::Respond(response) => Step::Gated(Arc::clone(gate), response),
        Step::Gated(..) => panic!("already gated"),
    }
}

type Observed = (AgentInvocationContext, Vec<Message>);

/// Answers each run from its own script, so concurrently started runs cannot take each
/// other's responses, and records every request.
#[derive(Default)]
struct ScriptedModel {
    scripts: Mutex<HashMap<String, VecDeque<Step>>>,
    observed: Mutex<Vec<Observed>>,
}

impl ScriptedModel {
    fn script(&self, run: RunId, steps: Vec<Step>) {
        self.scripts
            .lock()
            .unwrap()
            .insert(run.to_string(), steps.into());
    }

    fn requests_of(&self, run: RunId) -> Vec<Observed> {
        let run = run.to_string();
        self.observed
            .lock()
            .unwrap()
            .iter()
            .filter(|(invocation, _)| invocation.run_id == run)
            .cloned()
            .collect()
    }

    fn unused(&self, run: RunId) -> usize {
        self.scripts
            .lock()
            .unwrap()
            .get(&run.to_string())
            .map_or(0, VecDeque::len)
    }
}

#[async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.observed
            .lock()
            .unwrap()
            .push((invocation.clone(), messages.to_vec()));
        let step = self
            .scripts
            .lock()
            .unwrap()
            .get_mut(&invocation.run_id)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| panic!("unscripted request of {}", invocation.run_id));
        match step {
            Step::Respond(response) => Ok(response),
            Step::Gated(gate, response) => {
                gate.arrived.notify_one();
                gate.release.notified().await;
                Ok(response)
            }
        }
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "file-touch-model".into()
    }
}

struct World {
    runtime: AgentRuntime,
    model: Arc<ScriptedModel>,
    receiver: EventReceiver,
    events: Vec<Event>,
    root: PathBuf,
    _directory: tempfile::TempDir,
    _storage: support::ReviewStorage,
}

impl World {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for (name, text) in [
            ("x.txt", "alpha\nbeta\n"),
            ("y.txt", "one\n"),
            ("z.txt", "two\n"),
        ] {
            std::fs::write(root.join(name), text).unwrap();
        }
        let bus = Arc::new(EventBus::new(1024));
        let receiver = bus.subscribe();
        let executor = Arc::new(ToolExecutor::with_standard_tools_in(
            bus.clone(),
            Arc::new(DirectSandbox::new_unchecked()),
            Some(root.clone()),
        ));
        let model = Arc::new(ScriptedModel::default());
        let runtime = AgentRuntime::new(bus, executor, model.clone()).with_project_rules(Arc::new(
            runtime::RulesSource::new(
                runtime::ProjectTrust::Approved,
                runtime::RulesSettings {
                    context_window_tokens: 1_000_000,
                    response_headroom_tokens: 16_384,
                    max_injection_bytes: 65_536,
                },
                None,
                Some(root.clone()),
                None,
            ),
        ));
        runtime.set_project_root(root.clone()).unwrap();
        let storage = support::ReviewStorage::new();
        storage.attach(&runtime);
        Self {
            runtime,
            model,
            receiver,
            events: Vec::new(),
            root,
            _directory: directory,
            _storage: storage,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Starts a root run owning `thread`, with its own script.
    fn start(
        &self,
        thread: &str,
        role: Role,
        interactive: bool,
        prompt: &str,
        steps: Vec<Step>,
    ) -> RunId {
        let run = self.runtime.reserve_run_id();
        self.model.script(run, steps);
        self.runtime.bind_thread_root(thread, run).unwrap();
        self.runtime
            .spawn_reserved(
                run,
                None,
                role,
                prompt,
                RunConfig {
                    interactive,
                    keep_alive: interactive,
                    ..Default::default()
                },
            )
            .unwrap();
        run
    }

    async fn until(&mut self, run: RunId, phase: AgentRunPhase) {
        loop {
            let event = self.receiver.recv().await.unwrap();
            let reached = match &event.kind {
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id, to, ..
                }) if run_id == &run.to_string() => {
                    if matches!(
                        to,
                        AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                    ) {
                        assert_eq!(*to, phase, "unexpected end of {run}");
                    }
                    *to == phase
                }
                _ => false,
            };
            self.events.push(event);
            if reached {
                return;
            }
        }
    }

    /// The notices delivered to `run`, as the events report them.
    fn delivered_to(&self, run: RunId) -> Vec<(event_bus::AgentMessage, DeliveryDisposition)> {
        self.events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::AgentMessage(AgentMessageEvent::Delivered {
                    message,
                    disposition,
                }) if message.recipient_run_id == run.to_string()
                    && message.sender_run_id == "runtime" =>
                {
                    Some((message.clone(), disposition.clone()))
                }
                _ => None,
            })
            .collect()
    }
}

fn history(request: &Observed) -> String {
    serde_json::to_string(&request.1).unwrap()
}

fn notices(request: &Observed) -> usize {
    history(request).matches(NOTICE_MARKER).count()
}

fn final_results(world: &World, run: RunId) -> Vec<String> {
    world
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Message(MessageEvent::FinalResultPublished { run_id, text })
                if run_id == &run.to_string() =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

/// Starts reader `B`, which reads `x.txt` and then waits at a gate; lets `A` edit
/// `x.txt` twice; and returns once `A` is done. `B` is released by the caller.
async fn reader_then_editor(
    world: &mut World,
    role: Role,
    interactive: bool,
    after_gate: Step,
    rest: Vec<Step>,
) -> (RunId, RunId, Arc<Gate>) {
    let gate = Arc::new(Gate::default());
    let x = world.path("x.txt");
    let mut steps = vec![read("read-x", &x), gated(&gate, after_gate)];
    steps.extend(rest);
    let reader = world.start("thread-b", role, interactive, "reader goal", steps);
    gate.arrived.notified().await;
    let editor = world.start(
        "thread-a",
        Role::Worker,
        false,
        "editor goal",
        vec![
            edit("edit-1", &x, "alpha", "alpha-a"),
            edit("edit-2", &x, "alpha-a", "alpha-aa"),
            say("editor done"),
        ],
    );
    world.until(editor, AgentRunPhase::Done).await;
    (reader, editor, gate)
}

#[tokio::test]
async fn file_touch_reader_gets_one_aggregated_notice_at_its_next_loop_head() {
    let mut world = World::new();
    let y = world.path("y.txt");
    let (reader, editor, gate) = reader_then_editor(
        &mut world,
        Role::Worker,
        false,
        read("read-y", &y),
        vec![say("reader done")],
    )
    .await;
    gate.release.notify_one();
    world.until(reader, AgentRunPhase::Done).await;

    let requests = world.model.requests_of(reader);
    assert_eq!(requests.len(), 3);
    assert_eq!(world.model.unused(reader), 0);
    assert_eq!(notices(&requests[0]) + notices(&requests[1]), 0);
    assert_eq!(notices(&requests[2]), 1, "{}", history(&requests[2]));
    let text = history(&requests[2]);
    assert!(
        text.contains("x.txt") && text.contains("2 changes"),
        "{text}"
    );
    assert!(
        !text.contains("y.txt\\\":"),
        "only the changed file is named"
    );
    assert!(text.contains("kind=send") && text.contains("from=runtime"));

    // The sent history is a prefix of the next request, and the turn is untouched.
    assert_eq!(requests[2].1[..requests[1].1.len()], requests[1].1[..]);
    assert_eq!(requests[1].0.turn, requests[2].0.turn);

    let delivered = world.delivered_to(reader);
    assert_eq!(delivered.len(), 1);
    let (message, disposition) = &delivered[0];
    assert!(message.message_id.starts_with("file-touch-"));
    assert_eq!(message.kind, AgentMessageKind::Send);
    assert_eq!(message.reply_to, None);
    assert_eq!(*disposition, DeliveryDisposition::Aside);
    assert!(text.contains(&message.message_id));

    for request in world.model.requests_of(editor) {
        assert_eq!(notices(&request), 0, "the writer is never told");
    }
    assert!(world.delivered_to(editor).is_empty());
}

#[tokio::test]
async fn file_touch_own_edit_after_the_other_threads_keeps_the_notice_pending() {
    let mut world = World::new();
    let x = world.path("x.txt");
    let (reader, _editor, gate) = reader_then_editor(
        &mut world,
        Role::Worker,
        false,
        edit("own-edit", &x, "beta", "beta-b"),
        vec![say("reader done")],
    )
    .await;
    gate.release.notify_one();
    world.until(reader, AgentRunPhase::Done).await;

    let requests = world.model.requests_of(reader);
    assert_eq!(requests.len(), 3);
    assert_eq!(notices(&requests[1]), 0);
    assert_eq!(notices(&requests[2]), 1, "{}", history(&requests[2]));
    assert_eq!(world.delivered_to(reader).len(), 1);
    assert_eq!(
        std::fs::read_to_string(&x).unwrap(),
        "alpha-aa\nbeta-b\n",
        "both edits landed"
    );
}

#[tokio::test]
async fn file_touch_stop_with_a_pending_notice_adds_no_request_and_keeps_the_final_text() {
    let mut world = World::new();
    let (reader, _editor, gate) = reader_then_editor(
        &mut world,
        Role::Worker,
        false,
        say("reader final report"),
        Vec::new(),
    )
    .await;
    gate.release.notify_one();
    world.until(reader, AgentRunPhase::Done).await;
    world.runtime.wait(reader).await.unwrap();

    let requests = world.model.requests_of(reader);
    assert_eq!(requests.len(), 2, "the notice alone starts no request");
    assert!(requests.iter().all(|request| notices(request) == 0));
    assert_eq!(
        world.runtime.run_result(reader).unwrap().as_deref(),
        Some("reader final report")
    );
    assert!(world.delivered_to(reader).is_empty());
}

#[tokio::test]
async fn file_touch_finish_with_a_pending_notice_publishes_the_result_once() {
    let mut world = World::new();
    let (reader, _editor, gate) = reader_then_editor(
        &mut world,
        Role::Orchestrator,
        false,
        call("finish", "finish", json!({"result": "reader finished"})),
        Vec::new(),
    )
    .await;
    gate.release.notify_one();
    world.until(reader, AgentRunPhase::Done).await;
    world.runtime.wait(reader).await.unwrap();

    assert_eq!(world.model.requests_of(reader).len(), 2);
    assert_eq!(final_results(&world, reader), ["reader finished"]);
    assert_eq!(
        world.runtime.run_result(reader).unwrap().as_deref(),
        Some("reader finished")
    );
    assert!(world.delivered_to(reader).is_empty());
}

#[tokio::test]
async fn file_touch_idle_run_is_not_woken_and_hears_it_with_the_next_human_input() {
    let mut world = World::new();
    let x = world.path("x.txt");
    let gate = Arc::new(Gate::default());
    let reader = world.start(
        "thread-b",
        Role::Worker,
        true,
        "reader goal",
        vec![
            read("read-x", &x),
            gated(&gate, say("first report")),
            say("second report"),
        ],
    );
    gate.arrived.notified().await;
    let editor = world.start(
        "thread-a",
        Role::Worker,
        false,
        "editor goal",
        vec![edit("edit-1", &x, "alpha", "alpha-a"), say("editor done")],
    );
    world.until(editor, AgentRunPhase::Done).await;
    gate.release.notify_one();
    world.until(reader, AgentRunPhase::Waiting).await;
    assert_eq!(world.model.requests_of(reader).len(), 2);
    assert_eq!(world.model.unused(reader), 1);

    world
        .runtime
        .send_message(reader, "human follow-up".into())
        .unwrap();
    world.until(reader, AgentRunPhase::Waiting).await;

    let requests = world.model.requests_of(reader);
    let (before, after) = (&requests[1], &requests[2]);
    assert_eq!(notices(before), 0);
    assert_eq!(notices(after), 1, "{}", history(after));
    assert!(history(after).contains("human follow-up"));
    assert_eq!(
        after.0.turn.as_ref().unwrap().origin.as_deref(),
        Some("human")
    );
    assert_ne!(
        after.0.turn.as_ref().unwrap().prompt_id,
        requests[0].0.turn.as_ref().unwrap().prompt_id,
        "the human input began its own prompt"
    );
    assert_eq!(world.delivered_to(reader).len(), 1);
    assert_eq!(world.model.unused(reader), 0);
    assert_eq!(requests.len(), 3);

    world.runtime.cancel(reader).unwrap();
    world.runtime.wait(reader).await.unwrap();
}
