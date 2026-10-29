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
        self.drive_via(budget, &self.runtime).await
    }

    async fn drive_via(&self, budget: &EvalBudgetConfig, desk: &dyn Desk) -> ReplayOutcome {
        let simulator = UserSimulator::new(
            Arc::clone(&self.user) as Arc<dyn runtime::AgentModel>,
            &self.thread.thread_id,
        );
        drive_with(
            Replay {
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
            },
            desk,
        )
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

/// A question store that lets someone else answer at the one moment the driver cannot
/// see coming: after its last check that the question still waits, as it sends its own
/// answer.
struct AnsweredJustBefore<'a> {
    runtime: &'a AgentRuntime,
    other_answer: &'a str,
    interrupted: std::sync::atomic::AtomicBool,
}

impl Desk for AnsweredJustBefore<'_> {
    fn is_answered(&self, id: &str) -> bool {
        self.runtime.is_answered(id)
    }

    fn answer(&self, id: &str, text: &str) -> Result<AnswerOutcome, String> {
        if !self
            .interrupted
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.runtime
                .answer_user_question(id, self.other_answer)
                .unwrap();
        }
        self.runtime.answer(id, text)
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
    assert!(payloads[1]["pending_question"].is_null(), "{payloads:?}");
    assert!(
        harness
            .runtime
            .list_agents()
            .iter()
            .all(|agent| agent.phase == AgentRunPhase::Stopped)
    );
}

#[tokio::test]
async fn a_question_answered_while_the_user_decides_is_not_answered_again() {
    // Given: an agent that asks a question, and a simulated user whose answer is still
    // being decided when the question is answered by someone else.
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let harness = Harness::new(
        [
            tool("ask_user", serde_json::json!({"title": "Which file?"})),
            text("asked"),
        ],
        10,
        [
            Step::Gated {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                then: Box::new(decision("answer", "lib.rs")),
            },
            decision("finish", ""),
        ],
    );
    let interloper = async {
        entered.notified().await;
        let requests = harness.user.requests();
        let Some(ContentBlock::Text { text }) = requests[0][1].content.first() else {
            panic!("simulator payload");
        };
        let payload: serde_json::Value = serde_json::from_str(text).unwrap();
        let id = payload["pending_question"]["id"].as_str().unwrap();
        harness.runtime.answer_user_question(id, "main.rs").unwrap();
        release.notify_one();
    };

    // When: the replay runs and the other answer lands before the decision is returned.
    let budget = EvalBudgetConfig::default();
    let (outcome, ()) = tokio::join!(harness.drive(&budget), interloper);

    // Then: the stale decision is dropped instead of failing the replay with
    // `question already answered`, and the question is not shown to the user again.
    assert_eq!(outcome.status, Status::Completed, "{}", outcome.reason);
    assert!(
        !outcome
            .conversation
            .iter()
            .any(|entry| matches!(entry, Entry::Answer { .. })),
        "{:?}",
        outcome.conversation
    );
    assert_eq!(outcome.user_turns, 1);
    let payloads = harness.simulator_payloads();
    assert_eq!(payloads.len(), 2);
    assert!(payloads[1]["pending_question"].is_null(), "{payloads:?}");
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

#[tokio::test]
async fn final_report_published_by_finish_is_kept_in_the_conversation() {
    // Given: a root that reports its result through `finish` after a progress message,
    // and a user who has nothing to add.
    let mut harness = Harness::new(
        [
            text("working on it"),
            tool("finish", serde_json::json!({"result": "all fixed"})),
        ],
        10,
        [decision("reply", "go on"), decision("finish", "")],
    );
    harness.thread.role = runtime::Role::Orchestrator;

    // When: the replay runs.
    let outcome = harness.drive(&EvalBudgetConfig::default()).await;

    // Then: the final report is the last thing the user saw, and nothing is flagged.
    assert_eq!(outcome.status, Status::Completed, "{}", outcome.reason);
    assert_eq!(
        outcome.conversation.last(),
        Some(&assistant("all fixed")),
        "{:?}",
        outcome.conversation
    );
    assert!(
        !outcome.reason.contains("missing"),
        "a captured report is not flagged: {}",
        outcome.reason
    );
}

#[test]
fn final_report_repeating_the_last_response_is_recorded_once() {
    // Given: a root response that `finish` then publishes again verbatim, and a
    // second report from a child run.
    let bus = EventBus::new(8);
    let mut observed = Observed {
        root: Some("root".into()),
        ..Observed::default()
    };
    let publish = |run_id: &str, text: &str| {
        Event::new(MessageEvent::FinalResultPublished {
            run_id: run_id.into(),
            text: text.into(),
        })
    };

    // When: the response, its repeat, a child's report and a blank report arrive.
    observed.observe(
        &Event::new(MessageEvent::MessageCompleted {
            run_id: "root".into(),
            text: "done".into(),
        }),
        &bus,
    );
    observed.observe(&publish("root", "done"), &bus);
    observed.observe(&publish("child", "child report"), &bus);
    observed.observe(&publish("root", "  "), &bus);

    // Then: only the root's one distinct message is kept.
    assert_eq!(observed.conversation, [assistant("done")]);
}

#[test]
fn a_completed_replay_without_a_report_names_the_gap() {
    // Given: observations whose last entry is still the user's input.
    let missing = Observed {
        conversation: vec![user("fix the bug")],
        lagged_events: 3,
        ..Observed::default()
    };
    let answered = Observed {
        conversation: vec![user("fix the bug"), assistant("fixed")],
        ..Observed::default()
    };

    // When / Then: only the completed replay lacking a report is described, with
    // the lag that may explain it.
    let note = missing_report(Status::Completed, &missing).unwrap();
    assert!(note.contains("final report is missing"), "{note}");
    assert!(note.contains("skipped 3 events"), "{note}");
    assert_eq!(missing_report(Status::Completed, &answered), None);
    assert_eq!(missing_report(Status::Incomplete, &missing), None);
}

#[tokio::test]
async fn events_a_small_bus_overwrote_before_the_observer_stops_are_counted() {
    // Given: an observer on a capacity-2 bus that has not run yet while four
    // events arrive, so two are overwritten before it reads or stops.
    let bus = Arc::new(EventBus::new(2));
    let observer = Observer::start(&bus);
    for index in 0..4 {
        bus.emit(Event::new(MessageEvent::MessageCompleted {
            run_id: "root".into(),
            text: index.to_string(),
        }));
    }

    // When: the observer stops, by its normal lag handling or its final drain.
    let observed = observer.finish().await;

    // Then: at least the two overwritten events are counted. Lag handling in
    // `recv` emits its own fault event, which can overwrite one more slot.
    assert!(observed.lagged_events >= 2, "{}", observed.lagged_events);
}

fn root_question(id: &str, answer: Option<&str>) -> UserQuestion {
    UserQuestion {
        id: id.into(),
        run_id: "root".into(),
        root_run_id: "root".into(),
        root_name: "chat:Worker:thread-1".into(),
        recipient_run_ids: Vec::new(),
        title: "Which file?".into(),
        options: Vec::new(),
        blocking: true,
        answer: answer.map(Into::into),
        answer_origin: None,
    }
}

#[test]
fn an_answered_question_never_becomes_pending_again() {
    // Given: an observer and a question the driver answered itself.
    let bus = EventBus::new(8);
    let mut observed = Observed {
        root: Some("root".into()),
        ..Observed::default()
    };
    let update = |question| Event::new(ToolEvent::UserQuestionUpdated { question });
    observed.observe(&update(root_question("q-1", None)), &bus);
    observed.mark_answered("q-1");
    assert!(observed.pending.is_empty());

    // When: the question is announced unanswered again, then reported answered late.
    observed.observe(&update(root_question("q-1", None)), &bus);
    observed.observe(&update(root_question("q-1", Some("a"))), &bus);

    // Then: it stays out of the pending list and is shown in the conversation once.
    assert!(observed.pending.is_empty());
    assert_eq!(observed.conversation.len(), 1);
}

#[test]
fn a_missed_answer_event_is_made_up_from_the_question_store() {
    // Given: two pending questions, one of which the store already holds an answer for
    // (its answer event was skipped by an observer that fell behind the bus).
    let bus = EventBus::new(8);
    let state = Mutex::new(Observed {
        root: Some("root".into()),
        ..Observed::default()
    });
    for id in ["q-1", "q-2"] {
        lock(&state).observe(
            &Event::new(ToolEvent::UserQuestionUpdated {
                question: root_question(id, None),
            }),
            &bus,
        );
    }

    // When: pending questions are reconciled with the store.
    reconcile_answered(&state, |id| id == "q-1");

    // Then: only the unanswered question is left to ask the simulated user about.
    let state = lock(&state);
    assert_eq!(
        state
            .pending
            .iter()
            .map(|question| question.id.as_str())
            .collect::<Vec<_>>(),
        ["q-2"]
    );
    assert!(state.answered.contains("q-1"));
}

#[tokio::test]
async fn a_question_answered_after_the_last_check_is_dropped_not_failed() {
    // Given: for each other answer (different from, and identical to, the simulated
    // user's), a question that is answered by someone else after the driver's last
    // check and before its own answer is stored.
    for other_answer in ["main.rs", "lib.rs"] {
        let harness = Harness::new(
            [
                tool("ask_user", serde_json::json!({"title": "Which file?"})),
                text("asked"),
            ],
            10,
            [decision("answer", "lib.rs"), decision("finish", "")],
        );
        let desk = AnsweredJustBefore {
            runtime: &harness.runtime,
            other_answer,
            interrupted: std::sync::atomic::AtomicBool::new(false),
        };

        // When: the replay runs.
        let outcome = harness.drive_via(&EvalBudgetConfig::default(), &desk).await;

        // Then: the simulated user's answer is dropped without failing the replay, and
        // it is neither recorded as the user's answer nor counted as a user turn.
        assert_eq!(
            outcome.status,
            Status::Completed,
            "{other_answer}: {}",
            outcome.reason
        );
        assert!(
            !outcome
                .conversation
                .iter()
                .any(|entry| matches!(entry, Entry::Answer { .. })),
            "{other_answer}: {:?}",
            outcome.conversation
        );
        assert_eq!(outcome.user_turns, 1, "{other_answer}");
        let payloads = harness.simulator_payloads();
        assert_eq!(payloads.len(), 2, "{other_answer}");
        assert!(payloads[1]["pending_question"].is_null(), "{payloads:?}");
    }
}

#[tokio::test]
async fn a_recorded_answer_in_the_original_conversation_is_replayed_by_one_simulated_answer() {
    // Given: an original conversation in which the user answered a question, replayed
    // against an agent that asks it again.
    let mut harness = Harness::new(
        [
            tool("ask_user", serde_json::json!({"title": "Which file?"})),
            text("asked"),
        ],
        10,
        [decision("answer", "lib.rs"), decision("finish", "")],
    );
    harness.thread.turns = vec![
        turn(UserTurnKind::InitialPrompt, "fix the bug"),
        UserTurn {
            kind: UserTurnKind::Answer,
            text: "lib.rs".into(),
            question: Some("Which file?".into()),
            reference: None,
        },
    ];

    // When: the replay runs.
    let outcome = harness.drive(&EvalBudgetConfig::default()).await;

    // Then: the simulated user saw the recorded answer as context and the pending
    // question, and answered it exactly once: one stored answer, one answer entry, one
    // user turn, one answer delivered to the agent.
    assert_eq!(outcome.status, Status::Completed, "{}", outcome.reason);
    let payloads = harness.simulator_payloads();
    assert_eq!(
        payloads[0]["original_user_messages"][1],
        serde_json::json!({"kind": "answer", "text": "lib.rs", "question": "Which file?"})
    );
    assert_eq!(payloads[0]["pending_question"]["title"], "Which file?");
    assert!(payloads[1]["pending_question"].is_null(), "{payloads:?}");
    let answers: Vec<_> = outcome
        .conversation
        .iter()
        .filter(|entry| matches!(entry, Entry::Answer { .. }))
        .collect();
    assert_eq!(answers.len(), 1, "{:?}", outcome.conversation);
    assert_eq!(outcome.user_turns, 2);
    let delivered = sent_texts(&harness.agent)
        .iter()
        .filter(|text| text.contains("[user-answer id="))
        .count();
    assert_eq!(delivered, 1);
}
