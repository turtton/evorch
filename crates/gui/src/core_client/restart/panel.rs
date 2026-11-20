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
    commands::Command,
    namespaces::lifecycle::{
        AdminId, AdminReceipt, LifecycleQuery, LifecycleReply,
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

    pub fn status(&self) -> Option<&RestartManagementStatus> {
        self.status.as_ref()
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
            _ => {}
        }
    }

    fn on_receipt(&mut self, receipt: &AdminReceipt) {
        self.notices.extend(text::receipt(receipt));
        if receipt.state
            == workbench_protocol::namespaces::lifecycle::AdminOperationState::Completed
        {
            self.uncertain.retain(|id| id != &receipt.operation_id);
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
