//! Per-source durable guard. Runtime supplies stable causal batch IDs/signals.
use super::*;
pub(crate) fn guard(conn: &Connection, lane_id: &str) -> Result<LaneWakeGuard, StorageError> {
    lane(conn, lane_id)?;
    let value: String = conn.query_row(
        "SELECT wake_guard_json FROM lanes WHERE lane_id=?1",
        [lane_id],
        |r| r.get(0),
    )?;
    parse(&value)
}
pub(crate) fn update_guard(
    conn: &Connection,
    lane_id: &str,
    expected: u64,
    signal: LaneGuardSignal,
) -> Result<LaneWakeGuard, StorageError> {
    let tx = transaction(conn)?;
    let state = update_guard_in_transaction(&tx, lane_id, expected, signal)?;
    tx.commit()?;
    Ok(state)
}
pub(super) fn update_guard_in_transaction(
    conn: &Connection,
    lane_id: &str,
    expected: u64,
    signal: LaneGuardSignal,
) -> Result<LaneWakeGuard, StorageError> {
    let mut state = guard(conn, lane_id)?;
    if state.revision != expected {
        return Err(StorageError::StaleRevision {
            kind: "wake_guard",
            expected,
            current: state.revision,
        });
    }
    match signal {
        LaneGuardSignal::RealUserInput | LaneGuardSignal::ExplicitUserResume => {
            state.no_progress_batches = 0;
            state.suppressed = false;
        }
        LaneGuardSignal::TerminalArtifact {
            revision,
            failure_episode,
        } => {
            if let Some(episode) = &failure_episode {
                id(episode)?;
            }
            state.failure_episode = failure_episode;
            if let Some(fingerprint) = revision.as_ref().and_then(ArtifactRevision::fingerprint)
                && state.artifact_fingerprint.as_deref() != Some(&fingerprint)
            {
                state.no_progress_batches = 0;
                state.suppressed = false;
                state.artifact_fingerprint = Some(fingerprint);
            }
        }
        LaneGuardSignal::WakeBatch { batch_id } => {
            id(&batch_id)?;
            if state.last_wake_batch_id.as_deref() != Some(&batch_id) {
                state.no_progress_batches = state.no_progress_batches.saturating_add(1);
                state.last_wake_batch_id = Some(batch_id);
                state.suppressed = state.no_progress_batches >= LANE_NO_PROGRESS_LIMIT;
            }
        }
    }
    state.revision = state
        .revision
        .checked_add(1)
        .ok_or(StorageError::OutOfRange("guard revision"))?;
    conn.execute(
        "UPDATE lanes SET wake_guard_json=?2 WHERE lane_id=?1",
        params![lane_id, json(&state)?],
    )?;
    Ok(state)
}
pub(crate) fn capacity_diagnostic(
    conn: &Connection,
    lane_id: &str,
) -> Result<Option<QuestionCapacityBlocked>, StorageError> {
    lane(conn, lane_id)?;
    let value: Option<String> = conn.query_row(
        "SELECT question_capacity_blocked_json FROM lanes WHERE lane_id=?1",
        [lane_id],
        |r| r.get(0),
    )?;
    value.as_deref().map(parse).transpose()
}
