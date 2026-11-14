//! Lane notifications through the production core: the real `BootstrappedHost` wiring (owner,
//! storage, command service and its delivery host) behind `HostBackend`. No test binds a
//! delivery destination; whatever binds it is the bootstrap. Only the model is scripted.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use workbench_protocol::namespaces::{ConversationCommand, WorkspaceQuery, WorkspaceReply};

use event_bus::lane::LaneDeliveryBinding;
use event_bus::{AgentRunPhase, Event, EventKind, EventReceiver, LifecycleEvent, ToolEvent};
use evorch_core::backend::HostBackend;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::lane_delivery::LaneDeliveryOutcome;
use runtime::{AgentInvocationContext, AgentModel, Role, RunId, RuntimeError};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::commands::{Command, ComposerRole, SendChat};
use workbench_protocol::envelope::*;
use workbench_protocol::ids::*;
use workbench_protocol::queries::{Query, QueryReply};

const LEAD: &str = "lead";
const REPORT: &str = "core delivery report summary";
const ANSWER: &str = "core delivery lead answer";

/// A run with a scripted tool call makes it once, a run listed in `finishing` ends its turn
/// with `finish`, and every other turn only says it is idle and waits.
#[derive(Default)]
struct Model {
    finishing: Mutex<HashSet<String>>,
    scripted: Mutex<HashMap<String, ContentBlock>>,
    /// Every message each run was sent, serialized, in request order.
    texts: Mutex<HashMap<String, Vec<String>>>,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.texts
            .lock()
            .unwrap()
            .entry(invocation.run_id.clone())
            .or_default()
            .push(serde_json::to_string(messages).unwrap());
        let scripted = self.scripted.lock().unwrap().remove(&invocation.run_id);
        let finish = self.finishing.lock().unwrap().contains(&invocation.run_id);
        let tool = scripted.or_else(|| {
            finish.then(|| ContentBlock::ToolUse {
                id: format!("finish-{}", messages.len()),
                name: "finish".into(),
                input: serde_json::json!({"result":"complete"}),
            })
        });
        let tool_use = tool.is_some();
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![tool.unwrap_or(ContentBlock::Text {
                    text: "idle".into(),
                })],
            },
            usage: Default::default(),
            finish_reason: if tool_use {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "core-lane-delivery".into()
    }
}

/// The runs' sandbox is not what is under test, and bwrap is not available everywhere.
struct Unisolated;

impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

fn tool(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

struct Core {
    boot: BootstrappedHost,
    backend: HostBackend,
    model: Arc<Model>,
    events: EventReceiver,
    /// Questions the bus announced, in order.
    questions: Vec<event_bus::UserQuestion>,
    _dir: test_socket_dir::SplitDir,
}

impl Core {
    async fn open() -> Self {
        // Owner hosts bind a unix socket below the ownership root, so the path stays short.
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let model = Arc::new(Model::default());
        let boot = BootstrappedHost::open_with_test_seams(
            BootstrapOptions {
                config_dir: root.clone(),
                project_root: root.clone(),
                ownership_root: dir.sockets().to_path_buf(),
            },
            TestSeams {
                model: model.clone(),
                sandbox: Arc::new(Unisolated),
            },
        )
        .await
        .unwrap();
        let backend = HostBackend::new(boot.host.clone(), "core-lane-delivery".into());
        let events = boot.runtime.event_bus().subscribe();
        Self {
            boot,
            backend,
            model,
            events,
            questions: Vec::new(),
            _dir: dir,
        }
    }

    fn request(&self, id: &str, body: RequestBody) -> ReplyPayload {
        let reply = self.backend.request(
            RequestEnvelope {
                request_id: RequestId::new(id).unwrap(),
                session_id: SessionId::new("session").unwrap(),
                body,
            },
            Provenance::human(Surface::Cli, ClientId::new("client").unwrap()),
        );
        match reply.result {
            ReplyResult::Success(payload) => payload,
            ReplyResult::Error(error) => panic!("{id} failed: {error:?}"),
        }
    }

    /// Starts the lead conversation the way a client does, through the core backend.
    fn start_lead(&self) -> RunId {
        let ReplyPayload::Query(QueryReply::Workspace(WorkspaceReply::Projects(projects))) = self
            .request(
                "projects",
                RequestBody::Query(Query::Workspace(WorkspaceQuery::ListProjects(
                    Default::default(),
                ))),
            )
        else {
            panic!("expected the project list");
        };
        let ReplyPayload::Command(reply) = self.request(
            "lead",
            RequestBody::Command(Command::Conversation(ConversationCommand::SendChat(
                SendChat {
                    project: projects.items[0].id.clone(),
                    thread: ThreadId::new(LEAD).unwrap(),
                    submission_id: SubmissionId::new("lead-1").unwrap(),
                    composer_role: ComposerRole::Orchestrator,
                    text: "coordinate".into(),
                    skills: Vec::new(),
                    images: Vec::new(),
                    model_preference: None,
                    fork_seed: None,
                },
            ))),
        ) else {
            panic!("expected a command reply");
        };
        reply.run_id.unwrap().as_str().parse().unwrap()
    }

    async fn next_event(&mut self) -> Event {
        let event = self.events.recv().await.unwrap();
        // Shown only when the test fails: what the runtime did up to the point it stopped.
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) => eprintln!("phase {run_id} -> {to:?} {reason:?}"),
            EventKind::Diagnostic(d) => {
                eprintln!(
                    "diagnostic {} {} {} thread={:?}",
                    d.source, d.code, d.detail, d.thread_id
                );
            }
            _ => (),
        }
        if let EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) = &event.kind {
            self.questions.push(question.clone());
        }
        event
    }

    /// Receives events until every run in `runs` has reached `wanted`, in any order.
    async fn phase(&mut self, runs: &[RunId], wanted: AgentRunPhase) {
        let mut pending: HashSet<String> = runs.iter().map(ToString::to_string).collect();
        while !pending.is_empty() {
            let event = self.next_event().await;
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) = &event.kind
                && pending.contains(run_id)
            {
                assert_ne!(*to, AgentRunPhase::Error, "{run_id} failed: {reason:?}");
                if *to == wanted {
                    pending.remove(run_id);
                }
            }
        }
    }

    /// Ends `run` through its own `finish` call.
    async fn finish(&self, run: RunId) {
        self.model.finishing.lock().unwrap().insert(run.to_string());
        self.boot
            .runtime
            .send_message(run, "finish independently".into())
            .unwrap();
        self.boot.runtime.wait(run).await.unwrap();
        self.model
            .finishing
            .lock()
            .unwrap()
            .remove(&run.to_string());
        assert_eq!(
            self.boot.runtime.inspect_agent(run).unwrap().phase,
            AgentRunPhase::Done
        );
    }

    fn script(&self, run: RunId, call: ContentBlock, prompt: &str) {
        self.model
            .scripted
            .lock()
            .unwrap()
            .insert(run.to_string(), call);
        self.boot.runtime.send_message(run, prompt.into()).unwrap();
    }

    /// A drive another flight (the runtime's own trigger) may hold is repeated until it is
    /// ours; every flight ends, so this is bounded by the runtime and not by a clock.
    async fn drive(&self, thread: &str) -> LaneDeliveryOutcome {
        loop {
            match self
                .boot
                .runtime
                .drive_lane_deliveries(thread)
                .await
                .unwrap()
            {
                LaneDeliveryOutcome::Busy => tokio::task::yield_now().await,
                outcome => return outcome,
            }
        }
    }

    fn saw(&self, run: RunId, needle: &str) -> bool {
        self.model.texts.lock().unwrap()[&run.to_string()]
            .iter()
            .any(|request| request.contains(needle))
    }
}

fn brief() -> event_bus::LaneBrief {
    event_bus::LaneBrief {
        verification_requirements: Default::default(),
        objective: "an independent deliverable".into(),
        inputs: vec![],
        deliverables: vec!["artifact".into()],
        acceptance_criteria: vec!["review".into()],
        constraints: vec![],
        workspace: Default::default(),
        report_policy: event_bus::LanePolicy::Attached,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_core_bootstrap_delivers_a_lane_report_and_a_lead_answer_by_restoring_finished_roots() {
    let mut core = Core::open().await;
    assert_eq!(
        core.boot.runtime.lane_delivery_binding(),
        LaneDeliveryBinding::Bound,
        "the bootstrap, not the test, binds delivery"
    );
    let lead = core.start_lead();
    core.phase(&[lead], AgentRunPhase::Waiting).await;
    let lane = core
        .boot
        .runtime
        .open_lane(lead, "core-lane", brief())
        .unwrap();
    core.phase(&[lane.root_run_id], AgentRunPhase::Waiting)
        .await;
    core.finish(lead).await;

    // Report: the finished lead is restored by the bootstrap's host to receive it.
    core.script(
        lane.root_run_id,
        tool(
            "report",
            "lane_report",
            serde_json::json!({"status":"ready","summary":REPORT,"artifact_refs":["artifact:one"]}),
        ),
        "report the milestone",
    );
    core.phase(&[lead, lane.root_run_id], AgentRunPhase::Waiting)
        .await;
    core.drive(LEAD).await;
    assert!(
        core.saw(lead, REPORT),
        "the restored lead received the lane report"
    );

    // Answer: the lane asks and ends, so both it and the lead are finished when the answer is
    // sent, and the answer has to restore the lane.
    core.model
        .finishing
        .lock()
        .unwrap()
        .insert(lane.root_run_id.to_string());
    core.script(
        lane.root_run_id,
        tool(
            "ask",
            "ask_user",
            serde_json::json!({"title":"Which task scope?","blocking":false}),
        ),
        "ask the managing lead",
    );
    core.boot.runtime.wait(lane.root_run_id).await.unwrap();
    core.model
        .finishing
        .lock()
        .unwrap()
        .remove(&lane.root_run_id.to_string());
    assert_eq!(
        core.boot
            .runtime
            .inspect_agent(lane.root_run_id)
            .unwrap()
            .phase,
        AgentRunPhase::Done
    );
    core.phase(&[lead], AgentRunPhase::Waiting).await;
    core.drive(LEAD).await;
    // drive() does not consume bus events. Waiting can precede this delivery,
    // so wait for the announcement instead of inspecting an earlier snapshot.
    let question = loop {
        if let Some(question) = core
            .questions
            .iter()
            .find(|q| q.title == "Which task scope?")
        {
            break question.clone();
        }
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Error,
            reason,
            ..
        }) = core.next_event().await.kind
        {
            panic!("{run_id} failed before the question announcement: {reason:?}");
        }
    };

    core.model.scripted.lock().unwrap().insert(
        lane.root_run_id.to_string(),
        tool("read-answer", "user_answers", serde_json::json!({})),
    );
    core.script(
        lead,
        tool(
            "answer",
            "lane_answer",
            serde_json::json!({
                "question_id": question.id,
                "management_revision": question.management_revision.unwrap(),
                "routing_revision": question.routing_revision,
                "operation_id": "core-answer",
                "action": {"kind": "answer", "text": ANSWER}
            }),
        ),
        "answer the pending lane question",
    );
    core.phase(&[lead, lane.root_run_id], AgentRunPhase::Waiting)
        .await;
    core.drive(LEAD).await;
    core.drive(&lane.thread_id).await;

    let stored = core
        .boot
        .runtime
        .user_question(&question.id)
        .unwrap()
        .unwrap();
    assert!(matches!(
        stored.answer_origin,
        Some(event_bus::AnswerOrigin::LaneLead { .. })
    ));
    assert!(
        core.saw(lane.root_run_id, ANSWER),
        "the restored lane could read the lead's answer"
    );
    assert_eq!(core.drive(LEAD).await, LaneDeliveryOutcome::Empty);
    assert_eq!(
        core.drive(&lane.thread_id).await,
        LaneDeliveryOutcome::Empty,
        "every notification was acknowledged"
    );
    core.boot.shutdown().await.unwrap();
}

/// What a client that subscribed before an action is told about afterwards.
struct Client(workbench_host::subscriptions::Subscription);

impl Client {
    /// Waits for the next event `wanted` accepts. The core announces what changed after it is
    /// stored, so a read made once this returns already sees the change.
    async fn until<T>(
        &mut self,
        wanted: impl Fn(&workbench_protocol::events::UiEvent) -> Option<T>,
    ) -> T {
        loop {
            let envelope = self.0.recv().await.expect("the subscription stays open");
            if let Some(found) = wanted(&envelope.event) {
                return found;
            }
        }
    }

    async fn lane_changed(&mut self, lane_id: &str) {
        self.until(|event| match event {
            workbench_protocol::events::UiEvent::Lanes(
                workbench_protocol::namespaces::LanesEvent::LaneChanged { lane, .. },
            ) if lane.lane_id.as_str() == lane_id => Some(()),
            _ => None,
        })
        .await;
    }

    async fn execution_changed(&mut self, thread: &str) {
        self.until(|event| match event {
            workbench_protocol::events::UiEvent::Execution(
                workbench_protocol::namespaces::ExecutionEvent::ThreadExecutionChanged {
                    thread: changed,
                    ..
                },
            ) if changed.as_str() == thread => Some(()),
            _ => None,
        })
        .await;
    }
}

impl Core {
    /// A client subscribed to one thread only.
    fn client_of(&self, thread: &str) -> Client {
        let host = self.boot.host.lock().unwrap();
        Client(
            host.projection()
                .subscribe_with_snapshot(&workbench_protocol::events::Subscribe {
                    scope: workbench_protocol::events::SubscriptionScope::Thread {
                        thread: ThreadId::new(thread).unwrap(),
                    },
                    after: None,
                })
                .unwrap(),
        )
    }

    fn client(&self) -> Client {
        let host = self.boot.host.lock().unwrap();
        Client(
            host.projection()
                .subscribe_with_snapshot(&workbench_protocol::events::Subscribe {
                    scope: workbench_protocol::events::SubscriptionScope::All,
                    after: None,
                })
                .unwrap(),
        )
    }

    fn query(&self, id: &str, query: Query) -> QueryReply {
        match self.request(id, RequestBody::Query(query)) {
            ReplyPayload::Query(reply) => reply,
            other => panic!("{other:?}"),
        }
    }

    fn lane_detail(&self, lane_id: &str) -> workbench_protocol::namespaces::lanes::LaneDetail {
        let QueryReply::Lanes(workbench_protocol::namespaces::LanesReply::Lane(detail)) = self
            .query(
                "lane-detail",
                Query::Lanes(workbench_protocol::namespaces::LanesQuery::GetLane {
                    lane_id: workbench_protocol::lane_attention::LaneId::new(lane_id).unwrap(),
                }),
            )
        else {
            panic!("a lane detail was expected");
        };
        *detail
    }

    fn execution(
        &self,
        thread: &str,
    ) -> workbench_protocol::namespaces::execution::ThreadExecution {
        let QueryReply::Execution(workbench_protocol::namespaces::ExecutionReply::ThreadExecution(
            execution,
        )) = self.query(
            "thread-execution",
            Query::Execution(
                workbench_protocol::namespaces::ExecutionQuery::GetThreadExecution(
                    workbench_protocol::commands::ThreadTarget {
                        thread: ThreadId::new(thread).unwrap(),
                    },
                ),
            ),
        )
        else {
            panic!("a thread execution was expected");
        };
        *execution
    }

    fn project(&self) -> ProjectId {
        let QueryReply::Workspace(WorkspaceReply::Projects(projects)) = self.query(
            "project-of",
            Query::Workspace(WorkspaceQuery::ListProjects(Default::default())),
        ) else {
            panic!("expected the project list");
        };
        projects.items[0].id.clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operator_stop_and_continue_reach_every_client_and_the_held_notice_is_delivered() {
    let mut core = Core::open().await;
    let lead = core.start_lead();
    core.phase(&[lead], AgentRunPhase::Waiting).await;
    let lane = core
        .boot
        .runtime
        .open_lane(lead, "stop-lane", brief())
        .unwrap();
    core.phase(&[lane.root_run_id], AgentRunPhase::Waiting)
        .await;
    assert!(!core.lane_detail(&lane.lane_id).delivery.held);

    // The operator stops the lead: both clients are told, and the ledger says it is held.
    let mut clients = [core.client(), core.client()];
    core.boot
        .runtime
        .stop(lead, runtime::StopScope::SelfOnly)
        .unwrap();
    core.phase(&[lead], AgentRunPhase::Stopped).await;
    for client in &mut clients {
        client.lane_changed(&lane.lane_id).await;
    }
    assert!(core.lane_detail(&lane.lane_id).delivery.held);

    // A report made while the lead is stopped stays held and is not shown to it.
    core.script(
        lane.root_run_id,
        tool(
            "report",
            "lane_report",
            serde_json::json!({"status":"ready","summary":REPORT,"artifact_refs":["artifact:held"]}),
        ),
        "report while the lead is stopped",
    );
    core.phase(&[lane.root_run_id], AgentRunPhase::Waiting)
        .await;
    assert!(!core.saw(lead, REPORT));
    assert!(core.lane_detail(&lane.lane_id).delivery.held);

    // Continue: both clients are told again, and the held report reaches the lead.
    let mut clients = [core.client(), core.client()];
    let ReplyPayload::Command(_) = core.request(
        "continue",
        RequestBody::Command(Command::Conversation(ConversationCommand::ContinueChat(
            workbench_protocol::commands::ContinueChat {
                project: core.project(),
                thread: ThreadId::new(LEAD).unwrap(),
                composer_role: ComposerRole::Orchestrator,
                model_preference: None,
            },
        ))),
    ) else {
        panic!("expected a command reply");
    };
    for client in &mut clients {
        client.lane_changed(&lane.lane_id).await;
    }
    core.drive(LEAD).await;
    assert!(core.saw(lead, REPORT), "the held report was delivered");
    assert!(!core.lane_detail(&lane.lane_id).delivery.held);
    core.boot.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_working_note_written_or_cleared_alone_reaches_every_client() {
    let mut core = Core::open().await;
    let lead = core.start_lead();
    core.phase(&[lead], AgentRunPhase::Waiting).await;
    assert!(core.execution(LEAD).ledger.is_none());

    let mut clients = [core.client(), core.client()];
    core.script(
        lead,
        tool(
            "note-1",
            "working_note_write",
            serde_json::json!({"body":"first working note"}),
        ),
        "write the working note",
    );
    for client in &mut clients {
        client.execution_changed(LEAD).await;
    }
    let written = core.execution(LEAD);
    assert!(written.goal.is_none() && written.todo.is_none());
    assert_eq!(
        written.ledger.as_ref().map(|ledger| ledger.body.as_str()),
        Some("first working note")
    );

    // Clearing is a note too: the view follows it although goal and todo never changed.
    let mut clients = [core.client(), core.client()];
    core.script(
        lead,
        tool(
            "note-2",
            "working_note_write",
            serde_json::json!({"body":""}),
        ),
        "clear the working note",
    );
    for client in &mut clients {
        client.execution_changed(LEAD).await;
    }
    let cleared = core.execution(LEAD).ledger.expect("the clear is an entry");
    assert!(cleared.body.is_empty());
    assert!(cleared.seq > written.ledger.unwrap().seq);
    core.boot.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_detach_reaches_the_lead_that_detached_the_lane_and_the_lane_thread_when_scoped() {
    let mut core = Core::open().await;
    let lead = core.start_lead();
    core.phase(&[lead], AgentRunPhase::Waiting).await;
    let lane = core
        .boot
        .runtime
        .open_lane(lead, "detach-lane", brief())
        .unwrap();
    core.phase(&[lane.root_run_id], AgentRunPhase::Waiting)
        .await;
    let revision = core.lane_detail(&lane.lane_id).summary.management_revision;

    // Each client sees only its own thread: the lead's and the lane's.
    let (mut of_lead, mut of_lane) = (core.client_of(LEAD), core.client_of(&lane.thread_id));
    let ReplyPayload::Command(_) = core.request(
        "detach",
        RequestBody::Command(Command::Lanes(
            workbench_protocol::namespaces::LanesCommand::ManageLane(
                workbench_protocol::lane_management::ManageLane {
                    request: workbench_protocol::lane_management::LaneManagementRequest {
                        operation_id:
                            workbench_protocol::lane_management::LaneManagementOperationId::new(
                                "detach-1",
                            )
                            .unwrap(),
                        lane_id: workbench_protocol::lane_attention::LaneId::new(&lane.lane_id)
                            .unwrap(),
                        expected_management_revision: revision,
                        action: workbench_protocol::lane_management::LaneManagementAction::Detach,
                    },
                    expected_family_revision: None,
                },
            ),
        )),
    ) else {
        panic!("expected a command reply");
    };
    // The lane has no lead now, yet the lead that managed it hears of the detach.
    for client in [&mut of_lead, &mut of_lane] {
        let detached = client
            .until(|event| match event {
                workbench_protocol::events::UiEvent::Lanes(
                    workbench_protocol::namespaces::LanesEvent::LaneChanged {
                        lane: changed, ..
                    },
                ) if changed.lane_id.as_str() == lane.lane_id => Some(changed.clone()),
                _ => None,
            })
            .await;
        assert!(detached.lead_thread_id.is_none());
    }
    core.boot.shutdown().await.unwrap();
}
