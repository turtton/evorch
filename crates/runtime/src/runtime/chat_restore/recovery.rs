//! Explicit human Continue is the only exit from retired recovery evidence.
use super::*;
use crate::quiesce::{RecoveryClaimState, ResumeIntent};

pub(super) struct RecoveryContinue {
    pub(super) execution: Arc<ExecutionControl>,
    pub(super) intent: ResumeIntent,
    pub(super) fence: storage::resume_intent::RecoveryContinueFence,
}

impl AgentRuntime {
    pub(super) fn prepare_recovery_continue(
        &self,
        store: &crate::RunStore,
        run: RunId,
        explicit: bool,
        input: Option<crate::quiesce::DurableInput>,
    ) -> Result<Option<RecoveryContinue>, RuntimeError> {
        let fail = |reason: &str| crate::quiesce::runtime::error(reason);
        let evidence = store
            .with_database(|db| {
                let Some(row) = db.resume_intent(&run.to_string())? else {
                    return Ok(None);
                };
                let revision = db
                    .resume_intent_revision(&run.to_string())?
                    .ok_or_else(|| storage::StorageError::Io("missing intent revision".into()))?;
                let claim = db.recovery_claims()?.into_iter().find(|claim| {
                    claim.members.iter().any(|m| {
                        m.run_id == run.to_string()
                            && match claim.state {
                                RecoveryClaimState::Prepared
                                | RecoveryClaimState::Released { .. } => {
                                    m.old_incarnation == row.incarnation
                                }
                                RecoveryClaimState::Activated => {
                                    m.new_incarnation == row.incarnation
                                }
                            }
                    })
                });
                Ok(claim.map(|claim| (row, revision, claim)))
            })
            .map_err(crate::quiesce::runtime::error)?;
        let Some((row, revision, claim)) = evidence else {
            return Ok(None);
        };
        if !explicit || matches!(claim.state, RecoveryClaimState::Prepared) {
            return Err(fail(
                "recovery claim requires explicit resolution before Continue",
            ));
        }
        if !self.shared.quiesce.running() {
            return Err(fail("manual recovery Continue requires a running runtime"));
        }
        if self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .is_some_and(|i| i.paused_recovery.is_some())
        {
            return Err(fail(
                "release the paused recovery component before Continue",
            ));
        }
        let mut intent =
            ResumeIntent::decode(row.clone()).map_err(crate::quiesce::runtime::error)?;
        let execution = ExecutionControl::new(false);
        intent.incarnation = execution.publication().incarnation();
        intent.active = true;
        intent.resume_requested = false;
        intent.stop_reason = None;
        intent.initial_input = input;
        // A human is not asserting the lost tool's result. The history-only
        // conversion reports uncertainty/aborted waits without replaying effects.
        intent.tool_intent.clear();
        let fence = storage::resume_intent::RecoveryContinueFence {
            record: storage::resume_intent::ResumeIntentRecord {
                run_id: run.to_string(),
                incarnation: intent.incarnation.clone(),
                payload: serde_json::to_string(&intent).map_err(crate::quiesce::runtime::error)?,
            },
            expected_incarnation: row.incarnation,
            expected_revision: revision,
        };
        Ok(Some(RecoveryContinue {
            execution,
            intent,
            fence,
        }))
    }
}
