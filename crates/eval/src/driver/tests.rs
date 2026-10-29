use std::path::PathBuf;

use event_bus::{DiagnosticEvent, UserQuestion};
use providers::ContentBlock;
use runtime::{AgentRuntime, RunStore};
use storage::thread_start::ThreadStartSnapshot;

use super::*;
use crate::source::{UserTurn, UserTurnKind};
use crate::test_support::{ScriptedModel, Step, decision, text, tool};

struct Harness {
    _dir: tempfile::TempDir,
    _storage: storage::Storage,
    bus: Arc<EventBus>,
    runtime: AgentRuntime,
    agent: Arc<ScriptedModel>,
    user: Arc<ScriptedModel>,
    thread: OriginalThread,
}

impl Harness {
    fn new(
        agent: impl IntoIterator<Item = Step>,
        agent_tokens: u64,
        user: impl IntoIterator<Item = Step>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("evorch.db"),
            ..storage::StorageConfig::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(1024));
        let agent = ScriptedModel::with_tokens(agent, agent_tokens);
        agent.attach(&bus);
        let user = ScriptedModel::new(user);
        user.attach(&bus);
        let runtime = AgentRuntime::new(
            Arc::clone(&bus),
            Arc::new(tools::ToolExecutor::new(Arc::clone(&bus))),
            Arc::clone(&agent) as Arc<dyn runtime::AgentModel>,
        )
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let thread = OriginalThread {
            thread_id: "thread-1".into(),
            role: runtime::Role::Worker,
            root_run_id: "original-root".into(),
            project_root: PathBuf::from("/original/repo"),
            source_toplevel: PathBuf::from("/original/repo"),
            captured_at_ns: 0,
            snapshot: ThreadStartSnapshot {
                repo_prefix: String::new(),
                head_commit: "0".repeat(40),
                head_ref: None,
                diff: Vec::new(),
                untracked: Vec::new(),
            },
            turns: vec![
                turn(UserTurnKind::InitialPrompt, "fix the bug"),
                turn(UserTurnKind::Message, "also add a test"),
            ],
        };
        Self {
            _dir: dir,
            _storage: storage,
            bus,
            runtime,
            agent,
            user,
            thread,
        }
    }

    async fn drive(&self, budget: &EvalBudgetConfig) -> ReplayOutcome {
        let simulator = UserSimulator::new(
            Arc::clone(&self.user) as Arc<dyn runtime::AgentModel>,
            &self.thread.thread_id,
        );
        drive(Replay {
            runtime: &self.runtime,
            bus: &self.bus,
            thread: &self.thread,
            simulator: &simulator,
            budget,
            chat: RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..RunConfig::default()
            },
        })
        .await
    }

    fn root_phases(&self) -> Vec<AgentRunPhase> {
        self.runtime
            .list_agents()
            .into_iter()
            .map(|agent| agent.phase)
            .collect()
    }

    fn simulator_payloads(&self) -> Vec<serde_json::Value> {
        self.user
            .requests()
            .iter()
            .map(|messages| {
                let Some(ContentBlock::Text { text }) = messages[1].content.first() else {
                    panic!("simulator payload");
                };
                serde_json::from_str(text).unwrap()
            })
            .collect()
    }
}

fn turn(kind: UserTurnKind, text: &str) -> UserTurn {
    UserTurn {
        kind,
        text: text.into(),
        question: None,
        reference: None,
    }
}

fn user(text: &str) -> Entry {
    Entry::User { text: text.into() }
}

fn assistant(text: &str) -> Entry {
    Entry::Assistant { text: text.into() }
}

fn sent_texts(model: &ScriptedModel) -> Vec<String> {
    model
        .requests()
        .iter()
        .flat_map(|messages| messages.iter())
        .filter(|message| message.role == providers::Role::User)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn simulated_reply_reaches_the_agent_and_finish_stops_the_replay() {
    // Given: an agent answering twice and a user who follows up once, then finishes.
    let harness = Harness::new(
        [text("found the bug"), text("test added")],
        10,
        [decision("reply", "also add a test"), decision("finish", "")],
    );

    // When: the replay runs.
    let outcome = harness.drive(&EvalBudgetConfig::default()).await;

    // Then: the follow-up was delivered, the user's view was recorded and the run stopped.
    assert_eq!(outcome.status, Status::Completed, "{}", outcome.reason);
    assert!(outcome.reason.contains("finished"), "{}", outcome.reason);
    assert_eq!(outcome.user_turns, 2);
    assert_eq!(
        outcome.conversation,
        [
            user("fix the bug"),
            assistant("found the bug"),
            user("also add a test"),
            assistant("test added"),
        ]
    );
    assert!(
        sent_texts(&harness.agent)
            .iter()
            .any(|text| text.contains("also add a test"))
    );
    assert_eq!(outcome.tokens, 20, "simulator requests are not counted");
    assert_eq!(harness.root_phases(), [AgentRunPhase::Stopped]);
    let payloads = harness.simulator_payloads();
    assert_eq!(payloads.len(), 2);
    assert_eq!(
        payloads[0]["original_user_messages"][1]["text"],
        "also add a test"
    );
    assert_eq!(
        payloads[0]["replay_conversation"][1],
        serde_json::json!({"kind": "assistant", "text": "found the bug"})
    );
    assert!(payloads[0]["pending_question"].is_null());
}

#[tokio::test]
async fn questions_from_the_root_are_answered_by_the_simulated_user() {
    // Given: an agent that asks which file to change, and a user who answers then finishes.
    let harness = Harness::new(
        [
            tool("ask_user", serde_json::json!({"title": "Which file?"})),
            text("asked"),
        ],
        10,
        [decision("answer", "lib.rs"), decision("finish", "")],
    );

    // When: the replay runs.
    let outcome = harness.drive(&EvalBudgetConfig::default()).await;

    // Then: the simulator saw the question and its answer reached the agent.
    assert_eq!(outcome.status, Status::Completed, "{}", outcome.reason);
    let payloads = harness.simulator_payloads();
    assert_eq!(payloads[0]["pending_question"]["title"], "Which file?");
    assert!(outcome.conversation.iter().any(|entry| matches!(
        entry,
        Entry::Question { title, .. } if title == "Which file?"
    )));
    assert!(outcome.conversation.iter().any(|entry| matches!(
        entry,
        Entry::Answer { title, text, .. } if title == "Which file?" && text == "lib.rs"
    )));
    assert_eq!(outcome.user_turns, 2);
    assert!(
        harness
            .runtime
            .list_agents()
            .iter()
            .all(|agent| agent.phase == AgentRunPhase::Stopped)
    );
}

#[tokio::test]
async fn exhausted_budgets_cancel_the_replay_as_incomplete() {
    // Given: replays that exceed the token, user turn and elapsed time budgets.
    let tokens = Harness::new(
        [text("first"), text("second")],
        1_000,
        [decision("reply", "more"), decision("reply", "more")],
    );
    let turns = Harness::new([text("first")], 10, [decision("reply", "more")]);
    let hanging = Harness::new([Step::Hang], 10, []);

    // When: each runs under its budget.
    let tokens = (
        tokens
            .drive(&EvalBudgetConfig {
                max_tokens: 1_500,
                ..EvalBudgetConfig::default()
            })
            .await,
        tokens,
    );
    let turns = (
        turns
            .drive(&EvalBudgetConfig {
                max_user_turns: 1,
                ..EvalBudgetConfig::default()
            })
            .await,
        turns,
    );
    let hanging = (
        hanging
            .drive(&EvalBudgetConfig {
                max_elapsed_secs: 1,
                ..EvalBudgetConfig::default()
            })
            .await,
        hanging,
    );

    // Then: each is incomplete, names its budget and leaves no live run.
    for ((outcome, harness), budget) in [
        (tokens, "token"),
        (turns, "user turn"),
        (hanging, "elapsed"),
    ] {
        assert_eq!(outcome.status, Status::Incomplete, "{}", outcome.reason);
        assert!(outcome.reason.contains(budget), "{}", outcome.reason);
        assert!(
            harness
                .root_phases()
                .iter()
                .all(|phase| matches!(phase, AgentRunPhase::Stopped | AgentRunPhase::Error)),
            "{:?}",
            harness.root_phases()
        );
    }
}

#[tokio::test]
async fn approval_requests_are_denied_and_escalation_denials_recorded() {
    // Given: an observer of a replay whose tools ask for approval or escalation.
    let bus = Arc::new(EventBus::new(64));
    let mut events = bus.subscribe();
    let mut observed = Observed {
        root: Some("root".into()),
        ..Observed::default()
    };

    // When: an approval request, a tool call and its denied escalation are observed.
    for kind in [
        EventKind::Tool(ToolEvent::ApprovalRequested {
            tool_name: "web_fetch".into(),
            call_id: "approval-1".into(),
            input: Some(serde_json::json!({"url": "https://example.com"})),
        }),
        EventKind::Tool(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "call-1".into(),
            input: Some(serde_json::json!({"command": "git push"})),
            run_id: Some("root".into()),
            parent_call_id: None,
        }),
        EventKind::Diagnostic(DiagnosticEvent {
            source: "sandbox".into(),
            severity: DiagnosticSeverity::Warning,
            code: diagnostic_codes::ESCALATION_REVIEW.into(),
            detail: "denied: shell escalation is disabled".into(),
            run_id: Some("root".into()),
            thread_id: None,
            call_id: Some("call-1".into()),
        }),
    ] {
        observed.observe(&Event::new(kind), &bus);
    }

    // Then: the approval was refused on the bus and both denials are recorded.
    let resolved = events.drain_pending_snapshot();
    assert!(resolved.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ApprovalResolved { call_id, approved: false }) if call_id == "approval-1"
    )));
    assert_eq!(observed.tool_calls, 1);
    assert_eq!(
        observed
            .denied
            .iter()
            .map(|denied| (denied.tool.as_str(), denied.reason.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                "web_fetch",
                "approval requests are denied in evaluation replays"
            ),
            ("shell", "denied: shell escalation is disabled"),
        ]
    );
    assert!(observed.denied[1].input.contains("git push"));
}

#[test]
fn user_visible_questions_are_tracked_until_answered() {
    // Given: an observer and a root question followed by its answer.
    let bus = EventBus::new(8);
    let mut observed = Observed {
        root: Some("root".into()),
        ..Observed::default()
    };
    let question = UserQuestion {
        id: "q-1".into(),
        run_id: "root".into(),
        root_run_id: "root".into(),
        root_name: "chat:Worker:thread-1".into(),
        recipient_run_ids: Vec::new(),
        title: "Which file?".into(),
        options: vec!["a".into()],
        blocking: true,
        answer: None,
        answer_origin: None,
    };
    let answered = UserQuestion {
        answer: Some("a".into()),
        ..question.clone()
    };

    // When / Then: the question is pending until its answer is observed.
    observed.observe(
        &Event::new(ToolEvent::UserQuestionUpdated { question }),
        &bus,
    );
    assert_eq!(observed.pending.len(), 1);
    observed.observe(
        &Event::new(ToolEvent::UserQuestionUpdated { question: answered }),
        &bus,
    );
    assert!(observed.pending.is_empty());
    assert_eq!(observed.conversation.len(), 1);
}
