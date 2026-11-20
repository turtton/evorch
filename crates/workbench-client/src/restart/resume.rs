use workbench_protocol::{
    events::{EventCursor, Subscribe, SubscriptionScope, SubscriptionStarted},
    wire::InstanceEpoch,
};

/// How a session is rebuilt on a connection to a core that may have restarted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumePlan {
    /// The same epoch: the held cursor still names a position in this core's stream.
    ReplayAfter(EventCursor),
    /// A new epoch (or nothing held): the old cursor is dropped, a snapshot is
    /// read, and only then is a subscription followed.
    SnapshotFirst,
}

pub fn plan_resume(held: Option<EventCursor>, core_epoch: InstanceEpoch) -> ResumePlan {
    match held {
        Some(cursor) if cursor.core_epoch == core_epoch => ResumePlan::ReplayAfter(cursor),
        _ => ResumePlan::SnapshotFirst,
    }
}

/// The subscribe request of a plan: a snapshot plan never carries a cursor.
pub fn subscribe_request(scope: SubscriptionScope, plan: &ResumePlan) -> Subscribe {
    Subscribe {
        scope,
        after: match plan {
            ResumePlan::ReplayAfter(cursor) => Some(*cursor),
            ResumePlan::SnapshotFirst => None,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    /// The held state stays; every later event follows.
    Replaying,
    /// The held state is replaced by this snapshot.
    Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResumeError {
    #[error("the core replayed where a snapshot was required")]
    ReplayInsteadOfSnapshot,
    #[error("the stream belongs to epoch {got} but the connection is epoch {expected}")]
    EpochMismatch { expected: u64, got: u64 },
}

/// Judge the core's answer to a subscribe against the plan and the epoch of
/// the connection it came on.
pub fn check_started(
    plan: &ResumePlan,
    started: &SubscriptionStarted,
    core_epoch: InstanceEpoch,
) -> Result<Started, ResumeError> {
    if started.high_water.core_epoch != core_epoch {
        return Err(ResumeError::EpochMismatch {
            expected: core_epoch.get(),
            got: started.high_water.core_epoch.get(),
        });
    }
    match (plan, started.snapshot_id.is_some()) {
        (_, true) => Ok(Started::Snapshot),
        (ResumePlan::ReplayAfter(_), false) => Ok(Started::Replaying),
        (ResumePlan::SnapshotFirst, false) => Err(ResumeError::ReplayInsteadOfSnapshot),
    }
}

/// An event of another epoch than the stream was started in must not be applied.
pub fn event_in_epoch(cursor: &EventCursor, core_epoch: InstanceEpoch) -> bool {
    cursor.core_epoch == core_epoch
}
