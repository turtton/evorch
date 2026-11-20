//! Public restart leaf contract. Product coordinator registration is deferred.
use super::contracts::*;
use crate::{
    validation::ValidationError,
    wire::{InstanceEpoch, Revision},
};
use serde::{Deserialize, Serialize};
mod legacy;
pub use legacy::*;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartBinding {
    pub target_id: AdminId,
    pub expected_core_epoch: InstanceEpoch,
    pub target_revision: Revision,
    pub operation_id: AdminId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRestart {
    pub binding: RestartBinding,
    pub confirmed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRestart {
    pub restart_id: AdminId,
    pub expected_core_epoch: InstanceEpoch,
    pub expected_revision: Revision,
    pub operation_id: AdminId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptBinding {
    pub restart_id: AdminId,
    pub core_epoch: InstanceEpoch,
    pub connection_epoch: InstanceEpoch,
    pub run_incarnation: AdminId,
    pub blocker_identity: AdminId,
    pub revision: Revision,
    pub phase: RestartPhase,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptShell {
    pub binding: InterruptBinding,
    pub operation_id: AdminId,
    pub confirmed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RestartCommand {
    Request(RequestRestart),
    Cancel(CancelRestart),
    Interrupt(InterruptShell),
}
impl RestartCommand {
    pub fn action(&self) -> AdminAction {
        match self {
            Self::Request(_) => AdminAction::Request,
            Self::Cancel(_) => AdminAction::Cancel,
            Self::Interrupt(_) => AdminAction::Interrupt,
        }
    }
    pub fn operation_id(&self) -> &AdminId {
        match self {
            Self::Request(r) => &r.binding.operation_id,
            Self::Cancel(r) => &r.operation_id,
            Self::Interrupt(r) => &r.operation_id,
        }
    }
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Request(r) => confirmation(r.confirmed),
            Self::Interrupt(r) => confirmation(r.confirmed),
            Self::Cancel(_) => Ok(()),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RestartQuery {
    Targets,
    Status,
    ShellBlockers,
}
impl RestartQuery {
    pub fn action(&self) -> AdminAction {
        match self {
            Self::Targets => AdminAction::Targets,
            Self::Status | Self::ShellBlockers => AdminAction::Status,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartTarget {
    pub id: AdminId,
    pub revision: Revision,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartTargets {
    /// Process management epoch, independent of the event cursor epoch.
    pub core_epoch: InstanceEpoch,
    pub revision: Revision,
    pub targets: Vec<RestartTarget>,
    pub truncated: bool,
}
/// Public management status; the legacy quiesce DTO remains unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartManagementStatus {
    pub audit: AdminAuditDiagnostics,
    pub core_epoch: InstanceEpoch,
    pub status: RestartStatus,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RestartReply {
    Targets(RestartTargets),
    Status(RestartManagementStatus),
    ShellBlockers(ShellBlockers),
}
impl RestartReply {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Targets(t) => items(&t.targets)?,
            Self::Status(s) => s.status.validate()?,
            Self::ShellBlockers(s) => {
                items(&s.blockers)?;
                if s.total.get().checked_sub(s.blockers.len() as u64) != Some(s.omitted.get()) {
                    return Err(ValidationError::Inconsistent {
                        field: "restart.shell_blockers",
                        reason: "total must include displayed and omitted blockers",
                    });
                }
            }
        };
        bounded(self)
    }
}

/// Process-local, bounded best-effort audit diagnostics. All values reset on restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminAuditDiagnostics {
    /// Aggregate rows dropped, unknown, or lost to cross-window queue saturation.
    pub denial_persistence_failures: Revision,
    pub denial_loss: Option<DenialLossSummary>,
}
/// Denials lost specifically to cross-window queue saturation. Memory only: this
/// is not an audit row, never rewrites timestamps, and is cleared on restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DenialLossSummary {
    pub count: Revision,
    pub first_ms: Revision,
    pub last_ms: Revision,
}

/// Opaque confirmation handles, never shell arguments, output, paths or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellBlocker {
    pub run: crate::ids::RunId,
    pub teardown_confirmed: bool,
    pub observed: bool,
    pub binding: InterruptBinding,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellBlockers {
    pub blockers: Vec<ShellBlocker>,
    pub total: Revision,
    pub omitted: Revision,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn blocker() -> ShellBlocker {
        ShellBlocker {
            run: crate::ids::RunId::new("run").unwrap(),
            teardown_confirmed: false,
            observed: false,
            binding: InterruptBinding {
                restart_id: AdminId::new("restart").unwrap(),
                core_epoch: InstanceEpoch(1),
                connection_epoch: InstanceEpoch(2),
                run_incarnation: AdminId::new("incarnation").unwrap(),
                blocker_identity: AdminId::new("shell").unwrap(),
                revision: Revision(3),
                phase: RestartPhase::WaitingSafePoint,
            },
        }
    }
    #[test]
    fn shell_blocker_projection_is_bounded_and_counts_omissions_exactly() {
        let blocker = blocker();
        let reply = |n, total, omitted| {
            RestartReply::ShellBlockers(ShellBlockers {
                blockers: vec![blocker.clone(); n],
                total: Revision(total),
                omitted: Revision(omitted),
            })
        };
        assert!(reply(0, 0, 0).validate().is_ok());
        assert!(reply(64, 70, 6).validate().is_ok());
        assert!(reply(65, 65, 0).validate().is_err());
        assert!(reply(64, 63, 0).validate().is_err());
        assert!(reply(64, 70, 5).validate().is_err());
        assert!(reply(0, u64::MAX, u64::MAX).validate().is_ok());
        assert!(reply(1, u64::MAX, u64::MAX).validate().is_err());
    }
    #[test]
    fn shell_blocker_wire_retains_confirmation_binding_and_rejects_extra_fields() {
        let query = serde_json::to_value(RestartQuery::ShellBlockers).unwrap();
        assert_eq!(query, serde_json::json!({"type": "shell_blockers"}));
        assert_eq!(
            serde_json::from_value::<RestartQuery>(query).unwrap(),
            RestartQuery::ShellBlockers
        );
        let original = blocker();
        let wire = serde_json::to_value(&original).unwrap();
        assert_eq!(
            serde_json::from_value::<ShellBlocker>(wire.clone()).unwrap(),
            original
        );
        let mut extra = wire.clone();
        extra["path"] = serde_json::json!("/not-a-wire-target");
        assert!(serde_json::from_value::<ShellBlocker>(extra).is_err());
        let mut invalid_id = wire.clone();
        invalid_id["binding"]["blocker_identity"] = serde_json::json!("x".repeat(129));
        assert!(serde_json::from_value::<ShellBlocker>(invalid_id).is_err());
        let mut missing = wire;
        missing["binding"]
            .as_object_mut()
            .unwrap()
            .remove("connection_epoch");
        assert!(serde_json::from_value::<ShellBlocker>(missing).is_err());
    }
    #[test]
    fn maximum_shell_projection_fits_the_total_wire_budget() {
        let mut b = blocker();
        // Escaping doubles the run ID's serialized size without violating its byte limit.
        b.run = crate::ids::RunId::new("\\".repeat(crate::limits::MAX_ID_BYTES)).unwrap();
        let maximum = AdminId::new("x".repeat(128)).unwrap();
        b.binding.restart_id = maximum.clone();
        b.binding.run_incarnation = maximum.clone();
        b.binding.blocker_identity = maximum;
        b.binding.core_epoch = InstanceEpoch(u64::MAX);
        b.binding.connection_epoch = InstanceEpoch(u64::MAX);
        b.binding.revision = Revision(u64::MAX);
        let reply = RestartReply::ShellBlockers(ShellBlockers {
            blockers: vec![b; MAX_ADMIN_ITEMS],
            total: Revision(u64::MAX),
            omitted: Revision(u64::MAX - MAX_ADMIN_ITEMS as u64),
        });
        reply.validate().unwrap();
        assert!(serde_json::to_vec(&reply).unwrap().len() <= MAX_ADMIN_REPLY_BYTES);
    }
}
