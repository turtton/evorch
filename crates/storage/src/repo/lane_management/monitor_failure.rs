//! The permanent nudge-failure observation and latest progress are compared in
//! the same transaction as the synthetic Failed report; Ready always wins.
use super::*;
use crate::lane_control::{LaneOperationError, LaneReportProducer, LaneStatus};
use crate::lane_management::LaneMonitorFailure;
pub(crate) fn append(
    c: &Connection,
    r: LaneMonitorFailure,
) -> Result<Option<crate::lanes::LaneReportAppended>, StorageError> {
    let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
    let old = lane(&tx, &r.observation.lane_id)?;
    let Some(e) = crate::repo::lane_control::episode(&tx, &r.episode_id)? else {
        return Ok(None);
    };
    let progress = crate::repo::lane_control::progress_anchor(&tx, &old.lane_id)?;
    let latest = crate::repo::lanes::reports(
        &tx,
        &old.lane_id,
        old.latest_report_seq.saturating_sub(1),
        1,
    )?
    .pop();
    if e.resolved_at_ns.is_some()
        || e.nudge_state != event_bus::lane::LaneNudgeState::Failed
        || e.revision != r.expected_episode_revision
        || e.lane_id != old.lane_id
        || e.management_revision != r.observation.management_revision
        || e.progress_anchor != r.observation.progress_anchor
        || progress != e.progress_anchor
        || old.latest_report_seq != r.expected_report_seq
        || latest.is_some_and(|p| p.status != LaneStatus::Working)
    {
        return Ok(None);
    }
    crate::repo::lane_control::check_observation(&tx, &r.observation)?;
    if r.report.status != LaneStatus::Failed
        || r.report.producer != LaneReportProducer::Harness
        || r.report.failure_episode.as_deref() != Some(&r.episode_id)
    {
        return Err(StorageError::LaneOperation(
            LaneOperationError::InvalidInput,
        ));
    }
    let appended = crate::repo::lane_delivery::append_report_in_transaction(
        &tx,
        &old.lane_id,
        r.observation.management_revision,
        r.report,
        r.observation.at_ns,
    )?;
    tx.commit()?;
    Ok(Some(appended))
}
