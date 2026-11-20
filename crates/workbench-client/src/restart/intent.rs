use super::confirmation::Confirmed;
use workbench_protocol::{
    commands::Command,
    ids::RestartId,
    namespaces::lifecycle::{
        AdminId, LifecycleCommand, RestartPhase,
        restart::{
            CancelRestart, InterruptBinding, InterruptShell, RequestRestart, RestartBinding,
            RestartCommand, RestartManagementStatus, RestartTargets, ShellBlocker, ShellBlockers,
        },
    },
    validation::ValidationError,
};

/// The target every command line means unless it names another.
pub const CURRENT_TARGET: &str = "current";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSelector {
    Current,
    Id(AdminId),
}
impl TargetSelector {
    /// An opaque ID only; a path, an argument vector or an environment is not one.
    pub fn parse(text: &str) -> Result<Self, ValidationError> {
        if text == CURRENT_TARGET {
            return Ok(Self::Current);
        }
        AdminId::new(text).map(Self::Id)
    }
    pub fn id(&self) -> &str {
        match self {
            Self::Current => CURRENT_TARGET,
            Self::Id(id) => id.as_str(),
        }
    }
}
impl Default for TargetSelector {
    fn default() -> Self {
        Self::Current
    }
}

/// Why a command was not built. Nothing was sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntentError {
    #[error("the core offers no restart target `{0}`")]
    UnknownTarget(String),
    #[error("the target list was cut off; `{0}` may exist but cannot be bound")]
    TargetsTruncated(String),
    #[error("no restart is being prepared")]
    NoRestart,
    #[error("shell blocker {0} is not in the list the core gave")]
    NoSuchBlocker(String),
    #[error("what was shown is stale: {0} changed")]
    Stale(&'static str),
    #[error(transparent)]
    Invalid(#[from] ValidationError),
}

/// Bind a restart to the target revision and core epoch the person saw.
pub fn plan_restart(
    targets: &RestartTargets,
    selector: &TargetSelector,
    operation_id: AdminId,
) -> Result<RestartBinding, IntentError> {
    let id = selector.id();
    match targets.targets.iter().find(|t| t.id.as_str() == id) {
        Some(target) => Ok(RestartBinding {
            target_id: target.id.clone(),
            expected_core_epoch: targets.core_epoch,
            target_revision: target.revision,
            operation_id,
        }),
        None if targets.truncated => Err(IntentError::TargetsTruncated(id.into())),
        None => Err(IntentError::UnknownTarget(id.into())),
    }
}

pub fn request_restart(binding: RestartBinding, _confirmed: Confirmed) -> Command {
    Command::Lifecycle(LifecycleCommand::Restart(RestartCommand::Request(
        RequestRestart {
            binding,
            confirmed: true,
        },
    )))
}

/// Cancel what `status` shows. Cancel needs no confirmation: it only ever
/// returns toward the running state, and the core classifies it so.
pub fn cancel_restart(
    status: &RestartManagementStatus,
    operation_id: AdminId,
) -> Result<Command, IntentError> {
    let restart_id = status
        .status
        .restart_id
        .as_ref()
        .ok_or(IntentError::NoRestart)?;
    Ok(Command::Lifecycle(LifecycleCommand::Restart(
        RestartCommand::Cancel(CancelRestart {
            restart_id: admin_id(restart_id)?,
            expected_core_epoch: status.core_epoch,
            expected_revision: status.status.revision.into(),
            operation_id,
        }),
    )))
}

fn admin_id(restart_id: &RestartId) -> Result<AdminId, IntentError> {
    Ok(AdminId::new(restart_id.as_str())?)
}

/// How a person points at one blocker of the list that was shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockerSelector {
    Index(usize),
    Run(String),
}

/// The blocker to interrupt, taken from `blockers` untouched, after checking
/// that it still describes `status`. The binding is never edited: whatever
/// differs from the core's current state is for the core to refuse.
pub fn select_blocker<'a>(
    blockers: &'a ShellBlockers,
    selector: &BlockerSelector,
) -> Result<&'a ShellBlocker, IntentError> {
    let found = match selector {
        BlockerSelector::Index(i) => blockers.blockers.get(*i),
        BlockerSelector::Run(run) => blockers.blockers.iter().find(|b| b.run.as_str() == run),
    };
    found.ok_or_else(|| {
        IntentError::NoSuchBlocker(match selector {
            BlockerSelector::Index(i) => format!("#{i}"),
            BlockerSelector::Run(run) => run.clone(),
        })
    })
}

/// The first element of the binding that no longer matches the status.
pub fn stale_binding(
    binding: &InterruptBinding,
    status: &RestartManagementStatus,
) -> Option<&'static str> {
    let same_restart = status
        .status
        .restart_id
        .as_ref()
        .is_some_and(|id| id.as_str() == binding.restart_id.as_str());
    if !same_restart {
        Some("restart")
    } else if binding.core_epoch != status.core_epoch {
        Some("core epoch")
    } else if binding.revision.get() != status.status.revision {
        Some("revision")
    } else if binding.phase != status.status.phase {
        Some("phase")
    } else {
        None
    }
}

pub fn interrupt_shell(
    binding: InterruptBinding,
    operation_id: AdminId,
    _confirmed: Confirmed,
) -> Command {
    Command::Lifecycle(LifecycleCommand::Restart(RestartCommand::Interrupt(
        InterruptShell {
            binding,
            operation_id,
            confirmed: true,
        },
    )))
}

/// Phases in which the core can still turn back to running, and so the person
/// can still cancel. After capture the old work is never revived.
pub fn cancellable(phase: RestartPhase) -> bool {
    matches!(
        phase,
        RestartPhase::Requested
            | RestartPhase::PreparingDrains
            | RestartPhase::WaitingSafePoint
            | RestartPhase::Sealing
            | RestartPhase::Sealed
    )
}
