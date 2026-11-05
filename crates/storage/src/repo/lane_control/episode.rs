use super::*;
fn check_observation(conn: &Connection, r: &ObserveLaneStall) -> Result<LaneRecord, StorageError> {
    let l = managed(conn, &r.lane_id, r.management_revision, None)?;
    let destination = crate::repo::lane_delivery::destination(conn, &l.thread_id)?
        .ok_or_else(|| err(LaneOperationError::OwnershipLost))?;
    let deleted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",
        [&r.owner.root_run_id],
        |row| row.get(0),
    )?;
    if deleted
        || destination.stopped
        || destination.owner.as_ref() != Some(&r.owner)
        || l.root_run_id.as_deref() != Some(&r.owner.root_run_id)
        || l.generation != r.owner.generation
    {
        return Err(err(LaneOperationError::OwnershipLost));
    }
    if progress_anchor(conn, &r.lane_id)? != r.progress_anchor {
        return Err(err(LaneOperationError::StaleEpisode));
    }
    Ok(l)
}
pub(crate) fn episode(
    conn: &Connection,
    episode_id: &str,
) -> Result<Option<LaneStallEpisode>, StorageError> {
    let s:Option<String>=conn.query_row("SELECT e.payload FROM lane_stall_episodes e JOIN lanes l ON l.lane_id=e.lane_id WHERE episode_id=?1 AND l.thread_id NOT IN (SELECT thread_id FROM thread_tombstones)",[episode_id],|r|r.get(0)).optional()?;
    s.as_deref().map(parse).transpose()
}
fn save(conn: &Connection, e: &LaneStallEpisode, previous: u64) -> Result<(), StorageError> {
    if conn.execute("UPDATE lane_stall_episodes SET payload=?2 WHERE episode_id=?1 AND json_extract(payload,'$.revision')=?3",params![e.episode_id,bounded(e,4096)?,previous])?!=1 {return Err(err(LaneOperationError::StaleEpisode));}
    Ok(())
}
pub(crate) fn observe_stall(
    conn: &Connection,
    r: ObserveLaneStall,
) -> Result<LaneStallEpisode, StorageError> {
    let tx = tx(conn)?;
    let l = check_observation(&tx, &r)?;
    let episode_id = format!(
        "le-{}",
        digest(&(&l.lane_id, l.management_revision, &r.progress_anchor))?
    );
    if let Some(e) = episode(&tx, &episode_id)? {
        return Ok(e);
    }
    let e = LaneStallEpisode {
        episode_id,
        lane_id: l.lane_id,
        management_revision: l.management_revision,
        revision: 1,
        classification: r.classification,
        progress_anchor: r.progress_anchor,
        first_detected_at_ns: r.at_ns,
        resolved_at_ns: None,
        nudge_state: LaneNudgeState::NotRequested,
        notification_id: None,
    };
    tx.execute("INSERT INTO lane_stall_episodes(episode_id,lane_id,management_revision,progress_epoch,payload) VALUES (?1,?2,?3,?4,?5)",params![e.episode_id,e.lane_id,e.management_revision,e.progress_anchor.epoch,bounded(&e,4096)?])?;
    tx.commit()?;
    Ok(e)
}
pub(crate) fn request_nudge(
    conn: &Connection,
    r: ObserveLaneStall,
    episode_id: &str,
    expected_revision: u64,
) -> Result<LaneStallEpisode, StorageError> {
    let tx = tx(conn)?;
    let l = check_observation(&tx, &r)?;
    let mut e = episode(&tx, episode_id)?.ok_or_else(|| err(LaneOperationError::StaleEpisode))?;
    if e.lane_id != l.lane_id
        || e.management_revision != l.management_revision
        || e.progress_anchor != r.progress_anchor
        || e.resolved_at_ns.is_some()
    {
        return Err(err(LaneOperationError::StaleEpisode));
    }
    // Both live and SilentDone use this same durable one-shot slot.
    if e.nudge_state != LaneNudgeState::NotRequested {
        return Ok(e);
    }
    if e.revision != expected_revision {
        return Err(err(LaneOperationError::StaleEpisode));
    }
    let previous = e.revision;
    e.revision += 1;
    if crate::repo::lane_delivery::guard(&tx, &l.lane_id)?.suppressed {
        e.nudge_state = LaneNudgeState::Suppressed;
    } else {
        e.nudge_state = LaneNudgeState::Pending;
        e.notification_id = Some(crate::repo::lane_delivery::enqueue(
            &tx,
            LaneNotification {
                notification_id: String::new(),
                lane_id: l.lane_id,
                management_revision: l.management_revision,
                source_thread_id: l.thread_id.clone(),
                destination_thread_id: l.thread_id,
                kind: LaneNotificationKind::Nudge {
                    episode_id: e.episode_id.clone(),
                },
                report_seq: None,
                created_at_ns: r.at_ns,
            },
        )?);
    }
    save(&tx, &e, previous)?;
    tx.commit()?;
    Ok(e)
}
/// Runtime reports a permanent delivery failure or suppression; it cannot renew
/// the slot. Accepted is committed only with the durable input receipt below.
pub(crate) fn nudge_outcome(
    conn: &Connection,
    r: ObserveLaneStall,
    episode_id: &str,
    revision: u64,
    state: LaneNudgeState,
) -> Result<LaneStallEpisode, StorageError> {
    if !matches!(state, LaneNudgeState::Failed | LaneNudgeState::Suppressed) {
        return Err(err(LaneOperationError::InvalidInput));
    }
    let tx = tx(conn)?;
    check_observation(&tx, &r)?;
    let mut e = episode(&tx, episode_id)?.ok_or_else(|| err(LaneOperationError::StaleEpisode))?;
    if e.lane_id != r.lane_id
        || e.management_revision != r.management_revision
        || e.progress_anchor != r.progress_anchor
        || e.resolved_at_ns.is_some()
        || e.revision != revision
        || e.nudge_state != LaneNudgeState::Pending
    {
        return Err(err(LaneOperationError::StaleEpisode));
    }
    e.nudge_state = state;
    e.revision += 1;
    save(&tx, &e, revision)?;
    tx.execute("UPDATE lane_notifications SET state='obsolete' WHERE notification_id=?1 AND state='pending' AND receipt_prompt_id IS NULL",[&e.notification_id])?;
    tx.commit()?;
    Ok(e)
}
pub(crate) fn resolve_episodes(
    conn: &Connection,
    lane_id: &str,
    at: i64,
) -> Result<(), StorageError> {
    let mut s=conn.prepare("SELECT payload FROM lane_stall_episodes WHERE lane_id=?1 AND json_extract(payload,'$.resolved_at_ns') IS NULL")?;
    let entries = s
        .query_map([lane_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for text in entries {
        let mut e: LaneStallEpisode = parse(&text)?;
        let previous = e.revision;
        e.revision += 1;
        e.resolved_at_ns = Some(at);
        if matches!(
            e.nudge_state,
            LaneNudgeState::NotRequested | LaneNudgeState::Pending
        ) {
            e.nudge_state = LaneNudgeState::Obsolete;
        }
        save(conn, &e, previous)?;
        conn.execute("UPDATE lane_notifications SET state='obsolete' WHERE notification_id=?1 AND state='pending' AND receipt_prompt_id IS NULL",[e.notification_id])?;
    }
    Ok(())
}
/// Checked at delivery acceptance as well as claim, so a progress race cannot
/// inject an obsolete nudge. B additionally checks runtime terminal eligibility.
pub(crate) fn check_nudge(
    conn: &Connection,
    notice: &LaneNotification,
) -> Result<(), StorageError> {
    if let LaneNotificationKind::Nudge { episode_id } = &notice.kind {
        let e = episode(conn, episode_id)?.ok_or_else(|| err(LaneOperationError::StaleEpisode))?;
        if e.lane_id != notice.lane_id
            || e.management_revision != notice.management_revision
            || e.resolved_at_ns.is_some()
            || !matches!(
                e.nudge_state,
                LaneNudgeState::Pending | LaneNudgeState::Accepted
            )
            || e.notification_id.as_deref() != Some(&notice.notification_id)
            || progress_anchor(conn, &e.lane_id)? != e.progress_anchor
        {
            return Err(err(LaneOperationError::StaleEpisode));
        }
    }
    Ok(())
}
pub(crate) fn accept_nudge(
    conn: &Connection,
    notice: &LaneNotification,
) -> Result<(), StorageError> {
    if let LaneNotificationKind::Nudge { episode_id } = &notice.kind {
        check_nudge(conn, notice)?;
        let mut e =
            episode(conn, episode_id)?.ok_or_else(|| err(LaneOperationError::StaleEpisode))?;
        let previous = e.revision;
        e.revision += 1;
        e.nudge_state = LaneNudgeState::Accepted;
        save(conn, &e, previous)?;
    }
    Ok(())
}
