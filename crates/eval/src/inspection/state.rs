//! How the facts on disk and in the launching process become states. Both the
//! `eval_report` tool and the GUI read states through here, so they cannot
//! disagree about what a missing exit or an unfinished trial means.

use crate::driver::Status;
use crate::output::RunEntry;

use super::snapshot::{GroupPhase, MemberPhase, ProcessStatus, TrialState};

/// What is known of whether an evaluation process can still change its files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Activity {
    Running,
    Ended,
    Unknown,
}

/// A process this process did not see end has no recorded exit, which is also
/// what a running one looks like. Only a finished manifest and a complete
/// report show that nothing is left to do.
pub(super) fn activity(process: &ProcessStatus, manifest_finished: bool, report: bool) -> Activity {
    match process {
        ProcessStatus::Running => Activity::Running,
        ProcessStatus::Exited(_) => Activity::Ended,
        ProcessStatus::Unknown if manifest_finished && report => Activity::Ended,
        ProcessStatus::Unknown => Activity::Unknown,
    }
}

pub(super) fn trial_state(
    entry: Option<&RunEntry>,
    activity: Activity,
    manifest_finished: bool,
) -> TrialState {
    if let Some(entry) = entry
        && entry.finished_at.is_some()
    {
        return match entry.status {
            Some(Status::Failed | Status::PrepareFailed) => TrialState::Failed,
            _ => TrialState::Finished,
        };
    }
    // The replay loop is over once the manifest says so, whoever ran it.
    if manifest_finished || activity == Activity::Ended {
        return if entry.is_some() {
            TrialState::Interrupted
        } else {
            TrialState::Unavailable
        };
    }
    match (activity, entry) {
        (Activity::Running, Some(_)) => TrialState::Running,
        (Activity::Running, None) => TrialState::Queued,
        _ => TrialState::Unverified,
    }
}

/// `manifest_finished` is `None` while there is no usable manifest.
pub(super) fn member_phase(
    process: &ProcessStatus,
    manifest_finished: Option<bool>,
    report: bool,
) -> MemberPhase {
    match process {
        ProcessStatus::Exited(exit) if !exit.succeeded() => MemberPhase::Failed,
        ProcessStatus::Exited(_) if report => MemberPhase::Finished,
        ProcessStatus::Exited(_) => MemberPhase::FinishedWithoutReport,
        ProcessStatus::Running => match manifest_finished {
            None => MemberPhase::Starting,
            Some(false) => MemberPhase::RunningTrials,
            Some(true) => MemberPhase::Reporting,
        },
        ProcessStatus::Unknown if manifest_finished == Some(true) && report => {
            MemberPhase::Finished
        }
        ProcessStatus::Unknown => MemberPhase::Unverified,
    }
}

pub(super) fn group_phase(phases: &[MemberPhase]) -> GroupPhase {
    if phases.iter().any(|phase| phase.is_active()) {
        GroupPhase::Running
    } else if phases.contains(&MemberPhase::Unverified) {
        GroupPhase::Unverified
    } else if !phases.is_empty() && phases.iter().all(|phase| *phase == MemberPhase::Finished) {
        GroupPhase::Finished
    } else {
        GroupPhase::Incomplete
    }
}
