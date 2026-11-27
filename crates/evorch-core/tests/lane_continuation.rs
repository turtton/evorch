//! A message typed into a lane's own thread, through the production core: the same wire
//! `SendChat` a client sends, behind `HostBackend` and the real `BootstrappedHost`. The host has
//! never bound the lane's root and the sidebar does not list the thread; the composer asks for a
//! Worker. The message must still reach the lane's own root (same run, Orchestrator, lane
//! scope) so that the lane can report again and its lead can close the new Ready. Only the model
//! is scripted.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use event_bus::{AgentRunPhase, EventKind, EventReceiver, LifecycleEvent};
use evorch_core::backend::HostBackend;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, Role, RunId, RuntimeError};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::commands::{Command, ComposerRole, SendChat};
use workbench_protocol::envelope::*;
use workbench_protocol::ids::*;
use workbench_protocol::namespaces::{ConversationCommand, WorkspaceQuery, WorkspaceReply};
use workbench_protocol::queries::{Query, QueryReply};

const LEAD: &str = "lead";
const OBJECTIVE_LINE: &str = "Investigate the flaky   build";
const TITLE: &str = "Investigate the flaky build";
const FOLLOW_UP: &str = "please also check the second target";

/// A run with a scripted tool call makes it once; every other turn only says it is idle and
/// waits.
#[derive(Default)]
struct Model {
    scripted: Mutex<HashMap<String, ContentBlock>>,
    /// What each run's provider requests carried, in request order.
    requests: Mutex<HashMap<String, Vec<Request>>>,
}

/// The wire-relevant parts of one provider request: the messages and the tool schemas.
struct Request {
    messages: Vec<serde_json::Value>,
    tools: String,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests
            .lock()
            .unwrap()
            .entry(invocation.run_id.clone())
            .or_default()
            .push(Request {
                messages: messages
                    .iter()
                    .map(|message| serde_json::to_value(message).unwrap())
                    .collect(),
                tools: serde_json::to_string(tools).unwrap(),
            });
        let tool = self.scripted.lock().unwrap().remove(&invocation.run_id);
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
        "core-lane-continuation".into()
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

struct Core {
    boot: BootstrappedHost,
    backend: HostBackend,
    model: Arc<Model>,
    events: EventReceiver,
    config_dir: std::path::PathBuf,
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
        let backend = HostBackend::new(boot.host.clone(), "core-lane-continuation".into());
        let events = boot.runtime.event_bus().subscribe();
        Self {
            boot,
            backend,
            model,
            events,
            config_dir: root,
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

    /// What a client sends for one message: the project is the one the core lists.
    fn send(&self, id: &str, thread: &str, role: ComposerRole, text: &str) -> RunId {
        let ReplyPayload::Query(QueryReply::Workspace(WorkspaceReply::Projects(projects))) = self
            .request(
                &format!("projects-{id}"),
                RequestBody::Query(Query::Workspace(WorkspaceQuery::ListProjects(
                    Default::default(),
                ))),
            )
        else {
            panic!("expected the project list");
        };
        let ReplyPayload::Command(reply) = self.request(
            id,
            RequestBody::Command(Command::Conversation(ConversationCommand::SendChat(
                SendChat {
                    project: projects.items[0].id.clone(),
                    thread: ThreadId::new(thread).unwrap(),
                    submission_id: SubmissionId::new(format!("sub-{id}")).unwrap(),
                    composer_role: role,
                    text: text.into(),
                    skills: Vec::new(),
                    attachments: Vec::new(),
                    model_preference: None,
                    fork_seed: None,
                },
            ))),
        ) else {
            panic!("expected a command reply");
        };
        reply.run_id.unwrap().as_str().parse().unwrap()
    }

    /// Receives events until every run in `runs` has reached `wanted`, in any order.
    async fn phase(&mut self, runs: &[RunId], wanted: AgentRunPhase) {
        let mut pending: std::collections::HashSet<String> =
            runs.iter().map(ToString::to_string).collect();
        while !pending.is_empty() {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) = self.events.recv().await.unwrap().kind
                && pending.contains(&run_id)
            {
                assert_ne!(to, AgentRunPhase::Error, "{run_id} failed: {reason:?}");
                if to == wanted {
                    pending.remove(&run_id);
                }
            }
        }
    }

    /// The lane root makes the scripted call once it is sent `prompt`.
    fn script(&self, run: RunId, call: ContentBlock, prompt: &str) {
        self.model
            .scripted
            .lock()
            .unwrap()
            .insert(run.to_string(), call);
        self.boot.runtime.send_message(run, prompt.into()).unwrap();
    }

    fn report_ready(&self, root: RunId, summary: &str) {
        self.script(
            root,
            ContentBlock::ToolUse {
                id: format!("report-{summary}"),
                name: "lane_report".into(),
                input: serde_json::json!({"status":"ready","summary":summary,"artifact_refs":["artifact:one"]}),
            },
            "report your result",
        );
    }

    /// The runs that were sent a request containing `needle`.
    fn runs_that_saw(&self, needle: &str) -> Vec<String> {
        self.model
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, requests)| {
                requests.iter().any(|request| {
                    request
                        .messages
                        .iter()
                        .any(|message| message.to_string().contains(needle))
                })
            })
            .map(|(run, _)| run.clone())
            .collect()
    }

    /// Every request of `run` extends the previous one: the history that was already sent and
    /// the tool schemas are unchanged, so the provider's cached prefix is reused.
    fn assert_prefix_preserved(&self, run: RunId) {
        let all = self.model.requests.lock().unwrap();
        let requests = &all[&run.to_string()];
        assert!(requests.len() >= 4, "the run was asked on every turn");
        for pair in requests.windows(2) {
            assert_eq!(pair[0].tools, pair[1].tools, "tool schemas changed");
            assert!(
                pair[1].messages.len() > pair[0].messages.len()
                    && pair[1].messages[..pair[0].messages.len()] == pair[0].messages[..],
                "a request did not extend the history already sent"
            );
        }
    }

    fn stored_title(&self, thread: &str) -> Option<String> {
        self.boot
            .runtime
            .with_storage_reader(|db| db.thread(thread))
            .unwrap()
            .flatten()
            .and_then(|row| row.title)
    }

    /// The title and role the shared sidebar lists the thread with, if it lists it.
    fn sidebar_entry(&self, thread: &str) -> Option<serde_json::Value> {
        let sidebar: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.config_dir.join("sidebar.json")).ok()?)
                .ok()?;
        sidebar["threads"]
            .as_array()?
            .iter()
            .find(|entry| entry["id"] == thread)
            .cloned()
    }

    fn ledger_root(&self, lane_id: &str) -> String {
        self.boot
            .runtime
            .with_storage_reader(|db| db.lane(lane_id))
            .unwrap()
            .flatten()
            .and_then(|lane| lane.root_run_id)
            .expect("a started lane has a root")
    }
}

fn brief() -> event_bus::LaneBrief {
    event_bus::LaneBrief {
        verification_requirements: Default::default(),
        objective: format!("{OBJECTIVE_LINE}\nsecond line that is not part of the title"),
        inputs: vec![],
        deliverables: vec!["artifact".into()],
        acceptance_criteria: vec!["review".into()],
        constraints: vec![],
        workspace: Default::default(),
        report_policy: event_bus::LanePolicy::Attached,
    }
}

fn close_request(
    lane_id: &str,
    ready_seq: u64,
    management: u64,
    request: u64,
) -> runtime::lane_close::LaneCloseRequest {
    runtime::lane_close::LaneCloseRequest {
        lane_id: lane_id.into(),
        ready_seq,
        expected_management_revision: management,
        expected_request_revision: request,
        note: "accepted".into(),
        verification_id: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_message_to_an_unbound_lane_continues_its_root_so_it_can_report_and_be_closed() {
    let mut core = Core::open().await;
    let lead = core.send("lead", LEAD, ComposerRole::Orchestrator, "coordinate");
    core.phase(&[lead], AgentRunPhase::Waiting).await;
    let lane = core
        .boot
        .runtime
        .open_lane(lead, "continued-lane", brief())
        .unwrap();
    let root = lane.root_run_id;
    core.phase(&[root], AgentRunPhase::Waiting).await;
    assert_eq!(
        core.stored_title(&lane.thread_id).as_deref(),
        Some(TITLE),
        "the lane is titled from its objective when it is created"
    );
    assert!(
        core.sidebar_entry(&lane.thread_id).is_none(),
        "the premise: no client has listed the lane's thread"
    );

    // The lane's first Ready, and the lead's view of it before anything is typed into the lane.
    core.report_ready(root, "first result");
    core.phase(&[root], AgentRunPhase::Waiting).await;
    let writer = core.boot.storage_writer().clone();
    let before = writer.lane_close_context(lane.lane_id.clone()).unwrap();
    let first_ready = before.latest_ready_seq.expect("the lane reported Ready");

    // The same wire message a client sends; the composer asks for a Worker.
    let accepted = core.send(
        "follow-up",
        &lane.thread_id,
        ComposerRole::Worker,
        FOLLOW_UP,
    );
    assert_eq!(accepted, root, "the message continues the lane's own run");
    core.phase(&[root], AgentRunPhase::Waiting).await;
    assert_eq!(
        core.runs_that_saw(FOLLOW_UP),
        vec![root.to_string()],
        "no other run was started for the message"
    );
    assert_eq!(core.ledger_root(&lane.lane_id), root.to_string());
    assert_eq!(core.stored_title(&lane.thread_id).as_deref(), Some(TITLE));
    let listed = core
        .sidebar_entry(&lane.thread_id)
        .expect("the thread is listed once it is used");
    assert_eq!(
        listed["title"], TITLE,
        "the sidebar title is the lane's, not the message"
    );

    // Input ends the earlier Ready's standing: the same close is refused as it always was.
    let refused = core
        .boot
        .runtime
        .close_lane(
            lead,
            close_request(
                &lane.lane_id,
                first_ready,
                before.management_revision,
                before.request_revision,
            ),
        )
        .unwrap_err();
    assert!(
        refused.code.starts_with("stale_"),
        "a close of the Ready that the input superseded was accepted or failed oddly: {refused}"
    );

    // The same root, still the ledger's current root, reports again and the lead closes that.
    core.report_ready(root, "second result");
    core.phase(&[root], AgentRunPhase::Waiting).await;
    let now = writer.lane_close_context(lane.lane_id.clone()).unwrap();
    let second_ready = now.latest_ready_seq.expect("the lane reported Ready again");
    assert!(second_ready > first_ready);
    let closed = core
        .boot
        .runtime
        .close_lane(
            lead,
            close_request(
                &lane.lane_id,
                second_ready,
                now.management_revision,
                now.request_revision,
            ),
        )
        .unwrap();
    assert_eq!(closed.lane_id, lane.lane_id);
    core.assert_prefix_preserved(root);
    core.boot.shutdown().await.unwrap();
}
