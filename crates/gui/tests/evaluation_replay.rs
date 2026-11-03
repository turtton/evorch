mod evaluations_support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use eval::inspection::{
    Artifact, EvaluationInspector, GroupPhase, MemberPhase, Purpose, ReportStatus, TrialCost,
    TrialState,
};
use evaluations_support::{
    FakeReader, MemberSpec, TrialSpec, entry, group, listing, member, no_cost, trial,
};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, ToolEvent};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::evaluations::{
    EvaluationReader, EvaluationsModel, InspectorReader, ManualSpawner, ReplayLoad, ReplayTarget,
    ReplayView,
};
use storage::{Storage, StorageConfig};
use workspace_ui::UiSettings;

fn started(run_id: &str, parent: Option<&str>, name: &str, role: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run_id.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: name.into(),
        role: role.into(),
    })
}

fn answered(run_id: &str, text: &str) -> Event {
    Event::new(MessageEvent::MessageCompleted {
        run_id: run_id.into(),
        text: text.into(),
    })
}

/// Records a finished trial's database the way an evaluation run leaves it: a
/// root run that called a tool and a Reviewer it delegated to.
fn record_trial_database(path: &Path) {
    let storage = Storage::open(StorageConfig {
        db_path: path.to_path_buf(),
        ..StorageConfig::default()
    })
    .expect("trial storage");
    let handle = storage.handle();
    let events = [
        started("run-1", None, "root-agent", "orchestrator"),
        started("run-2", Some("run-1"), "child-reviewer", "reviewer"),
        Event::new(ToolEvent::ToolStarted {
            tool_name: "read_file".into(),
            call_id: "call-1".into(),
            input: Some(serde_json::json!({"path": "src/lib.rs"})),
            run_id: Some("run-1".into()),
            parent_call_id: None,
        }),
        Event::new(ToolEvent::ToolCompleted {
            tool_name: "read_file".into(),
            call_id: "call-1".into(),
            is_error: false,
            output: Some("fn main() {}".into()),
            detail: None,
            run_id: Some("run-1".into()),
            parent_call_id: None,
        }),
        answered("run-1", "the root answer"),
        answered("run-2", "the reviewer verdict"),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        }),
    ];
    for event in &events {
        handle
            .append_event(Some("trial-session"), event)
            .expect("append");
    }
    drop(storage);
}

fn directory_listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn reader_for(root: &Path) -> InspectorReader {
    InspectorReader(EvaluationInspector::new(root))
}

#[test]
fn a_finished_trial_replays_its_root_run_child_run_and_tool_call() {
    // Given: the database of a finished trial with a root run, a Reviewer and a tool call.
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("evorch.db");
    record_trial_database(&database);

    // When: it is replayed.
    let load = reader_for(dir.path()).replay(&ReplayTarget {
        database: Some(database),
        conversation: None,
    });

    // Then: both runs are listed, the child under its parent, and each run's conversation is readable.
    let ReplayLoad::Ready(data) = load else {
        panic!("a current database replays: {load:?}");
    };
    let runs: Vec<_> = data
        .runs()
        .iter()
        .map(|run| {
            (
                run.run_id.as_str(),
                run.parent.as_deref(),
                run.role.as_str(),
            )
        })
        .collect();
    assert_eq!(
        runs,
        vec![
            ("run-1", None, "orchestrator"),
            ("run-2", Some("run-1"), "reviewer")
        ]
    );
    assert_eq!(data.default_run(), Some("run-1"));
    let root = format!(
        "{:?}",
        data.transcript("run-1").expect("root run").entries()
    );
    assert!(root.contains("the root answer"), "{root}");
    assert!(
        root.contains("read_file"),
        "the tool call is replayed: {root}"
    );
    let child = format!(
        "{:?}",
        data.transcript("run-2").expect("child run").entries()
    );
    assert!(child.contains("the reviewer verdict"), "{child}");
    assert!(!child.contains("the root answer"));
}

#[test]
fn replaying_leaves_the_database_and_its_directory_untouched() {
    // Given: a finished trial's database and a fingerprint of it and its directory.
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("evorch.db");
    record_trial_database(&database);
    let before = (
        std::fs::read(&database).expect("database bytes"),
        directory_listing(dir.path()),
    );

    // When: it is replayed.
    let load = reader_for(dir.path()).replay(&ReplayTarget {
        database: Some(database.clone()),
        conversation: None,
    });

    // Then: it replayed, and no byte changed and no -wal, -shm or journal file appeared.
    assert!(matches!(load, ReplayLoad::Ready(_)));
    let after = (
        std::fs::read(&database).expect("database bytes"),
        directory_listing(dir.path()),
    );
    assert_eq!(before, after);
}

#[test]
fn a_database_on_an_older_schema_is_refused_and_conversation_json_stands_in() {
    // Given: a trial database from an older evorch and the conversation the user saw.
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("evorch.db");
    {
        let old = rusqlite::Connection::open(&database).expect("old database");
        old.execute_batch("PRAGMA user_version = 1; CREATE TABLE legacy (id INTEGER);")
            .expect("old schema");
    }
    let conversation = dir.path().join("conversation.json");
    std::fs::write(
        &conversation,
        serde_json::to_vec(&[
            eval::conversation::Entry::User {
                text: "fix the bug".into(),
            },
            eval::conversation::Entry::Assistant {
                text: "fixed".into(),
            },
        ])
        .expect("conversation json"),
    )
    .expect("write conversation");
    let before = std::fs::read(&database).expect("database bytes");

    // When: it is replayed.
    let load = reader_for(dir.path()).replay(&ReplayTarget {
        database: Some(database.clone()),
        conversation: Some(conversation),
    });

    // Then: the old database is not migrated, the reason says so, and the conversation is offered.
    let ReplayLoad::Fallback {
        reason,
        conversation,
    } = load
    else {
        panic!("an older schema must not replay: {load:?}");
    };
    assert!(
        reason.contains("cannot open the trial database"),
        "{reason}"
    );
    assert!(reason.contains("schema"), "{reason}");
    assert_eq!(std::fs::read(&database).expect("database bytes"), before);
    let Artifact::Present(entries) = conversation else {
        panic!("conversation.json should be read: {conversation:?}");
    };
    assert_eq!(entries.len(), 2);
}

#[derive(Default)]
struct Mutations {
    commands: AtomicUsize,
    background: AtomicUsize,
    withdrawals: AtomicUsize,
}

struct CountingSink(Arc<Mutations>);

impl CommandSink for CountingSink {
    fn submit(&mut self, _command: WorkbenchCommand) -> Vec<LoopEvent> {
        self.0.commands.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }

    fn start_background_run(&self, _text: String) -> Option<runtime::RunId> {
        self.0.background.fetch_add(1, Ordering::SeqCst);
        None
    }

    fn withdraw_follow_up(
        &mut self,
        _thread: &str,
        _prompt_id: &str,
    ) -> Result<(), runtime::RuntimeError> {
        self.0.withdrawals.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn finished_group(database: PathBuf) -> eval::inspection::EvaluationGroupSnapshot {
    let target = member(MemberSpec {
        index: 0,
        thread: "thread-target",
        purpose: Purpose::Target,
        phase: MemberPhase::Finished,
        trials: vec![trial(TrialSpec {
            label: "baseline",
            index: 1,
            state: TrialState::Finished,
            cost: TrialCost::Reported(evaluations_support::cost(0.1)),
            database: Some(database),
        })],
        planned: 1,
        report: ReportStatus::Missing,
        markdown: false,
        cost: no_cost(),
    });
    group("g-replay", GroupPhase::Finished, vec![target])
}

#[test]
fn replay_operations_issue_no_commands_and_leave_the_live_conversation_alone() {
    // Given: a finished group whose trial database is real, and a workbench whose command sink counts.
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("evorch.db");
    record_trial_database(&database);
    let load = reader_for(dir.path()).replay(&ReplayTarget {
        database: Some(database.clone()),
        conversation: None,
    });
    let snapshot = finished_group(database);
    let reader = FakeReader::new(listing(vec![entry(snapshot.clone())])).with_group(snapshot);
    *reader.replay.lock().expect("replay") = Some(load);
    let reads = ManualSpawner::default();
    let model = EvaluationsModel::new(Some(reader.clone()), reads.spawner(), Arc::new(|| {}));
    let mutations = Arc::new(Mutations::default());
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_command_sink(Box::new(CountingSink(Arc::clone(&mutations))))
        .with_evaluations(model);
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 800.0]);
    harness.state_mut().open_evaluations_tab();
    let settle = |harness: &mut HeadlessWorkbench<DemoSource>| {
        for _ in 0..8 {
            harness.run();
            if reads.pending() == 0 {
                return;
            }
            reads.run_all();
        }
        harness.run();
    };

    // When: the user opens the group, the thread, the trial's replay and then its child run.
    settle(&mut harness);
    harness.click_label("g-replay");
    settle(&mut harness);
    harness.click_label("target · thread-target");
    settle(&mut harness);
    harness.click_label("Replay baseline-1");
    settle(&mut harness);

    // Then: the replay is shown read-only, with both runs to pick from.
    assert!(harness.has_label("read-only"));
    assert!(harness.has_label("root-agent (orchestrator)"));
    harness.click_label("  child-reviewer (reviewer)");
    assert!(
        harness.has_label("✓ read_file"),
        "the root run's tool call is drawn"
    );
    settle(&mut harness);
    assert_eq!(harness.state().evaluations().replay_run(), Some("run-2"));
    let (_, view) = harness.state().evaluations().replay().expect("a replay");
    assert!(matches!(view, ReplayView::Ready(_)));

    // And: it read the trial once, issued no command of any kind and added nothing to the
    // live conversation or its runs.
    assert_eq!(reader.replay_calls.load(Ordering::SeqCst), 1);
    assert_eq!(mutations.commands.load(Ordering::SeqCst), 0);
    assert_eq!(mutations.background.load(Ordering::SeqCst), 0);
    assert_eq!(mutations.withdrawals.load(Ordering::SeqCst), 0);
    assert!(harness.state().transcripts().run("run-1").is_none());
    assert!(harness.state().transcripts().run("run-2").is_none());
}

#[test]
fn a_replayed_trial_is_served_from_the_cache_when_opened_again() {
    // Given: a trial replayed once and closed.
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("evorch.db");
    record_trial_database(&database);
    let load = reader_for(dir.path()).replay(&ReplayTarget {
        database: Some(database.clone()),
        conversation: None,
    });
    let snapshot = finished_group(database);
    let reader = FakeReader::new(listing(vec![entry(snapshot.clone())])).with_group(snapshot);
    *reader.replay.lock().expect("replay") = Some(load);
    let reads = ManualSpawner::default();
    let mut model = EvaluationsModel::new(Some(reader.clone()), reads.spawner(), Arc::new(|| {}));
    let now = std::time::Instant::now();
    let drain = |model: &mut EvaluationsModel| {
        for _ in 0..4 {
            model.poll_once(now, true);
            reads.run_all();
        }
        model.poll_once(now, true);
    };
    drain(&mut model);
    model.select_group("g-replay");
    drain(&mut model);
    model.select_member(0);
    assert!(model.select_trial("baseline-1"));
    drain(&mut model);
    model.close_replay();

    // When: the same trial is opened again.
    assert!(model.select_trial("baseline-1"));
    drain(&mut model);

    // Then: it shows at once from the cache without reading the database again.
    let (_, view) = model.replay().expect("a replay");
    assert!(matches!(view, ReplayView::Ready(_)));
    assert_eq!(reader.replay_calls.load(Ordering::SeqCst), 1);
}
