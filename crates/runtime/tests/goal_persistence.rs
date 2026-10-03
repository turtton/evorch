mod support;

use std::sync::Arc;

use event_bus::{
    EventBus, EventKind, EventReceiver, GoalReference, GoalState, OrchestratorEvent, RecvError,
    RunPurpose,
};
use runtime::orchestration::delivery::FixtureDeliveryAdapter;
use runtime::orchestration::ledger::{GoalLedger, OrchestrationSettings};
use runtime::orchestration::supervisor::{GoalSpec, GoalSupervisor};
use runtime::{AgentRuntime, Role, RunConfig};
use sandbox::DirectSandbox;
use storage::{Database, Storage, StorageConfig, StorageHandle};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};
use tools::ToolExecutor;

use support::ScriptedModel;

fn runtime_with(bus: Arc<EventBus>) -> AgentRuntime {
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(
        bus,
        executor,
        Arc::new(ScriptedModel::gated([], Arc::new(Notify::new()))),
    )
}

fn spawn_storage_bridge(
    bus: &EventBus,
    handle: StorageHandle,
) -> (
    tokio::task::JoinHandle<()>,
    mpsc::UnboundedReceiver<OrchestratorEvent>,
) {
    let mut subscriber = bus.subscribe();
    let (persisted, events) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            match subscriber.recv().await {
                Ok(event) => {
                    handle
                        .append_event(Some("goal-persistence"), &event)
                        .expect("persist event");
                    if let EventKind::Orchestrator(event) = event.kind {
                        let _ = persisted.send(event);
                    }
                }
                Err(RecvError::Lagged(skipped)) => panic!("storage bridge lagged by {skipped}"),
                Err(RecvError::Closed) => return,
            }
        }
    });
    (task, events)
}

async fn wait_for_event(events: &mut EventReceiver, expected: impl Fn(&OrchestratorEvent) -> bool) {
    loop {
        if let EventKind::Orchestrator(event) = events.recv().await.expect("supervisor event").kind
            && expected(&event)
        {
            return;
        }
    }
}

fn spec() -> GoalSpec {
    GoalSpec {
        session_id: "goal-persistence".into(),
        project_id: "evorch".into(),
        thread_id: "thread-73".into(),
        goal: "implement issue 73".into(),
        references: vec![GoalReference {
            kind: "issue".into(),
            value: "73".into(),
        }],
        constraints: vec!["durable".into()],
        repo: "turtton/evorch".into(),
        base_ref: "main".into(),
    }
}

#[tokio::test]
async fn adopt_marks_active_goal_paused_with_recovery_reason() {
    let bus = Arc::new(EventBus::new(256));
    let runtime = runtime_with(Arc::clone(&bus));
    let first = GoalSupervisor::spawn(
        runtime.clone(),
        Arc::clone(&bus),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );
    let root = runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    let goal_id = first.create_goal(spec(), root);
    first.synchronize().await.expect("goal created");
    let snapshot = first.snapshot(&goal_id).expect("snapshot");
    let mut events = bus.subscribe();
    let fresh_runtime = runtime_with(Arc::clone(&bus));
    let adopted = GoalSupervisor::spawn(
        fresh_runtime,
        Arc::clone(&bus),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );

    adopted.adopt(vec![(snapshot, vec![])]).expect("adopt");
    adopted.synchronize().await.expect("goal adopted");

    let current = adopted.snapshot(&goal_id).expect("adopted snapshot");
    assert_eq!(current.state, GoalState::Paused);
    assert!(current.detached);
    wait_for_event(&mut events, |event| {
        matches!(event,
            OrchestratorEvent::GoalStateChanged { goal_id: changed, reason, .. }
                if changed == &goal_id && reason == "recovered-after-restart"
        )
    })
    .await;
}

#[tokio::test]
async fn resume_of_adopted_goal_dispatches_recovery_run_not_child_continuation() {
    let source_bus = Arc::new(EventBus::new(256));
    let source_runtime = runtime_with(Arc::clone(&source_bus));
    let source = GoalSupervisor::spawn(
        source_runtime.clone(),
        Arc::clone(&source_bus),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );
    let old_root =
        source_runtime.delegate_background(Role::Orchestrator, "OLD".into(), RunConfig::default());
    let goal_id = source.create_goal(spec(), old_root);
    source.synchronize().await.expect("goal created");
    let snapshot = source.snapshot(&goal_id).expect("snapshot");

    let bus = Arc::new(EventBus::new(256));
    let runtime = runtime_with(Arc::clone(&bus));
    let handle = GoalSupervisor::spawn(
        runtime,
        Arc::clone(&bus),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );
    let mut events = bus.subscribe();
    handle.adopt(vec![(snapshot, vec![])]).expect("adopt");
    handle.synchronize().await.expect("goal adopted");
    handle.resume(&goal_id).expect("resume");
    handle.synchronize().await.expect("goal resumed");
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            OrchestratorEvent::RunAttached {
                goal_id: attached,
                parent_run_id: None,
                purpose: RunPurpose::Recovery { .. },
                ..
            } if attached == &goal_id
        )
    })
    .await;
    assert!(!handle.snapshot(&goal_id).expect("snapshot").detached);
}

#[tokio::test]
async fn goal_events_round_trip_and_resume_dispatches_continuation() {
    let temp = TempDir::new().expect("tempdir");
    let config = StorageConfig {
        db_path: temp.path().join("goals.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage");
    let bus = Arc::new(EventBus::new(512));
    let runtime = runtime_with(Arc::clone(&bus));
    let (bridge, mut persisted) = spawn_storage_bridge(&bus, storage.handle());
    let handle = GoalSupervisor::spawn(
        runtime.clone(),
        Arc::clone(&bus),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );
    let root = runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    let goal_id = handle.create_goal(spec(), root);
    handle.synchronize().await.expect("goal created");
    handle.pause(&goal_id).expect("pause");
    // append_event acknowledges the committed transaction. Waiting for this
    // exact transition also covers all earlier events on the bridge receiver.
    loop {
        if matches!(persisted.recv().await.expect("persisted goal event"),
            OrchestratorEvent::GoalStateChanged { goal_id: changed, to: GoalState::Paused, .. }
                if changed == goal_id
        ) {
            break;
        }
    }
    let expected = handle.snapshot(&goal_id).expect("snapshot");
    bridge.abort();
    let _ = bridge.await;
    storage.close();

    let stored = Database::open(&config)
        .expect("reopen")
        .events_all_ordered()
        .expect("events");
    let orchestrator = stored
        .iter()
        .filter_map(|stored| match &stored.event.kind {
            EventKind::Orchestrator(event) => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    let replayed =
        GoalLedger::replay_checked(orchestrator.into_iter()).expect("valid persisted history");
    let restored = replayed.get(&goal_id).expect("restored").snapshot().clone();
    assert_eq!(restored, expected);

    let fresh_bus = Arc::new(EventBus::new(256));
    let fresh_runtime = runtime_with(Arc::clone(&fresh_bus));
    let fresh = GoalSupervisor::spawn(
        fresh_runtime,
        Arc::clone(&fresh_bus),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );
    let mut events = fresh_bus.subscribe();
    fresh.adopt(vec![(restored, vec![])]).expect("adopt");
    fresh.synchronize().await.expect("goal adopted");
    fresh.resume(&goal_id).expect("resume");
    wait_for_event(&mut events, |event| {
        matches!(event,
            OrchestratorEvent::ContinuationDispatched { goal_id: dispatched, .. }
                if dispatched == &goal_id
        )
    })
    .await;
}
