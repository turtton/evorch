use super::*;
use crate::{AgentRuntime, Role, RunConfig, RuntimeError};

pub(crate) fn error(reason: impl ToString) -> RuntimeError {
    RuntimeError::Model {
        reason: format!("durable intent: {}", reason.to_string()),
    }
}
impl AgentRuntime {
    pub fn quiesce_state(&self) -> QuiesceState {
        self.shared.quiesce.state()
    }
    /// Stop opening turns; never abort a provider/tool future. Dropping the waiter
    /// does not undo the request. Concurrent callers join the same transaction.
    pub fn request_quiesce(&self) {
        let _ = self.shared.quiesce.begin();
    }
    /// Only success permits the host to exit; no elapsed-time escape hatch.
    pub async fn wait_quiesced(&self) -> Result<(), RuntimeError> {
        let mut version = self.shared.quiesce.subscribe();
        loop {
            let complete = self.complete_restart_safe_points()?;
            if complete {
                return Ok(());
            }
            version.changed().await.map_err(error)?;
        }
    }
    pub async fn quiesce(&self) -> Result<(), RuntimeError> {
        self.begin_restart_quiesce()?;
        self.wait_quiesced().await
    }
    /// Read-only evidence. PR 06 must validate current authority and the
    /// incarnation/receipt before admission; these rows are not permits.
    pub fn resume_intents(&self) -> Result<Vec<ResumeIntent>, RuntimeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let rows = store
            .with_database(|db| db.resume_intents())
            .map_err(error)?;
        rows.into_iter()
            .map(|row| ResumeIntent::decode(row).map_err(error))
            .collect()
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn admit_intent(
        &self,
        run: RunId,
        incarnation: String,
        parent: Option<RunId>,
        role: Role,
        prompt: &str,
        config: &RunConfig,
        history_only: bool,
        defer_registration: bool,
    ) -> Result<bool, RuntimeError> {
        self.admit_intent_with_snapshot(
            run,
            incarnation,
            parent,
            role,
            prompt,
            config,
            history_only,
            defer_registration,
            None,
        )
        .map(|(start, _)| start)
        .map_err(error)
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn admit_intent_with_snapshot(
        &self,
        run: RunId,
        incarnation: String,
        parent: Option<RunId>,
        role: Role,
        prompt: &str,
        config: &RunConfig,
        history_only: bool,
        defer_registration: bool,
        snapshot: Option<&storage::thread_index::SnapshotWrite>,
    ) -> Result<(bool, Option<storage::thread_index::SnapshotReceipt>), AdmissionError> {
        self.shared.quiesce.admit(
            ResumeIntent {
                version: 1,
                run_id: run,
                incarnation,
                parent_run_id: parent,
                role: role.name().into(),
                wait: ResumeWait::Admission,
                initial_input: (!history_only).then(|| DurableInput {
                    id: config
                        .initial_prompt_id
                        .clone()
                        .unwrap_or_else(|| "initial".into()),
                    text: prompt.into(),
                    images: config.images.clone(),
                    trusted_user: config.initial_prompt_id.is_some(),
                }),
                inputs: Vec::new(),
                active: true,
                resume_requested: false,
                stop_reason: None,
                checkpoint: None,
                snapshot: None,
                tool_intent: Vec::new(),
                provider_input: None,
                boundary_pending: false,
                responses_replay: Default::default(),
                compactions: Vec::new(),
                route_pins: Vec::new(),
            },
            self.shared.run_store.get(),
            defer_registration,
            snapshot,
        )
    }
    pub(crate) fn queue_intent_input(
        &self,
        run: RunId,
        input: DurableInput,
    ) -> Result<(), RuntimeError> {
        let execution = self.execution_snapshot(run)?;
        self.shared
            .quiesce
            .update(
                run,
                &execution.publication().incarnation(),
                self.shared.run_store.get(),
                |intent| {
                    if !intent.inputs.iter().any(|old| old.id == input.id) {
                        intent.inputs.push(input);
                    }
                },
            )
            .map_err(error)
    }
    pub(crate) fn note_intent_stop(
        &self,
        run: RunId,
        incarnation: &str,
        reason: StopReason,
    ) -> bool {
        let queued = self.shared.quiesce.queued(run, incarnation);
        if let Err(error) =
            self.shared
                .quiesce
                .update(run, incarnation, self.shared.run_store.get(), |intent| {
                    if intent.stop_reason != Some(StopReason::CliDisconnected)
                        || reason == StopReason::Cancelled
                    {
                        intent.stop_reason = Some(reason);
                    }
                    intent.resume_requested = false;
                    if queued {
                        intent.active = false;
                    }
                })
        {
            self.shared.quiesce.fail(error);
        }
        queued
    }
    /// The caller holds A/R and has fenced any unregistered execution out of
    /// registration. A registered execution must keep the strict update CAS.
    pub(crate) fn note_fenced_intent_stop(
        &self,
        run: RunId,
        incarnation: &str,
        reason: StopReason,
        registered: bool,
    ) -> bool {
        if registered {
            return self.note_intent_stop(run, incarnation, reason);
        }
        let queued = self.shared.quiesce.queued(run, incarnation);
        if let Err(error) = self.shared.quiesce.stop_unregistered(
            run,
            incarnation,
            self.shared.run_store.get(),
            reason,
        ) {
            self.shared.quiesce.fail(error);
        }
        queued
    }
    pub(crate) fn end_intent(&self, run: RunId, incarnation: &str, reason: StopReason) {
        if let Err(error) =
            self.shared
                .quiesce
                .retire(run, incarnation, self.shared.run_store.get(), reason)
        {
            self.shared.quiesce.fail(error);
        }
    }
    /// Record provenance before the existing incarnation-fenced subtree stop.
    pub fn record_cli_disconnected(&self, run: RunId) -> Result<(), RuntimeError> {
        let execution = self.execution_snapshot(run)?;
        self.shared
            .quiesce
            .update(
                run,
                &execution.publication().incarnation(),
                self.shared.run_store.get(),
                |intent| {
                    intent.stop_reason = Some(StopReason::CliDisconnected);
                },
            )
            .map_err(error)
    }
}
