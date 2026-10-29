use std::sync::Arc;

use event_bus::{Event, EventBus, LifecycleEvent, OrchestratorEvent};
use runtime::{AgentRuntime, RunConfig, RunId, RunStore};

use super::*;
use crate::test_support::ScriptedModel;
use crate::workspace::tests::Source;

/// A user database recorded by a live runtime, as the GUI would leave it.
pub(crate) struct Recorded {
    pub(crate) source: Source,
    pub(crate) config: storage::StorageConfig,
    pub(crate) storage: storage::Storage,
    runtime: AgentRuntime,
    bus: Arc<EventBus>,
}

impl Recorded {
    pub(crate) fn new() -> Self {
        let source = Source::new();
        let config = storage::StorageConfig {
            db_path: source.dir.path().join("evorch-events.db"),
            ..storage::StorageConfig::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(1024));
        let runtime = AgentRuntime::new(
            Arc::clone(&bus),
            Arc::new(tools::ToolExecutor::new(Arc::clone(&bus))),
            ScriptedModel::new([]),
        )
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        Self {
            source,
            config,
            storage,
            runtime,
            bus,
        }
    }

    fn chat(&self, project: &Path) -> RunConfig {
        RunConfig {
            conversation: true,
            interactive: true,
            keep_alive: true,
            project_root: Some(project.to_path_buf()),
            ..RunConfig::default()
        }
    }

    async fn turn(&self, events: &mut event_bus::EventReceiver, run: RunId) {
        loop {
            let event = events.recv().await.expect("event bus open");
            if let event_bus::EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. }) =
                event.kind
                && run_id == run.to_string()
            {
                return;
            }
        }
    }

    /// A Worker thread with a follow-up, an answered question and a continuation run.
    pub(crate) async fn conversation(&self, thread: &str, project: &Path) -> RunId {
        let mut events = self.bus.subscribe();
        let first = self
            .runtime
            .delegate_chat(
                thread,
                Role::Worker,
                "fix the bug".into(),
                self.chat(project),
            )
            .unwrap();
        self.turn(&mut events, first).await;
        let question = self
            .runtime
            .request_user_question(first, "Which file?".into(), vec![], true)
            .unwrap();
        self.runtime
            .answer_user_question(&question.id, "lib.rs")
            .unwrap();
        self.turn(&mut events, first).await;
        self.runtime
            .continue_goal(first, "also add a test".into(), self.chat(project))
            .unwrap();
        self.turn(&mut events, first).await;
        self.runtime
            .stop(first, runtime::StopScope::Subtree)
            .unwrap();
        self.runtime.wait(first).await.unwrap();
        let next = self
            .runtime
            .delegate_chat(
                thread,
                Role::Worker,
                "thanks, ship it".into(),
                self.chat(project),
            )
            .unwrap();
        self.turn(&mut events, next).await;
        self.runtime
            .stop(next, runtime::StopScope::Subtree)
            .unwrap();
        self.runtime.wait(next).await.unwrap();
        first
    }

    fn database(&self) -> Database {
        Database::open_read_only(&self.config.db_path).unwrap()
    }
}

#[tokio::test]
async fn recorded_thread_yields_its_start_role_and_user_turns_in_order() {
    // Given: a recorded Worker thread in a subdirectory project with every kind of user turn.
    let recorded = Recorded::new();
    let project = recorded.source.root().join("app").canonicalize().unwrap();
    let first = recorded.conversation("thread-1", &project).await;

    // When: the thread is loaded from the read-only database.
    let thread = OriginalThread::load(&recorded.database(), "thread-1").unwrap();

    // Then: the start, its repository and the user's turns are reconstructed.
    assert_eq!(thread.role, Role::Worker);
    assert_eq!(thread.root_run_id, first.to_string());
    assert_eq!(thread.project_root, project);
    assert_eq!(
        thread.source_toplevel,
        recorded.source.root().canonicalize().unwrap()
    );
    assert_eq!(thread.snapshot.repo_prefix, "app/");
    assert_eq!(thread.snapshot.head_commit, recorded.source.head());
    assert_eq!(thread.initial_prompt(), "fix the bug");
    let turns: Vec<_> = thread
        .turns
        .iter()
        .map(|turn| (turn.kind, turn.text.as_str(), turn.question.as_deref()))
        .collect();
    assert_eq!(
        turns,
        [
            (UserTurnKind::InitialPrompt, "fix the bug", None),
            (UserTurnKind::Answer, "lib.rs", Some("Which file?")),
            (UserTurnKind::Message, "also add a test", None),
            (UserTurnKind::Message, "thanks, ship it", None),
        ]
    );
}

#[tokio::test]
async fn threads_without_a_replayable_start_or_with_a_goal_are_refused() {
    // Given: a thread started outside git, a thread with a goal, and an unknown thread.
    let recorded = Recorded::new();
    let outside = recorded.source.dir.path().join("plain");
    std::fs::create_dir(&outside).unwrap();
    let outside = outside.canonicalize().unwrap();
    recorded.conversation("plain", &outside).await;
    recorded
        .conversation("goal", &recorded.source.root().canonicalize().unwrap())
        .await;
    recorded
        .storage
        .handle()
        .append_event(
            None,
            &Event::new(OrchestratorEvent::ThreadGoalUpdated {
                snapshot: event_bus::ThreadGoalSnapshot {
                    goal_id: "goal-1".into(),
                    thread_id: "goal".into(),
                    root_run_id: "run-1".into(),
                    related_root_run_ids: Vec::new(),
                    objective: "objective".into(),
                    original_request: "objective".into(),
                    criteria: Vec::new(),
                    checks: Vec::new(),
                    phase: event_bus::ThreadGoalPhase::Working,
                    review_enabled: false,
                    checks_paused: false,
                    work_stopped: false,
                    epoch: 1,
                    review_round: 0,
                    findings: Vec::new(),
                    reason: None,
                    usage: event_bus::ThreadGoalUsage::default(),
                    max_tokens: None,
                    max_review_rounds: 3,
                },
            }),
        )
        .unwrap();
    let database = recorded.database();

    // When / Then: each is refused with its reason.
    assert!(matches!(
        OriginalThread::load(&database, "plain"),
        Err(SourceError::NotReplayable {
            reason: "not_git_repository",
            ..
        })
    ));
    assert!(matches!(
        OriginalThread::load(&database, "goal"),
        Err(SourceError::UsesThreadGoal(_))
    ));
    assert!(matches!(
        OriginalThread::load(&database, "missing"),
        Err(SourceError::NoStartState(_))
    ));
}

#[test]
fn repository_top_level_drops_the_recorded_prefix() {
    assert_eq!(
        strip_prefix(Path::new("/work/repo/crates/app"), "crates/app/").unwrap(),
        PathBuf::from("/work/repo")
    );
    assert_eq!(
        strip_prefix(Path::new("/work/repo"), "").unwrap(),
        PathBuf::from("/work/repo")
    );
    assert!(strip_prefix(Path::new("/work/repo/other"), "crates/app/").is_err());
}
