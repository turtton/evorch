//! Tool calls whose run ended without a result stop showing as running.

use event_bus::{AgentRunPhase, Event, LifecycleEvent, ToolEvent};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use gui::model::transcript::{ToolStatus, TranscriptEntry};
use runtime::{AgentInspection, AgentSummary, RunId};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

struct EmptySource;

impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }
    fn inspect(&self, _: RunId) -> Option<AgentInspection> {
        None
    }
}

const RUN: &str = "run-target";

fn state(root: &std::path::Path) -> WorkbenchState<EmptySource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("p");
    sidebar
        .add_project(project.clone(), "Project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("target"), project, "Target")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("target")).unwrap();
    let mut json = serde_json::to_value(sidebar).unwrap();
    json["threads"][0]["run_ids"] = serde_json::json!([RUN]);
    WorkbenchState::new(EmptySource, &UiSettings::default())
        .unwrap()
        .with_sidebar(serde_json::from_value(json).unwrap())
        .with_sidebar_path(root.join("sidebar.json"))
}

/// What the store kept of a run besides its unfinished call.
#[derive(Clone, Copy, Debug)]
enum Record {
    /// Its phase events and a checkpoint snapshot.
    Checkpointed,
    /// Its phase events, but no snapshot to restore its phase from.
    NoSnapshot,
    /// A snapshot, but no phase event, as after an event subscriber lagged.
    NoPhase,
}

/// A run whose process exited mid-call: the call started, and no completion
/// or terminal phase was recorded.
fn persist_unfinished_call(storage: &storage::Storage, record: Record) {
    let started = Event::new(LifecycleEvent::AgentRunStarted {
        run_id: RUN.into(),
        parent_run_id: None,
        agent_name: "chat:Worker:target".into(),
        role: "Worker".into(),
    });
    let running = Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: RUN.into(),
        from: AgentRunPhase::Pending,
        to: AgentRunPhase::Running,
        reason: None,
    });
    let call = Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: "call-lost".into(),
        input: Some(serde_json::json!({"command": "sleep 600"})),
        run_id: Some(RUN.into()),
        parent_call_id: None,
    });
    let events = match record {
        Record::Checkpointed | Record::NoSnapshot => vec![started, running, call],
        Record::NoPhase => vec![started, call],
    };
    for event in events {
        storage.handle().append_event(Some("gui"), &event).unwrap();
    }
    if matches!(record, Record::NoSnapshot) {
        return;
    }
    storage
        .handle()
        .upsert_run_context(&storage::RunContextRecord {
            run_id: RUN.into(),
            role: "Worker".into(),
            name: "chat:Worker:target".into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json: "[]".into(),
            checkpoints_json: "[]".into(),
            terminal_phase: "Checkpoint".into(),
            restorable: true,
            updated_at_ns: 1,
            host_inputs_json: "[]".into(),
        })
        .unwrap();
}

fn tool_status(state: &WorkbenchState<EmptySource>) -> Option<ToolStatus> {
    state
        .transcript()
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Tool {
                call_id, status, ..
            } if call_id == "call-lost" => Some(status.clone()),
            _ => None,
        })
}

#[test]
fn a_restored_orphan_run_interrupts_its_unfinished_call() {
    // Given: a recorded run that exited mid-call, with no live owner.
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    persist_unfinished_call(&storage, Record::Checkpointed);
    storage.close();
    let registry = dir.path().join("owners.db");
    drop(runtime::ownership::Registry::open(&registry).unwrap());
    let database = storage::Database::open(&config).unwrap();

    // When: history is restored without an ownership registry, the call keeps
    // running because nothing proves that the run ended.
    let mut unknown = state(dir.path());
    unknown.restore_history(&database).unwrap();
    assert_eq!(tool_status(&unknown), Some(ToolStatus::Running));

    // When: ownership shows the run is orphaned and reconciles it as stopped.
    let mut restored = state(dir.path());
    restored
        .restore_history_with_ownership(&database, &registry)
        .unwrap();

    // Then: its unfinished call is interrupted, and the card says so without a spinner.
    assert_eq!(
        restored.thread_phases()[RUN],
        workspace_ui::ThreadRunPhase::Stopped
    );
    assert_eq!(tool_status(&restored), Some(ToolStatus::Interrupted));
    let mut harness = HeadlessWorkbench::new(restored, [1000.0, 700.0]);
    for _ in 0..3 {
        harness.step();
    }
    let label = format!(
        "⏹ Shell sleep 600 · {}",
        gui::panes::transcript_tool::INTERRUPTED_META
    );
    assert!(harness.has_label(&label), "missing {label:?}");
}

fn recorded(dir: &std::path::Path, record: Record) -> storage::Database {
    let config = storage::StorageConfig {
        db_path: dir.join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    persist_unfinished_call(&storage, record);
    storage.close();
    storage::Database::open(&config).unwrap()
}

/// A responsive process that owns `thread` and is running `run` there.
fn live_owner(
    dir: &std::path::Path,
    thread: &str,
    run: &str,
) -> (
    runtime::ownership::OwnerHost,
    runtime::ownership::OwnerPermit,
) {
    let host = runtime::ownership::OwnerHost::open(
        dir,
        Default::default(),
        std::sync::Arc::new(event_bus::EventBus::new(32)),
    )
    .unwrap();
    let mut permit = host.start(thread).unwrap();
    permit.run_id = Some(run.into());
    permit.begin_turn().unwrap();
    (host, permit)
}

fn running_calls(state: &WorkbenchState<EmptySource>) -> usize {
    state
        .transcript()
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                TranscriptEntry::Tool {
                    status: ToolStatus::Running,
                    ..
                }
            )
        })
        .count()
}

#[test]
fn an_orphan_call_is_interrupted_even_when_its_phase_cannot_be_restored() {
    for record in [Record::NoSnapshot, Record::NoPhase] {
        for unrelated_owner in [false, true] {
            // Given: an orphan call whose run lacks a snapshot or a phase event,
            // and possibly a live process that owns only another thread.
            let dir = tempfile::tempdir().unwrap();
            let database = recorded(dir.path(), record);
            let registry = dir.path().join("owners.db");
            drop(runtime::ownership::Registry::open(&registry).unwrap());
            let _unrelated =
                unrelated_owner.then(|| live_owner(dir.path(), "unrelated", "unrelated-run"));

            // When: history is restored with the ownership registry.
            let mut restored = state(dir.path());
            restored
                .restore_history_with_ownership(&database, &registry)
                .unwrap();

            // Then: the run keeps its replayed phase, but no call is left running.
            let case = format!("{record:?}, unrelated owner: {unrelated_owner}");
            assert_eq!(
                restored.thread_phases().get(RUN),
                matches!(record, Record::NoSnapshot)
                    .then_some(&workspace_ui::ThreadRunPhase::Running),
                "{case}"
            );
            assert_eq!(
                tool_status(&restored),
                Some(ToolStatus::Interrupted),
                "{case}"
            );
            assert_eq!(running_calls(&restored), 0, "{case}");
        }
    }
}

#[test]
fn a_call_in_a_thread_that_a_live_process_owns_keeps_running() {
    for record in [Record::Checkpointed, Record::NoSnapshot, Record::NoPhase] {
        for between_rounds in [false, true] {
            // Given: an unfinished call in a thread that a live process owns.
            let dir = tempfile::tempdir().unwrap();
            let database = recorded(dir.path(), record);
            let (_host, permit) = live_owner(dir.path(), "target", RUN);
            if between_rounds {
                // A checkpoint drops the run from active_runs while it continues.
                permit.checkpoint(&[]).unwrap();
            }

            // When: history is restored with the ownership registry.
            let mut restored = state(dir.path());
            restored
                .restore_history_with_ownership(&database, &permit.registry_path)
                .unwrap();

            // Then: the owner may still report the result, so the call keeps running.
            assert_eq!(
                tool_status(&restored),
                Some(ToolStatus::Running),
                "{record:?}, between rounds: {between_rounds}"
            );
        }
    }
}

#[test]
fn a_call_left_active_by_an_owner_that_exited_mid_call_is_interrupted() {
    for record in [Record::NoSnapshot, Record::NoPhase] {
        for owner_thread in ["target", "unrelated"] {
            // Given: an owner that began a turn of the run and exited mid-call
            // without a checkpoint, so its record still lists the run as active.
            let dir = tempfile::tempdir().unwrap();
            let database = recorded(dir.path(), record);
            let (host, permit) = live_owner(dir.path(), owner_thread, RUN);
            drop(host);
            let case = format!("{record:?}, owner of {owner_thread}");
            let owners = runtime::ownership::Registry::open_readonly(&permit.registry_path)
                .unwrap()
                .list()
                .unwrap();
            assert!(
                owners.iter().any(|owner| owner.active_runs.contains(RUN)),
                "{case}"
            );

            // When: history is restored with the ownership registry.
            let mut restored = state(dir.path());
            restored
                .restore_history_with_ownership(&database, &permit.registry_path)
                .unwrap();

            // Then: the stale active run does not keep its call running.
            assert_eq!(
                tool_status(&restored),
                Some(ToolStatus::Interrupted),
                "{case}"
            );
            assert_eq!(running_calls(&restored), 0, "{case}");
        }
    }
}

/// Ownership that cannot prove that the owner executing the run is dead.
#[derive(Clone, Copy, Debug)]
enum Unproven {
    MissingRegistry,
    CorruptRegistry,
    /// An owner of the thread, running the run, whose socket path loops
    /// (ELOOP): an endpoint failure of unknown cause.
    LoopingSocket(&'static str),
    /// An owner of the thread, running the run, whose ID names no socket.
    InvalidOwnerId(&'static str),
    /// A live process running the run in a turn of another thread.
    LiveOtherThread,
}

fn unproven_ownership(
    dir: &std::path::Path,
    case: Unproven,
) -> (
    std::path::PathBuf,
    Option<(
        runtime::ownership::OwnerHost,
        runtime::ownership::OwnerPermit,
    )>,
) {
    let registry = dir.join("owners.db");
    match case {
        Unproven::MissingRegistry => {}
        Unproven::CorruptRegistry => std::fs::write(&registry, "invalid database").unwrap(),
        Unproven::LoopingSocket(thread) | Unproven::InvalidOwnerId(thread) => {
            let looping = matches!(case, Unproven::LoopingSocket(_));
            let mut owner = runtime::ownership::ThreadOwner::new(
                thread.into(),
                runtime::ownership::Lease {
                    owner_id: if looping {
                        "a".repeat(32)
                    } else {
                        "../invalid-owner".into()
                    },
                    generation: 1,
                    expires_at: 0,
                },
            );
            owner.active_turn = true;
            owner.active_runs.insert(RUN.into());
            runtime::ownership::Registry::open(&registry)
                .unwrap()
                .start(&owner)
                .unwrap();
            if looping {
                let socket = dir.join(format!("{}.sock", owner.lease.owner_id));
                std::os::unix::fs::symlink(&socket, &socket).unwrap();
            }
        }
        Unproven::LiveOtherThread => {
            return (registry, Some(live_owner(dir, "unrelated", RUN)));
        }
    }
    (registry, None)
}

#[test]
fn a_call_keeps_running_while_ownership_cannot_prove_its_owner_dead() {
    for record in [Record::NoSnapshot, Record::NoPhase] {
        for case in [
            Unproven::MissingRegistry,
            Unproven::CorruptRegistry,
            Unproven::LoopingSocket("target"),
            Unproven::InvalidOwnerId("target"),
            Unproven::LoopingSocket("unrelated"),
            Unproven::InvalidOwnerId("unrelated"),
            Unproven::LiveOtherThread,
        ] {
            // Given: an unfinished call whose owner may still be executing it.
            let dir = tempfile::tempdir().unwrap();
            let database = recorded(dir.path(), record);
            let (registry, _owner) = unproven_ownership(dir.path(), case);

            // When: history is restored with that ownership.
            let mut restored = state(dir.path());
            restored
                .restore_history_with_ownership(&database, &registry)
                .unwrap();

            // Then: the call keeps running, because it may still report a result.
            assert_eq!(
                tool_status(&restored),
                Some(ToolStatus::Running),
                "{record:?}, {case:?}"
            );
        }
    }
}
