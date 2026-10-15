use super::*;

pub(super) struct AdmissionModel {
    pub(super) reject: AtomicBool,
}

#[async_trait::async_trait]
impl AgentModel for AdmissionModel {
    fn requires_admission(&self) -> bool {
        true
    }

    async fn admit(&self, _: &crate::AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        if self.reject.load(Ordering::Relaxed) {
            return Err(RuntimeError::Model {
                reason: "admission rejected".into(),
            });
        }
        Ok(())
    }

    async fn complete(
        &self,
        context: &crate::AgentInvocationContext,
        role: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        CompletingModel
            .complete(context, role, messages, tools)
            .await
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn restored_event_observes_registered_entry_before_execution() {
    // Given: a terminal goal on a current-thread executor.
    let fixture = Fixture::new();
    let run = fixture.terminal().await;
    let mut events = fixture.runtime.shared.bus.subscribe();
    // When: continuing with new authority and receiving the first restore event.
    fixture
        .runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap();
    let event = events.recv().await.unwrap();
    // Then: publication observes the new Pending entry, never the old terminal entry.
    assert!(
        matches!(event.kind, event_bus::EventKind::Lifecycle(LifecycleEvent::AgentRunRestored {
        run_id, restored_by, message_id
    }) if run_id == run.to_string() && restored_by == "user" && !message_id.is_empty())
    );
    let entry = fixture.runtime.entry(run).unwrap();
    assert_eq!(*entry.phase_rx.borrow(), AgentRunPhase::Pending);
    assert!(entry.config.interactive && entry.config.keep_alive);
}

#[tokio::test]
async fn restored_event_is_absent_when_admission_fails() {
    // Given: a terminal goal and a provider that rejects its next admission.
    let model = Arc::new(AdmissionModel {
        reject: AtomicBool::new(false),
    });
    let fixture = Fixture::with_model(model.clone());
    let run = fixture.terminal().await;
    model.reject.store(true, Ordering::Relaxed);
    let mut events = fixture.runtime.shared.bus.subscribe();
    // When: the continuation fails admission.
    fixture
        .runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap();
    assert!(fixture.runtime.wait_admission(run).await.is_err());
    // Then: no restore was announced and the old entry remains terminal.
    assert!(events.drain_pending_snapshot().is_empty());
    assert_eq!(
        *fixture.runtime.entry(run).unwrap().phase_rx.borrow(),
        AgentRunPhase::Done
    );
}

struct HeldAdmissionModel {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl AgentModel for HeldAdmissionModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "held".into()
    }
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(&self, _: &crate::AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn answer_during_chat_admission_updates_the_preinstalled_seed() {
    for new_id in [false, true] {
        let fixture = Fixture::new();
        let run = fixture
            .runtime
            .delegate_chat(
                "admission",
                Role::Worker,
                "initial".into(),
                RunConfig::default(),
            )
            .unwrap();
        // Store a pending question while the synchronous registration is live.
        let question = fixture
            .runtime
            .request_user_question(run, "Which scope?".into(), vec![], false)
            .unwrap();
        fixture.runtime.wait(run).await.unwrap();
        let config = storage::StorageConfig {
            db_path: fixture._dir.path().join("goal.sqlite3"),
            ..Default::default()
        };
        let model = Arc::new(HeldAdmissionModel {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let bus = Arc::new(EventBus::new(128));
        // A fresh runtime has no registered source context to accidentally share.
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                .with_run_store(crate::RunStore::open(&config, fixture.storage.handle()).unwrap());
        let run = if new_id {
            runtime
                .delegate_chat(
                    "admission",
                    Role::Worker,
                    "followup".into(),
                    RunConfig::default(),
                )
                .unwrap()
        } else {
            runtime.resume_chat(run, RunConfig::default()).unwrap()
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), model.entered.notified())
            .await
            .unwrap();
        let seeded = runtime.shared.review_runs.lock().unwrap()[&run].clone();
        runtime
            .answer_user_question(&question.id, "Only the requested scope")
            .unwrap();
        model.release.notify_one();
        runtime.wait_admission(run).await.unwrap();
        let installed = runtime.shared.review_runs.lock().unwrap()[&run].clone();
        assert!(Arc::ptr_eq(&seeded.user_requests, &installed.user_requests));
        assert!(installed.stamp().synced);
        assert!(
            installed
                .requests()
                .0
                .iter()
                .any(|request| request.text == "Only the requested scope")
        );
        runtime.cancel(run).unwrap();
    }
}
