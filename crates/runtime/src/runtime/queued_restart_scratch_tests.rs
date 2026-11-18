use super::*;
use crate::scratch::{ScratchKey, ThreadScratch};

struct AdmissionModel;

#[async_trait::async_trait]
impl crate::AgentModel for AdmissionModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "unused".into()
    }
    fn requires_admission(&self) -> bool {
        true
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        unreachable!("a discarded queued launch must never reach the model")
    }
}

#[derive(Clone, Copy, Debug)]
enum Disposal {
    Cancelled,
    CompletedAdmission,
    GrantFailure,
    Replaced,
    DispatchRefused,
    StillQueued,
}

#[tokio::test]
async fn only_proven_unstarted_queued_disposals_release_their_own_incarnation() {
    for disposal in [
        Disposal::Cancelled,
        Disposal::CompletedAdmission,
        Disposal::GrantFailure,
        Disposal::Replaced,
        Disposal::DispatchRefused,
        Disposal::StillQueued,
    ] {
        let bus = Arc::new(event_bus::EventBus::new(16));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(AdmissionModel),
        );
        let temp = tempfile::tempdir().unwrap();
        let scratch = Arc::new(ThreadScratch::at(temp.path().join("scratch")));
        runtime.set_thread_scratch(scratch.clone());
        let key = ScratchKey::Thread("thread".into());
        let dir = scratch.prepare(&key).unwrap();
        std::fs::write(dir.join("keep"), "keep").unwrap();
        scratch.record_scratch_lease(
            "thread",
            &crate::ownership::Lease {
                owner_id: "owner".into(),
                generation: 1,
                expires_at: u64::MAX,
            },
        );
        let run = RunId::new(41);
        let execution = runtime.reserve_admission(run, None, None);
        let incarnation = execution.publication().incarnation();
        scratch.register_execution(&key, run, &incarnation).unwrap();
        runtime
            .shared
            .admissions
            .lock()
            .unwrap()
            .get_mut(&run)
            .unwrap()
            .queued = Some(QueuedLaunch {
            parent: None,
            role: Role::Worker,
            prompt: "unused".into(),
            config: RunConfig::default(),
            continuation: RunContinuation::Fresh,
            review_seed: None,
        });
        // A newer execution of the same run must remain independently protected.
        let newer = ExecutionControl::new(false);
        let newer_incarnation = newer.publication().incarnation();
        scratch
            .register_execution(&key, run, &newer_incarnation)
            .unwrap();
        match disposal {
            Disposal::Cancelled => {
                runtime
                    .shared
                    .admissions
                    .lock()
                    .unwrap()
                    .get_mut(&run)
                    .unwrap()
                    .cancelled = true;
                runtime.discard_cancelled_restart_launches();
            }
            Disposal::CompletedAdmission => {
                runtime.fail_reserved_admission(run, &execution, "admission failed".into());
                runtime.discard_cancelled_restart_launches();
            }
            Disposal::GrantFailure => {
                let result: Result<(), _> = runtime.with_restart_launch_fence(|| {
                    Err(RuntimeError::Model {
                        reason: "grant failed".into(),
                    })
                });
                assert!(result.is_err());
            }
            Disposal::Replaced => {
                runtime
                    .shared
                    .spawn_intents
                    .lock()
                    .unwrap()
                    .get_mut(&run)
                    .unwrap()
                    .execution = Some(newer);
                assert!(runtime.with_restart_launch_fence(|| Ok(())).is_ok());
            }
            Disposal::DispatchRefused => {
                let result = runtime.with_restart_launch_fence(|| {
                    assert!(runtime.shared.dispatch_tails.try_close());
                    Ok(())
                });
                assert!(result.is_err());
            }
            Disposal::StillQueued => runtime.discard_cancelled_restart_launches(),
        }
        assert!(scratch.archive_snapshot("thread").is_none(), "{disposal:?}");
        scratch.release_execution(run, &newer_incarnation);
        assert_eq!(
            scratch.archive_snapshot("thread").is_some(),
            !matches!(disposal, Disposal::StillQueued),
            "{disposal:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("keep")).unwrap(),
            "keep",
            "{disposal:?}"
        );
        assert!(runtime.list_agents().is_empty(), "{disposal:?}");
    }
}
