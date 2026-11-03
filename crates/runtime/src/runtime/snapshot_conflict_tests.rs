//! A save that lost its compare-and-swap must refuse the whole operation: no
//! run is registered, nothing is announced, and the newer snapshot stays.
use super::*;
use crate::restore::revision::conflict_support::{drain, replace_snapshot};
use event_bus::{AgentMessageKind, EventKind, LifecycleEvent};

/// The root never answers; every other run completes at once.
#[derive(Default)]
struct Model {
    completed: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        let root = messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, providers::ContentBlock::Text { text } if text == "ROOT"));
        if root {
            return std::future::pending().await;
        }
        self.completed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(providers::ChatResponse {
            usage_reported: true,
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: providers::Usage::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "conflict".into()
    }
}

struct Fixture {
    runtime: AgentRuntime,
    model: Arc<Model>,
    storage: storage::Storage,
    root: RunId,
    child: RunId,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// A live root and a finished child with a saved snapshot.
    async fn new() -> Self {
        test_support::isolated_state_dir();
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("conflict.sqlite3"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(256));
        let model = Arc::new(Model::default());
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let root =
            runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
        let child = runtime
            .delegate_background_as_child(root, Role::Worker, "CHILD", RunConfig::default())
            .unwrap();
        assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
        Self {
            runtime,
            model,
            storage,
            root,
            child,
            _dir: dir,
        }
    }

    fn bundle(&self) -> storage::thread_index::SnapshotBundle {
        self.runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_bundle(self.child)
            .unwrap()
            .unwrap()
    }

    /// Another save of the child lands once the operation has read its snapshot.
    fn replace_at(&self, point: cancellation::HookPoint) {
        let before = self.bundle();
        let handle = self.storage.handle();
        self.runtime.on_execution(
            point,
            &self.runtime.execution_snapshot(self.child).unwrap(),
            move || replace_snapshot(&handle, &before),
        );
    }

    fn assert_refused(
        &self,
        before: &storage::thread_index::SnapshotBundle,
        events: &[event_bus::Event],
    ) {
        let after = self.bundle();
        assert_eq!(after.snapshot_revision, before.snapshot_revision + 1);
        assert_eq!(after.record.updated_at_ns, before.record.updated_at_ns + 1);
        assert!(after.record.restorable, "the snapshot was not consumed");
        assert_eq!(
            *self.runtime.entry(self.child).unwrap().phase_rx.borrow(),
            AgentRunPhase::Done,
            "the child was not registered again"
        );
        assert_eq!(
            self.model
                .completed
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the child did not run again"
        );
        let child = self.child.to_string();
        assert!(
            !events.iter().any(|event| match &event.kind {
                EventKind::Lifecycle(
                    LifecycleEvent::AgentRunRestored { run_id, .. }
                    | LifecycleEvent::AgentRunStarted { run_id, .. },
                ) => *run_id == child,
                _ => false,
            }),
            "{events:?}"
        );
    }
}

#[tokio::test]
async fn message_restore_that_lost_to_a_newer_save_is_refused_without_registering() {
    let fixture = Fixture::new().await;
    let before = fixture.bundle();
    let bus = fixture.runtime.shared.bus.clone();
    let mut subscriber = bus.subscribe();
    fixture.replace_at(cancellation::HookPoint::RestoreMessagePrepared);
    let result = fixture.runtime.prepare_delivery(
        fixture.root,
        fixture.child,
        AgentMessageKind::Send,
        "resume".into(),
        None,
    );
    let Err(RuntimeError::RunRestoreFailed {
        reason: crate::RunRestoreFailure::SnapshotConsumeFailed(reason),
        ..
    }) = result
    else {
        panic!("a restore against a replaced snapshot must fail: {result:?}");
    };
    assert!(reason.contains("replaced"), "{reason}");
    let events = drain(&bus, &mut subscriber).await;
    fixture.assert_refused(&before, &events);
}

#[tokio::test]
async fn answered_child_continuation_that_lost_to_a_newer_save_is_refused_without_registering() {
    let fixture = Fixture::new().await;
    let question = fixture
        .runtime
        .request_user_question(fixture.child, "Which one?".into(), vec![], false)
        .unwrap();
    let before = fixture.bundle();
    let bus = fixture.runtime.shared.bus.clone();
    let mut subscriber = bus.subscribe();
    fixture.replace_at(cancellation::HookPoint::ContinuationSnapshotPrepared);
    let error = fixture
        .runtime
        .answer_subagent_question(fixture.root, &question.id, "A")
        .await
        .unwrap_err();
    assert!(error.contains("replaced"), "{error}");
    let events = drain(&bus, &mut subscriber).await;
    fixture.assert_refused(&before, &events);
}
