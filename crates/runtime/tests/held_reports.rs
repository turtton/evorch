//! A subagent's report to a finished or stopped parent is held instead of
//! dropped. The parent is woken for it, except while the operator's stop holds.
mod support;

use event_bus::{
    AgentMessageEvent, AgentMessageKind, AgentRunPhase, DeliveryDisposition, EventBus, EventKind,
    EventReceiver, LifecycleEvent,
};
use providers::FinishReason;
use runtime::{AgentRuntime, Role, RunConfig, RunId, RunStore};
use std::sync::Arc;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

struct Fixture {
    runtime: AgentRuntime,
    bus: Arc<EventBus>,
    model: Arc<ScriptedModel>,
    _storage: Storage,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("held.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(512));
    let model = Arc::new(ScriptedModel::new([]));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus.clone())),
        model.clone(),
    )
    .with_sequential_run_ids()
    .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    Fixture {
        runtime,
        bus,
        model,
        _storage: storage,
        _dir: dir,
    }
}

impl Fixture {
    async fn script(&self, marker: &str, texts: &[&str]) {
        self.model
            .add_keyed(
                marker,
                texts
                    .iter()
                    .map(|text| Ok(text_response(text, FinishReason::Stop)))
                    .collect::<Vec<_>>(),
            )
            .await;
    }

    async fn gate(&self, marker: &str) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.model.gate_key(marker, Arc::clone(&gate)).await;
        gate
    }

    async fn request_text(&self, index: usize) -> String {
        serde_json::to_string(&self.model.wait_for_request(index).await).unwrap()
    }

    /// Thread ownership: a root holding it cannot be restored from disk alone.
    fn permit(&self) -> runtime::ownership::OwnerPermit {
        use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
        let registry_path = self._dir.path().join("owners.db");
        let owner = ThreadOwner::new(
            "thread".into(),
            Lease {
                owner_id: "host".into(),
                generation: 1,
                expires_at: u64::MAX,
            },
        );
        Registry::open(&registry_path)
            .unwrap()
            .start(&owner)
            .unwrap();
        OwnerPermit {
            registry_path,
            thread_id: owner.thread_id,
            lease: owner.lease,
            run_id: None,
        }
    }
}

async fn wake_requested(receiver: &mut EventReceiver, root: RunId) {
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::RootWakeRequested { run_id }) =
            receiver.recv().await.unwrap().kind
            && run_id == root.to_string()
        {
            return;
        }
    }
}

#[tokio::test]
async fn woken_root_receives_the_report_of_a_child_that_outlived_it() {
    // Given: a root finished while its background child was still working.
    let fixture = fixture().await;
    fixture
        .script("ROOT", &["root first", "root handled the report"])
        .await;
    fixture.script("CHILD", &["child result"]).await;
    let child_gate = fixture.gate("CHILD").await;
    let mut events = fixture.bus.subscribe();
    let root = fixture.runtime.delegate_background(
        Role::Orchestrator,
        "ROOT".into(),
        RunConfig::default(),
    );
    let child = fixture
        .runtime
        .delegate_background_as_child(root, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(root).await.unwrap(),
        AgentRunPhase::Done
    );
    // When: the child finishes and the host resumes the root it was asked to wake.
    child_gate.notify_one();
    assert_eq!(
        fixture.runtime.wait(child).await.unwrap(),
        AgentRunPhase::Done
    );
    wake_requested(&mut events, root).await;
    fixture
        .runtime
        .wake_chat(root, RunConfig::default())
        .unwrap();
    // Then: the root's next request carries the child's report.
    let request = fixture.request_text(2).await;
    assert!(request.contains("child result"), "{request}");
    fixture.runtime.cancel(root).unwrap();
    fixture.runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn operator_stopped_root_keeps_the_report_until_the_operator_resumes() {
    // Given: the operator stopped the root while its child kept running.
    let fixture = fixture().await;
    fixture
        .script("ROOT", &["root waits", "root handled the report"])
        .await;
    fixture.script("CHILD", &["child result"]).await;
    let child_gate = fixture.gate("CHILD").await;
    let mut events = fixture.bus.subscribe();
    let root = fixture.runtime.delegate_background(
        Role::Orchestrator,
        "ROOT".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Waiting,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == root.to_string()
        {
            break;
        }
    }
    let child = fixture
        .runtime
        .delegate_background_as_child(root, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    fixture
        .runtime
        .stop(root, runtime::StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(root).await.unwrap(),
        AgentRunPhase::Stopped
    );
    // When: the child finishes.
    child_gate.notify_one();
    assert_eq!(
        fixture.runtime.wait(child).await.unwrap(),
        AgentRunPhase::Done
    );
    // Then: the report is held, and the stop suppresses the wake. The wake
    // decision is made right after the report is held, without spawning work,
    // so a probe emitted afterwards bounds the events to inspect.
    loop {
        if let EventKind::AgentMessage(AgentMessageEvent::Delivered {
            message,
            disposition: DeliveryDisposition::Held,
        }) = events.recv().await.unwrap().kind
            && message.recipient_run_id == root.to_string()
        {
            break;
        }
    }
    fixture.bus.emit(event_bus::Event::new(
        LifecycleEvent::BackgroundTaskCompleted {
            task_id: "probe-end".into(),
        },
    ));
    loop {
        match events.recv().await.unwrap().kind {
            EventKind::Lifecycle(LifecycleEvent::RootWakeRequested { .. }) => {
                panic!("an operator-stopped root must not be woken")
            }
            EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { task_id })
                if task_id == "probe-end" =>
            {
                break;
            }
            _ => {}
        }
    }
    // When: the operator resumes the root, it receives the held report.
    fixture
        .runtime
        .resume_chat(root, RunConfig::default())
        .unwrap();
    let request = fixture.request_text(2).await;
    assert!(request.contains("child result"), "{request}");
    fixture.runtime.cancel(root).unwrap();
    fixture.runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn finished_subagent_is_woken_under_its_live_parents_authority() {
    // Given: a live root, and a middle orchestrator that finished while its
    // own background child was still working.
    let fixture = fixture().await;
    let _root_gate = fixture.gate("ROOT").await;
    fixture
        .script("MIDDLE", &["middle done", "middle handled the report"])
        .await;
    let middle_gate = fixture.gate("MIDDLE").await;
    fixture.script("GRAND", &["grandchild result"]).await;
    let grand_gate = fixture.gate("GRAND").await;
    let root = fixture.runtime.delegate_background(
        Role::Orchestrator,
        "ROOT".into(),
        RunConfig::default(),
    );
    fixture.model.wait_for_request(0).await;
    let middle = fixture
        .runtime
        .delegate_background_as_child(root, Role::Orchestrator, "MIDDLE", RunConfig::default())
        .unwrap();
    fixture.model.wait_for_request(1).await;
    let grandchild = fixture
        .runtime
        .delegate_background_as_child(middle, Role::Worker, "GRAND", RunConfig::default())
        .unwrap();
    fixture.model.wait_for_request(2).await;
    middle_gate.notify_one();
    assert_eq!(
        fixture.runtime.wait(middle).await.unwrap(),
        AgentRunPhase::Done
    );
    // When: the grandchild finishes.
    middle_gate.notify_one();
    grand_gate.notify_one();
    assert_eq!(
        fixture.runtime.wait(grandchild).await.unwrap(),
        AgentRunPhase::Done
    );
    // Then: the middle orchestrator continues with the report.
    let request = fixture.request_text(3).await;
    assert!(request.contains("grandchild result"), "{request}");
    assert_eq!(
        fixture.runtime.wait(middle).await.unwrap(),
        AgentRunPhase::Done
    );
    fixture.runtime.cancel(root).unwrap();
    fixture.runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn subagent_message_to_a_finished_root_is_held_and_wakes_it() {
    let fixture = fixture().await;
    fixture.script("ROOT", &["root done"]).await;
    let _child_gate = fixture.gate("CHILD").await;
    let mut events = fixture.bus.subscribe();
    let root = fixture.runtime.delegate_background(
        Role::Orchestrator,
        "ROOT".into(),
        RunConfig {
            ownership: Some(fixture.permit()),
            ..Default::default()
        },
    );
    let child = fixture
        .runtime
        .delegate_background_as_child(root, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(root).await.unwrap(),
        AgentRunPhase::Done
    );
    let id = fixture
        .runtime
        .send_agent_message(child, root, AgentMessageKind::Send, "progress", None)
        .unwrap();
    loop {
        if let EventKind::AgentMessage(AgentMessageEvent::Delivered {
            message,
            disposition,
        }) = events.recv().await.unwrap().kind
            && message.message_id == id
        {
            assert_eq!(disposition, DeliveryDisposition::Held);
            break;
        }
    }
    wake_requested(&mut events, root).await;
    fixture.runtime.cancel(child).unwrap();
    fixture.runtime.wait(child).await.unwrap();
}
