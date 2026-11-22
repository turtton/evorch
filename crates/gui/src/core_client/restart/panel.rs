use super::text;
use std::time::Instant;
use workbench_client::{
    ClientError,
    restart::{
        confirmation::confirm,
        intent::{
            BlockerSelector, IntentError, TargetSelector, cancel_restart, cancellable,
            interrupt_shell, plan_restart, request_restart, select_blocker, stale_binding,
        },
        reconnect::{EvidenceObserver, PlannedEvidence, Stage},
        view::ReconnectView,
    },
};
use workbench_protocol::{
    commands::{Command, CommandReply, CommandResult},
    namespaces::lifecycle::{
        AdminId, AdminReceipt, LifecycleQuery, LifecycleReply, LifecycleResult,
        owned_graphs::{OwnedGraphsQuery, OwnedGraphsReply},
        restart::{
            InterruptBinding, RestartBinding, RestartManagementStatus, RestartQuery, RestartReply,
            RestartTargets, ShellBlocker, ShellBlockers,
        },
    },
    queries::{Query, QueryReply},
};

/// What the panel asks the connection actor to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelRequest {
    Query(Query),
    /// Sent once by the actor; never again.
    Command(Command),
}

/// What the person is being asked to confirm. The binding is exactly what was
/// on screen; it is dropped, not refreshed, when that changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirming {
    Restart(RestartBinding),
    Interrupt {
        run: String,
        binding: InterruptBinding,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Banner {
    None,
    Reconnecting(ReconnectView),
    Expired(Stage),
}

#[derive(Default)]
pub struct RestartPanel {
    targets: Option<RestartTargets>,
    status: Option<RestartManagementStatus>,
    blockers: Option<ShellBlockers>,
    selected: TargetSelector,
    confirming: Option<Confirming>,
    notices: Vec<String>,
    observer: EvidenceObserver,
    /// Operations sent whose end the panel has not seen.
    uncertain: Vec<AdminId>,
    /// This panel's own restart request, from the click until its operation is
    /// seen to have completed. It is the core's acceptance of a restart, so a
    /// lost connection while it stands may be the restart itself.
    own_restart: Option<AdminId>,
    /// Undisposed graphs the core counted when it last refused a restart.
    undisposed: Option<u64>,
    undisposed_due: bool,
}

fn restart_query(query: RestartQuery) -> PanelRequest {
    PanelRequest::Query(Query::Lifecycle(LifecycleQuery::Restart(query)))
}

impl RestartPanel {
    pub fn confirming(&self) -> Option<&Confirming> {
        self.confirming.as_ref()
    }

    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Keep the newest `keep` notices.
    pub fn trim_notices(&mut self, keep: usize) {
        let extra = self.notices.len().saturating_sub(keep);
        self.notices.drain(..extra);
    }

    pub fn clear_notices(&mut self) {
        self.notices.clear();
    }

    pub fn status(&self) -> Option<&RestartManagementStatus> {
        self.status.as_ref()
    }

    pub fn targets(&self) -> Option<&RestartTargets> {
        self.targets.as_ref()
    }

    pub fn blockers(&self) -> Option<&ShellBlockers> {
        self.blockers.as_ref()
    }

    pub fn undisposed(&self) -> Option<u64> {
        self.undisposed
    }

    /// Whether a restart this panel asked for has not been seen to end.
    pub fn restart_in_flight(&self) -> bool {
        self.own_restart.is_some()
    }

    /// Whether there is anything to read back after a reconnect.
    pub fn has_uncertain(&self) -> bool {
        !self.uncertain.is_empty()
    }

    pub fn select(&mut self, selector: TargetSelector) {
        self.selected = selector;
    }

    /// The reads that bring the screen up to date. Never a command.
    pub fn refresh_queries(&self) -> Vec<PanelRequest> {
        vec![
            restart_query(RestartQuery::Targets),
            restart_query(RestartQuery::Status),
            restart_query(RestartQuery::ShellBlockers),
        ]
    }

    /// After a lost reply or a restart: ask for every operation by its id.
    /// Never a command, so nothing is sent a second time.
    pub fn reconcile_queries(&self) -> Vec<PanelRequest> {
        self.uncertain
            .iter()
            .map(|id| {
                PanelRequest::Query(Query::Lifecycle(LifecycleQuery::OperationStatus {
                    operation_id: id.clone(),
                }))
            })
            .collect()
    }

    pub fn on_reply(&mut self, reply: &QueryReply, now: Instant) {
        let QueryReply::Lifecycle(reply) = reply else {
            return;
        };
        match reply {
            LifecycleReply::Restart(RestartReply::Targets(targets)) => {
                self.targets = Some(targets.clone());
                self.drop_stale_confirmation();
            }
            LifecycleReply::Restart(RestartReply::Status(status)) => {
                self.observer.observe_status(status, now);
                self.status = Some(status.clone());
                self.drop_stale_confirmation();
            }
            LifecycleReply::Restart(RestartReply::ShellBlockers(blockers)) => {
                self.blockers = Some(blockers.clone());
                self.drop_stale_confirmation();
            }
            LifecycleReply::OperationStatus(receipt) => self.on_receipt(receipt),
            LifecycleReply::OwnedGraphs(OwnedGraphsReply::Undisposed(graphs)) => {
                self.undisposed = Some(graphs.count.get());
                if graphs.count.get() > 0 {
                    self.notices.push(text::undisposed(graphs.count.get()));
                }
            }
            _ => {}
        }
    }

    /// The reply of a command: the core's receipt.
    pub fn on_command_reply(&mut self, reply: &CommandReply) {
        if let Some(CommandResult::Lifecycle(LifecycleResult::Accepted(receipt))) =
            reply.result.as_deref()
        {
            self.on_receipt(receipt);
        }
    }

    fn on_receipt(&mut self, receipt: &AdminReceipt) {
        self.notices.extend(text::receipt(receipt));
        if receipt.state
            == workbench_protocol::namespaces::lifecycle::AdminOperationState::Completed
        {
            self.uncertain.retain(|id| id != &receipt.operation_id);
            if self.own_restart.as_ref() == Some(&receipt.operation_id) {
                self.own_restart = None;
            }
        }
    }

    /// An old dialog must not run against a changed target or phase.
    fn drop_stale_confirmation(&mut self) {
        let stale = match (&self.confirming, &self.targets, &self.status) {
            (Some(Confirming::Restart(binding)), Some(targets), _) => {
                targets.core_epoch != binding.expected_core_epoch
                    || targets
                        .targets
                        .iter()
                        .find(|t| t.id == binding.target_id)
                        .is_none_or(|t| t.revision != binding.target_revision)
            }
            (Some(Confirming::Interrupt { binding, .. }), _, Some(status)) => {
                stale_binding(binding, status).is_some()
            }
            _ => false,
        };
        if stale {
            self.confirming = None;
            self.notices.push(
                "確認中の対象が変わったため、確認を取り消しました。内容を確認してからもう一度操作してください".into(),
            );
        }
    }

    /// Open the confirmation for a restart of the selected target.
    pub fn begin_restart(&mut self, operation_id: AdminId) -> Result<(), IntentError> {
        let targets = self
            .targets
            .as_ref()
            .ok_or(IntentError::UnknownTarget(self.selected.id().into()))?;
        let binding = plan_restart(targets, &self.selected, operation_id)?;
        self.confirming = Some(Confirming::Restart(binding));
        Ok(())
    }

    /// Open the confirmation for interrupting one listed shell.
    pub fn begin_interrupt(&mut self, selector: &BlockerSelector) -> Result<(), IntentError> {
        let blockers = self
            .blockers
            .as_ref()
            .ok_or(IntentError::NoSuchBlocker("-".into()))?;
        let chosen: ShellBlocker = select_blocker(blockers, selector)?.clone();
        if let Some(status) = &self.status
            && let Some(element) = stale_binding(&chosen.binding, status)
        {
            return Err(IntentError::Stale(element));
        }
        self.confirming = Some(Confirming::Interrupt {
            run: chosen.run.to_string(),
            binding: chosen.binding,
        });
        Ok(())
    }

    /// Something to tell the person that is not a reply.
    pub fn push_notice(&mut self, notice: String) {
        self.notices.push(notice);
    }

    pub fn dismiss_confirmation(&mut self) {
        self.confirming = None;
    }

    /// The person pressed the confirm button of the open dialog. This is the
    /// only way a restart or an interrupt command is produced.
    pub fn accept(&mut self, interrupt_op: AdminId) -> Option<PanelRequest> {
        let confirming = self.confirming.take()?;
        let proof = confirm(true, false, true).expect("a click in the dialog is the approval");
        let command = match confirming {
            Confirming::Restart(binding) => {
                self.uncertain.push(binding.operation_id.clone());
                self.own_restart = Some(binding.operation_id.clone());
                request_restart(binding, proof)
            }
            Confirming::Interrupt { binding, .. } => {
                self.uncertain.push(interrupt_op.clone());
                interrupt_shell(binding, interrupt_op, proof)
            }
        };
        Some(PanelRequest::Command(command))
    }

    /// Cancel needs no confirmation but is only offered before capture.
    pub fn cancel(&mut self, operation_id: AdminId) -> Option<PanelRequest> {
        let status = self.status.as_ref()?;
        if !cancellable(status.status.phase) {
            return None;
        }
        cancel_restart(status, operation_id)
            .ok()
            .map(PanelRequest::Command)
    }

    /// A refusal or a lost reply of a request. A restart the core refused for a
    /// conflict is the one place the undisposed graphs are asked about.
    pub fn on_request_failure(
        &mut self,
        command: bool,
        error: &ClientError,
        outcome_unknown: bool,
    ) {
        if outcome_unknown {
            self.notices.push(text::outcome_unknown());
            return;
        }
        if command
            && matches!(error, ClientError::Server(e) if e.code == workbench_protocol::ErrorCode::Conflict)
        {
            self.undisposed_due = true;
        }
        self.on_failure(error);
    }

    /// Ask once whether undisposed graphs are why a restart was refused.
    pub fn take_undisposed_query(&mut self) -> Option<PanelRequest> {
        std::mem::take(&mut self.undisposed_due).then_some(PanelRequest::Query(Query::Lifecycle(
            LifecycleQuery::OwnedGraphs(OwnedGraphsQuery::Undisposed),
        )))
    }

    /// The restart to wait for, from the core's own evidence: what the status
    /// showed, or this panel's accepted restart request that has not ended.
    /// Never from the lost connection alone.
    pub fn planned_restart(
        &self,
        epoch: workbench_protocol::wire::InstanceEpoch,
        now: Instant,
    ) -> Option<PlannedEvidence> {
        self.observer.evidence().cloned().or_else(|| {
            self.own_restart.as_ref().map(|_| PlannedEvidence {
                restart_id: None,
                predecessor_epoch: epoch,
                observed_at: now,
            })
        })
    }

    pub fn on_failure(&mut self, error: &ClientError) {
        self.notices.push(text::failure(error));
    }

    /// The connection ended. A planned restart only on the core's own evidence.
    pub fn planned_restart_evidence(&self) -> Option<&PlannedEvidence> {
        self.observer.evidence()
    }

    /// A fresh session after a reconnect starts from nothing: no held target,
    /// status or confirmation survives into the new epoch.
    pub fn on_new_session(&mut self) {
        self.targets = None;
        self.status = None;
        self.blockers = None;
        self.confirming = None;
        self.observer = EvidenceObserver::default();
        self.undisposed = None;
        self.undisposed_due = false;
    }

    pub fn banner(&self, view: Option<ReconnectView>, expired: Option<Stage>) -> Banner {
        match (view, expired) {
            (_, Some(stage)) => Banner::Expired(stage),
            (Some(view), None) => Banner::Reconnecting(view),
            (None, None) => Banner::None,
        }
    }
}

impl Banner {
    pub fn text(&self) -> Option<String> {
        match self {
            Banner::None => None,
            Banner::Reconnecting(view) => Some(text::reconnect(view.stage, view.remaining)),
            Banner::Expired(stage) => Some(text::expired(*stage).to_owned()),
        }
    }
}
