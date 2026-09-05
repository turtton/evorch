mod support;

use std::sync::Arc;

use event_bus::{
    EventBus, EventKind, GoalReference, GoalState, OrchestratorEvent, RecvError, RunPurpose,
};
use runtime::orchestration::delivery::FixtureDeliveryAdapter;
use runtime::orchestration::ledger::{GoalLedger, OrchestrationSettings};
use runtime::orchestration::supervisor::{GoalSpec, GoalSupervisor};
use runtime::{AgentRuntime, Role, RunConfig};
use sandbox::DirectSandbox;
use storage::{Database, Storage, StorageConfig, StorageHandle};
use tempfile::TempDir;
use tokio::sync::Notify;
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

fn spawn_storage_bridge(bus: &EventBus, handle: StorageHandle) -> tokio::task::JoinHandle<()> {
    let mut subscriber = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match subscriber.recv().await {
                Ok(event) => handle
                    .append_event(Some("goal-persistence"), &event)
                    .expect("persist event"),
                Err(RecvError::Lagged(skipped)) => panic!("storage bridge lagged by {skipped}"),
                Err(RecvError::Closed) => return,
            }
        }
    })
}

async fn settle() {
    for _ in 0..24 {
        tokio::task::yield_now().await;
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
    settle().await;
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
    settle().await;

    let current = adopted.snapshot(&goal_id).expect("adopted snapshot");
    assert_eq!(current.state, GoalState::Paused);
    assert!(current.detached);
    let mut found = false;
    while let Ok(Ok(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(10), events.recv()).await
    {
        if matches!(event.kind, EventKind::Orchestrator(OrchestratorEvent::GoalStateChanged { ref reason, .. }) if reason == "recovered-after-restart")
        {
            found = true;
            break;
        }
    }
    assert!(found);
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
    settle().await;
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
    settle().await;
    handle.resume(&goal_id).expect("resume");
    settle().await;

    let mut recovery = false;
    while let Ok(Ok(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(10), events.recv()).await
    {
        if matches!(
            event.kind,
            EventKind::Orchestrator(OrchestratorEvent::RunAttached {
                parent_run_id: None,
                purpose: RunPurpose::Recovery { .. },
                ..
            })
        ) {
            recovery = true;
            break;
        }
    }
    assert!(recovery);
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
    let bridge = spawn_storage_bridge(&bus, storage.handle());
    let handle = GoalSupervisor::spawn(
        runtime.clone(),
        Arc::clone(&bus),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );
    let root = runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    let goal_id = handle.create_goal(spec(), root);
    settle().await;
    handle.pause(&goal_id).expect("pause");
    settle().await;
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
    let replayed = GoalLedger::replay(orchestrator.into_iter());
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
    settle().await;
    fresh.resume(&goal_id).expect("resume");
    settle().await;
    let mut dispatched = false;
    while let Ok(Ok(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(10), events.recv()).await
    {
        if matches!(
            event.kind,
            EventKind::Orchestrator(OrchestratorEvent::ContinuationDispatched { .. })
        ) {
            dispatched = true;
            break;
        }
    }
    assert!(dispatched);
}
