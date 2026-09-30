//! Current callers may reuse stopped root history; disk delivery cannot renew authority.

use super::*;

impl AgentRuntime {
    pub(super) fn validate_history_restore(
        &self,
        record: &storage::RunContextRecord,
        descriptor: &RunRestoreDescriptor,
        authority: &RunConfig,
    ) -> Result<(), RuntimeError> {
        let fail = |reason: String| RuntimeError::RunRestoreFailed {
            run_id: record.run_id.clone(),
            reason: RunRestoreFailure::UnsupportedConfig(reason),
        };
        if let Some(permit) = &authority.ownership {
            permit
                .validate_generation()
                .map_err(|_| RuntimeError::StaleOwnership {
                    run_id: record.run_id.clone(),
                })?;
        }
        // Current callers reuse history, not the old execution or tool jobs.
        // Retain the authority/identity gates while reporting lost outcomes to
        // the next model turn instead of refusing the entire conversation.
        let history_descriptor = descriptor.conversation_descriptor();
        let descriptor = &history_descriptor;
        if (record.restorable && descriptor.restorable)
            || descriptor.renewable_ownership_only()
            || descriptor.renewable_root_context()
        {
            self.validate_stopped_history_tree(record, "root")?;
            return Ok(());
        }
        if !descriptor.renewable_team_root() {
            return Err(fail(
                descriptor
                    .non_restorable_reason
                    .clone()
                    .unwrap_or_else(|| "実行設定".into()),
            ));
        }
        let team = descriptor
            .renewable_team
            .as_ref()
            .expect("validated team identity");
        if team.coordinator_run_id.to_string() != record.run_id
            || record.parent_run_id.is_some()
            || record.role != agents::Role::Orchestrator.name()
        {
            return Err(fail("team coordinator identity mismatch".into()));
        }
        let store = authority.team_store.as_ref().ok_or_else(|| {
            fail("current_team_authority_required: provide the current durable team store".into())
        })?;
        if store.id != team.team_id
            || authority.topology.worker_limit().is_none()
            || authority
                .delegation_value
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            || authority.team_task.is_some()
        {
            return Err(fail("current_team_authority_required: team identity, topology and delegation value must be explicitly renewed".into()));
        }
        self.validate_stopped_history_tree(record, "team")?;
        // Read only from caller-granted storage. Expired claims are also blocked:
        // expiry alone cannot prove a prior process has stopped applying effects.
        let current = crate::team_context::TeamContext::persistent(
            team.coordinator_run_id,
            store,
            authority
                .topology
                .worker_limit()
                .expect("validated team topology"),
        )
        .map_err(|error| fail(format!("team storage validation failed: {error}")))?;
        if current
            .board
            .snapshot()
            .map_err(|error| fail(error.to_string()))?
            .iter()
            .any(|task| matches!(task.state, crate::team::ClaimState::Claimed(_)))
        {
            return Err(fail("team_reconciliation_required: persisted task claims must be reconciled before continuing".into()));
        }
        Ok(())
    }

    fn validate_stopped_history_tree(
        &self,
        record: &storage::RunContextRecord,
        scope: &str,
    ) -> Result<(), RuntimeError> {
        let root = crate::meta::parse_run_id(&record.run_id).map_err(|reason| {
            RuntimeError::RunRestoreFailed {
                run_id: record.run_id.clone(),
                reason: RunRestoreFailure::CorruptContext(reason),
            }
        })?;
        // Fence intents, pending admission and registered runs together, in the
        // same lock order as spawning: intents, admissions, then runs.
        let stopped = record.terminal_phase == "Stopped";
        {
            let intents = self
                .shared
                .spawn_intents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let runs = lock_runs(&self.shared.runs);
            let active = runs.iter().filter_map(|(id, entry)| {
                matches!(
                    *entry.phase_rx.borrow(),
                    AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
                )
                .then_some(*id)
            });
            let pending = admissions
                .iter()
                .filter_map(|(id, admission)| admission.snapshot().result.is_none().then_some(*id));
            for id in active.chain(pending) {
                let in_subtree = belongs_to_subtree(id, root, |candidate| {
                    runs.get(&candidate)
                        .and_then(|run| run.parent)
                        .or_else(|| {
                            admissions
                                .get(&candidate)
                                .and_then(|admission| admission.snapshot().parent)
                        })
                        .or_else(|| {
                            // Preserve the legacy ancestry rules for other snapshots.
                            if stopped {
                                intents.get(&candidate).and_then(|intent| intent.parent)
                            } else {
                                None
                            }
                        })
                });
                // Only an explicit operator stop may retain live descendants.
                // It never authorizes unrelated execution; ordinary terminal
                // snapshots retain their strict descendant reconciliation gate.
                if (stopped && !in_subtree) || (!stopped && in_subtree) {
                    return Err(RuntimeError::RunRestoreFailed {
                        run_id: record.run_id.clone(),
                        reason: RunRestoreFailure::UnsupportedConfig(format!(
                            "{scope}_reconciliation_required: the previous root or a descendant is still running or awaiting admission"
                        )),
                    });
                }
            }
        }
        // Terminal cleanup can fail while retaining process handles. Reusing
        // history must not race a shell still modifying the old workspace.
        if self
            .shared
            .executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .has_running_shell_jobs(&record.run_id)
        {
            return Err(RuntimeError::RunRestoreFailed {
                run_id: record.run_id.clone(),
                reason: RunRestoreFailure::UnsupportedConfig(
                    "shell_cleanup_required: previous shell processes are still running".into(),
                ),
            });
        }
        Ok(())
    }
}

/// Missing ancestry and cycles cannot establish membership in the resumed tree.
fn belongs_to_subtree(
    live: RunId,
    root: RunId,
    mut parent: impl FnMut(RunId) -> Option<RunId>,
) -> bool {
    let mut cursor = Some(live);
    let mut visited = std::collections::HashSet::new();
    while let Some(candidate) = cursor {
        if candidate == root {
            return true;
        }
        if !visited.insert(candidate) {
            return false;
        }
        cursor = parent(candidate);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Model {
        admission_pending: AtomicBool,
    }

    #[async_trait::async_trait]
    impl AgentModel for Model {
        fn requires_admission(&self) -> bool {
            self.admission_pending.load(Ordering::Relaxed)
        }

        async fn admit(
            &self,
            _: &crate::AgentInvocationContext,
            _: Role,
        ) -> Result<(), RuntimeError> {
            std::future::pending().await
        }

        async fn complete(
            &self,
            _: &crate::AgentInvocationContext,
            _: Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, RuntimeError> {
            Ok(providers::ChatResponse {
                message: providers::Message {
                    role: providers::Role::Assistant,
                    content: vec![providers::ContentBlock::Text {
                        text: "saved history".into(),
                    }],
                },
                finish_reason: providers::FinishReason::Stop,
                usage: providers::Usage::default(),
            })
        }

        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "test".into()
        }
    }

    struct Fixture {
        runtime: AgentRuntime,
        model: Arc<Model>,
        root: RunId,
        record: storage::RunContextRecord,
        _storage: storage::Storage,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        async fn new(phase: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let config = storage::StorageConfig {
                db_path: dir.path().join("renewal.sqlite3"),
                ..Default::default()
            };
            let storage = storage::Storage::open(config.clone()).unwrap();
            let bus = Arc::new(EventBus::new(128));
            let model = Arc::new(Model::default());
            let runtime =
                AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
                    .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
            let root = runtime.delegate_background(
                Role::Orchestrator,
                "root".into(),
                RunConfig::default(),
            );
            assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Done);
            let mut record = runtime
                .shared
                .run_store
                .get()
                .unwrap()
                .restore_record(root)
                .unwrap()
                .unwrap();
            record.terminal_phase = phase.into();
            storage.handle().upsert_run_context(&record).unwrap();
            Self {
                runtime,
                model,
                root,
                record,
                _storage: storage,
                _dir: dir,
            }
        }

        fn spawn(&self, parent: Option<RunId>) -> RunId {
            self.runtime.spawn_reserved(
                self.runtime.reserve_run_id(),
                parent,
                Role::Worker,
                "child",
                RunConfig::default(),
            )
        }

        fn validate(&self) -> Result<(), RuntimeError> {
            self.runtime
                .validate_stopped_history_tree(&self.record, "root")
        }

        fn assert_reconciliation(&self) {
            assert!(
                matches!(self.validate(), Err(RuntimeError::RunRestoreFailed {
                reason: RunRestoreFailure::UnsupportedConfig(reason), ..
            }) if reason == "root_reconciliation_required: the previous root or a descendant is still running or awaiting admission")
            );
        }
    }

    #[tokio::test]
    async fn stopped_record_allows_live_direct_child() {
        let fixture = Fixture::new("Stopped").await;
        fixture.spawn(Some(fixture.root));
        fixture.validate().unwrap();
    }

    #[tokio::test]
    async fn stopped_record_allows_live_grandchild_via_intents() {
        let fixture = Fixture::new("Stopped").await;
        let intermediate = fixture.runtime.reserve_run_id();
        fixture
            .runtime
            .track_goal_run(intermediate, &fixture.root.to_string());
        fixture.spawn(Some(intermediate));
        assert!(!lock_runs(&fixture.runtime.shared.runs).contains_key(&intermediate));
        fixture.validate().unwrap();
    }

    #[tokio::test]
    async fn stopped_record_rejects_live_foreign_run() {
        let fixture = Fixture::new("Stopped").await;
        fixture.spawn(Some(fixture.root));
        let foreign = fixture.runtime.reserve_run_id();
        fixture.spawn(Some(foreign));
        fixture.assert_reconciliation();
    }

    #[tokio::test]
    async fn non_stopped_record_still_rejects_live_child() {
        for phase in ["Done", "Error", "Checkpoint"] {
            let fixture = Fixture::new(phase).await;
            fixture.spawn(Some(fixture.root));
            fixture.assert_reconciliation();
        }
    }

    #[tokio::test]
    async fn stopped_record_allows_admission_pending_child() {
        let fixture = Fixture::new("Stopped").await;
        fixture
            .model
            .admission_pending
            .store(true, Ordering::Relaxed);
        let child = fixture.spawn(Some(fixture.root));
        assert!(!lock_runs(&fixture.runtime.shared.runs).contains_key(&child));
        assert!(
            fixture
                .runtime
                .admission_snapshot(child)
                .unwrap()
                .result
                .is_none()
        );
        fixture.validate().unwrap();
    }

    #[tokio::test]
    async fn non_stopped_record_keeps_foreign_run_behavior() {
        let fixture = Fixture::new("Done").await;
        fixture.spawn(None);
        fixture.validate().unwrap();
    }

    #[tokio::test]
    async fn stopped_goal_restores_in_place_with_live_child_and_current_authority() {
        let fixture = Fixture::new("Stopped").await;
        fixture
            .runtime
            .entry(fixture.root)
            .unwrap()
            .phase_tx
            .send_replace(AgentRunPhase::Stopped);
        let child = fixture.spawn(Some(fixture.root));
        assert_eq!(
            fixture
                .runtime
                .continue_goal(
                    fixture.root,
                    "continue".into(),
                    RunConfig {
                        delegation_value: Some("current authority".into()),
                        ..Default::default()
                    },
                )
                .unwrap(),
            fixture.root
        );
        let entry = fixture.runtime.entry(fixture.root).unwrap();
        assert_eq!(*entry.phase_rx.borrow(), AgentRunPhase::Pending);
        assert_eq!(
            entry.config.delegation_value.as_deref(),
            Some("current authority")
        );
        drop(entry);
        assert_eq!(
            fixture.runtime.entry(child).unwrap().parent,
            Some(fixture.root)
        );
        let record = fixture
            .runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(fixture.root)
            .unwrap()
            .unwrap();
        let descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
        assert!(!record.restorable && !descriptor.restorable);
        assert_eq!(
            descriptor.non_restorable_reason.as_deref(),
            Some("snapshot_consumed")
        );
    }

    #[tokio::test]
    async fn operator_self_stop_then_continue_preserves_waiting_child() {
        let fixture = Fixture::new("Done").await;
        let root = fixture.runtime.delegate_background(
            Role::Orchestrator,
            "root".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        );
        let child = fixture.runtime.spawn_reserved(
            fixture.runtime.reserve_run_id(),
            Some(root),
            Role::Worker,
            "child",
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        );
        for run in [root, child] {
            let mut phase = fixture.runtime.entry(run).unwrap().phase_rx.clone();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while *phase.borrow_and_update() != AgentRunPhase::Waiting {
                    phase.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
        }
        fixture
            .runtime
            .stop(root, crate::StopScope::SelfOnly)
            .unwrap();
        assert_eq!(
            fixture.runtime.wait(root).await.unwrap(),
            AgentRunPhase::Stopped
        );
        let store = fixture.runtime.shared.run_store.get().unwrap();
        assert_eq!(
            store.restore_record(root).unwrap().unwrap().terminal_phase,
            "Stopped"
        );
        assert_eq!(
            *fixture.runtime.entry(child).unwrap().phase_rx.borrow(),
            AgentRunPhase::Waiting
        );
        assert_eq!(
            fixture
                .runtime
                .continue_goal(root, "continue".into(), RunConfig::default())
                .unwrap(),
            root
        );
        assert_eq!(
            *fixture.runtime.entry(root).unwrap().phase_rx.borrow(),
            AgentRunPhase::Pending
        );
        assert_eq!(
            *fixture.runtime.entry(child).unwrap().phase_rx.borrow(),
            AgentRunPhase::Waiting
        );
        assert!(!store.restore_record(root).unwrap().unwrap().restorable);
    }

    #[tokio::test]
    async fn rejected_stopped_restore_does_not_consume_snapshot() {
        let fixture = Fixture::new("Stopped").await;
        fixture.spawn(None);
        assert!(
            fixture
                .runtime
                .continue_goal(fixture.root, "continue".into(), RunConfig::default())
                .is_err()
        );
        assert_eq!(
            fixture
                .runtime
                .shared
                .run_store
                .get()
                .unwrap()
                .restore_record(fixture.root)
                .unwrap(),
            Some(fixture.record)
        );
    }

    #[test]
    fn subtree_membership_rejects_missing_ancestry_and_cycles() {
        let root = RunId::new(1);
        let child = RunId::new(2);
        assert!(belongs_to_subtree(root, root, |_| None));
        assert!(!belongs_to_subtree(child, root, |_| None));
        assert!(!belongs_to_subtree(child, root, |_| Some(child)));
    }
}
