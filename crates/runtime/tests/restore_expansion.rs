mod support;

use event_bus::{AgentMessageKind, AgentRunPhase, EventBus};
use providers::FinishReason;
use runtime::{AgentRuntime, CoordinationTopology, Role, RunConfig, RunId, RunStore};
use std::sync::Arc;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response};
use tools::ToolExecutor;

struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
    model: Arc<ScriptedModel>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("restore.sqlite3"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
            model: Arc::new(ScriptedModel::new(
                (0..8).map(|_| Ok(text_response("saved answer", FinishReason::Stop))),
            )),
        }
    }
    fn runtime(&self) -> AgentRuntime {
        self.runtime_with_bus().0
    }
    fn runtime_with_bus(&self) -> (AgentRuntime, Arc<EventBus>) {
        let bus = Arc::new(EventBus::new(128));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus.clone())),
            self.model.clone(),
        )
        .with_run_store(RunStore::open(&self.config, self.storage.handle()).unwrap());
        (runtime, bus)
    }
    fn authority(&self) -> RunConfig {
        RunConfig {
            topology: CoordinationTopology::DynamicTeam { max_workers: 2 },
            team_store: Some(runtime::team_context::TeamStore {
                config: self.config.clone(),
                writer: self.storage.handle(),
                id: "project:thread".into(),
            }),
            delegation_value: Some("independent review".into()),
            finding_store: Some(self.config.db_path.clone()),
            memory: Some(runtime::memory::MemoryBoundary::default()),
            ..Default::default()
        }
    }
    async fn root(&self, runtime: &AgentRuntime) -> RunId {
        let root = runtime.delegate_background(
            Role::Orchestrator,
            "original team task".into(),
            RunConfig {
                name: Some("chat:Orchestrator:team".into()),
                ..self.authority()
            },
        );
        assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Done);
        root
    }
}

#[tokio::test]
async fn restarted_team_root_reuses_history_and_completed_board_only_with_current_authority() {
    let fixture = Fixture::new();
    let board =
        runtime::team::TeamBoard::durable(fixture.storage.handle(), "project:thread".into());
    board
        .enqueue(runtime::team::TaskSpec {
            id: "verified".into(),
            paths: vec!["src".into()],
        })
        .unwrap();
    let lease = board.claim("verified", "previous-worker", 0).unwrap();
    board.complete("verified", &lease, 0).unwrap();
    let before = board.snapshot().unwrap();
    let first = fixture.runtime();
    let old = fixture.root(&first).await;
    let diagnostics = first.restore_diagnostics(old).unwrap().unwrap();
    assert!(!diagnostics.disk_restorable);
    assert!(diagnostics.history_available_with_current_authority);
    assert_eq!(
        diagnostics.renewable_team.unwrap().team_id,
        "project:thread"
    );
    drop(first);
    let restarted = fixture.runtime();
    let new = restarted
        .delegate_chat(
            "team",
            Role::Orchestrator,
            "continue with current grants".into(),
            fixture.authority(),
        )
        .unwrap();
    assert!(new.get() > old.get());
    assert_eq!(restarted.wait(new).await.unwrap(), AgentRunPhase::Done);
    let messages = serde_json::to_string(&fixture.model.observed().await[1]).unwrap();
    assert!(messages.contains("original team task"));
    assert!(messages.contains("continue with current grants"));
    let current = runtime::team_context::TeamContext::persistent(
        new,
        fixture.authority().team_store.as_ref().unwrap(),
        2,
    )
    .unwrap();
    assert_eq!(current.board.snapshot().unwrap(), before);
}

#[tokio::test]
async fn team_history_rejects_missing_or_mismatched_current_store() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    fixture.root(&runtime).await;
    let mut wrong = fixture.authority();
    wrong.team_store.as_mut().unwrap().id = "another-team".into();
    for authority in [RunConfig::default(), wrong] {
        let error = runtime
            .delegate_chat("team", Role::Orchestrator, "continue".into(), authority)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("current_team_authority_required")
        );
    }
    assert_eq!(fixture.model.observed().await.len(), 1);
}

#[tokio::test]
async fn team_history_rejects_persisted_claims_and_live_unclaimed_descendants() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let root = fixture.root(&runtime).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    fixture.model.gate_key("still running", gate).await;
    let child = runtime
        .delegate_background_as_child(root, Role::Explorer, "still running", RunConfig::default())
        .unwrap();
    let error = runtime
        .continue_goal(root, "continue".into(), fixture.authority())
        .unwrap_err();
    assert!(error.to_string().contains("descendant is still running"));
    runtime.cancel(child).unwrap();
    runtime.wait(child).await.unwrap();
    let board =
        runtime::team::TeamBoard::durable(fixture.storage.handle(), "project:thread".into());
    board
        .enqueue(runtime::team::TaskSpec {
            id: "uncertain".into(),
            paths: vec!["src".into()],
        })
        .unwrap();
    board.claim("uncertain", "interrupted-worker", 0).unwrap();
    let error = runtime
        .continue_goal(root, "continue".into(), fixture.authority())
        .unwrap_err();
    assert!(error.to_string().contains("persisted task claims"));
    // A refused restore never consumes the valid history checkpoint.
    assert!(
        runtime
            .restore_diagnostics(root)
            .unwrap()
            .unwrap()
            .history_available_with_current_authority
    );
}

#[tokio::test]
async fn disk_authoritative_delivery_still_rejects_a_renewable_team_root() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let root = fixture.root(&runtime).await;
    let child = runtime
        .delegate_background_as_child(root, Role::Explorer, "child", RunConfig::default())
        .unwrap();
    runtime.wait(child).await.unwrap();
    let result = runtime.send_agent_message(child, root, AgentMessageKind::Send, "resume", None);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("current_team_authority_required")
    );
}

#[tokio::test]
async fn history_restore_checks_both_record_and_descriptor_flags() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let root = runtime.delegate_background(
        Role::Worker,
        "old".into(),
        RunConfig {
            name: Some("chat:Worker:ordinary".into()),
            ..Default::default()
        },
    );
    runtime.wait(root).await.unwrap();
    let db = storage::Database::open(&fixture.config).unwrap();
    let mut record = db.run_context(&root.to_string()).unwrap().unwrap();
    record.restorable = false;
    fixture
        .storage
        .handle()
        .upsert_run_context(&record)
        .unwrap();
    assert!(
        runtime
            .delegate_chat(
                "ordinary",
                Role::Worker,
                "continue".into(),
                RunConfig::default()
            )
            .is_err()
    );
    assert!(
        !runtime
            .restore_diagnostics(root)
            .unwrap()
            .unwrap()
            .history_available_with_current_authority
    );
}

#[tokio::test]
async fn continue_root_after_restart_renews_authority_and_keeps_root_identity() {
    let fixture = Fixture::new();
    let first = fixture.runtime();
    let root = fixture.root(&first).await;
    drop(first);
    let (restarted, bus) = fixture.runtime_with_bus();
    let mut events = bus.subscribe();
    assert_eq!(
        restarted
            .continue_goal(root, "followup after restart".into(), fixture.authority())
            .unwrap(),
        root
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await.unwrap().kind,
                event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Waiting,
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        restarted.inspect_agent(root).unwrap().role_name,
        Role::Orchestrator.name()
    );
    let messages = serde_json::to_string(&fixture.model.observed().await[1]).unwrap();
    assert!(messages.contains("original team task"));
    assert!(messages.contains("followup after restart"));
    restarted.cancel(root).unwrap();
    restarted.wait(root).await.unwrap();
}

#[tokio::test]
async fn root_reference_memory_and_finding_path_are_renewed_not_restored() {
    let fixture = Fixture::new();
    let first = fixture.runtime();
    let root = first.delegate_background(
        Role::Worker,
        "root with reference memory".into(),
        RunConfig {
            memory: Some(runtime::memory::MemoryBoundary::default()),
            finding_store: Some(fixture.config.db_path.clone()),
            ..Default::default()
        },
    );
    first.wait(root).await.unwrap();
    let diagnostics = first.restore_diagnostics(root).unwrap().unwrap();
    assert!(!diagnostics.disk_restorable);
    assert!(diagnostics.history_available_with_current_authority);
    assert_eq!(
        diagnostics.refusal_reason.as_deref(),
        Some("current_root_context_authority_required")
    );
    drop(first);
    let (restarted, bus) = fixture.runtime_with_bus();
    let mut events = bus.subscribe();
    restarted
        .continue_goal(root, "fresh default authority".into(), RunConfig::default())
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await.unwrap().kind,
                event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Waiting,
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    restarted.cancel(root).unwrap();
    restarted.wait(root).await.unwrap();
    // With no current memory/path grants, the next checkpoint has no renewal requirement.
    assert!(
        restarted
            .restore_diagnostics(root)
            .unwrap()
            .unwrap()
            .disk_restorable
    );
}

#[tokio::test]
async fn team_workers_branch_and_learning_state_are_not_blanket_renewed() {
    for blocker in ["team_task", "workspace_branch", "learning_internal"] {
        let fixture = Fixture::new();
        let runtime = fixture.runtime();
        let mut config = fixture.authority();
        match blocker {
            "team_task" => {
                config.team_task = Some(runtime::team::TaskSpec {
                    id: "worker-task".into(),
                    paths: vec!["src".into()],
                })
            }
            "workspace_branch" => config.workspace_branch = Some("existing-branch".into()),
            "learning_internal" => config.learning_internal = true,
            _ => unreachable!(),
        }
        let root =
            runtime.delegate_background(Role::Orchestrator, "unsafe root state".into(), config);
        runtime.wait(root).await.unwrap();
        let diagnostics = runtime.restore_diagnostics(root).unwrap().unwrap();
        assert!(!diagnostics.disk_restorable);
        assert!(!diagnostics.history_available_with_current_authority);
        assert!(diagnostics.refusal_reason.unwrap().contains(blocker));
        assert!(
            runtime
                .continue_goal(root, "continue".into(), fixture.authority())
                .is_err()
        );
    }
}

#[tokio::test]
async fn stale_current_ownership_cannot_consume_a_valid_checkpoint() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let root = runtime.delegate_background(Role::Worker, "root".into(), RunConfig::default());
    runtime.wait(root).await.unwrap();
    let registry_path = fixture._dir.path().join("owners.sqlite3");
    let current_lease = runtime::ownership::Lease {
        owner_id: "new-owner".into(),
        generation: 2,
        expires_at: u64::MAX,
    };
    let owner = runtime::ownership::ThreadOwner::new("thread".into(), current_lease);
    runtime::ownership::Registry::open(&registry_path)
        .unwrap()
        .start(&owner)
        .unwrap();
    let stale = runtime::ownership::OwnerPermit {
        registry_path,
        thread_id: "thread".into(),
        run_id: None,
        lease: runtime::ownership::Lease {
            owner_id: "old-owner".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    };
    assert!(matches!(
        runtime.continue_goal(
            root,
            "continue".into(),
            RunConfig {
                ownership: Some(stale),
                ..Default::default()
            }
        ),
        Err(runtime::RuntimeError::StaleOwnership { .. })
    ));
    assert!(
        runtime
            .restore_diagnostics(root)
            .unwrap()
            .unwrap()
            .disk_restorable
    );
}

struct PendingChildAdmission {
    inner: Arc<ScriptedModel>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl runtime::AgentModel for PendingChildAdmission {
    fn requires_admission(&self) -> bool {
        true
    }

    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        role: Role,
    ) -> Result<(), runtime::RuntimeError> {
        if role == Role::Explorer {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }

    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.inner.complete(invocation, role, messages, tools).await
    }

    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        self.inner.selected_model(role, category)
    }
}

#[tokio::test]
async fn team_history_rejects_child_before_and_after_admission_registration() {
    let fixture = Fixture::new();
    let bus = Arc::new(EventBus::new(128));
    let model = Arc::new(PendingChildAdmission {
        inner: fixture.model.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(RunStore::open(&fixture.config, fixture.storage.handle()).unwrap());
    let root = fixture.root(&runtime).await;
    fixture
        .model
        .gate_key("pending-child", Arc::new(tokio::sync::Notify::new()))
        .await;
    let child = runtime
        .delegate_background_as_child(root, Role::Explorer, "pending-child", RunConfig::default())
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), model.entered.notified())
        .await
        .unwrap();
    assert!(
        runtime.inspect_agent(child).is_err(),
        "admission has not registered the child"
    );
    for result in [
        runtime.continue_goal(root, "continue".into(), fixture.authority()),
        runtime.delegate_chat(
            "team",
            Role::Orchestrator,
            "continue".into(),
            fixture.authority(),
        ),
    ] {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("awaiting admission")
        );
    }
    assert_eq!(fixture.model.observed().await.len(), 1);
    model.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while runtime.inspect_agent(child).is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let error = runtime
        .continue_goal(root, "continue".into(), fixture.authority())
        .unwrap_err();
    assert!(error.to_string().contains("descendant is still running"));
    runtime.cancel(child).unwrap();
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Error);
    // Refusals did not consume the checkpoint; once the old child stops,
    // a fresh root run can reuse it with current authority.
    let resumed = runtime
        .continue_goal(
            root,
            "continue after child stops".into(),
            fixture.authority(),
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while runtime.inspect_agent(resumed).unwrap().phase != AgentRunPhase::Waiting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(resumed, root);
    runtime.cancel(resumed).unwrap();
    runtime.wait(resumed).await.unwrap();
}
