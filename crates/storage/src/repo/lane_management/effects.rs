use super::*;
use event_bus::UserQuestion;
use event_bus::lane::{
    LaneNotification, LaneNotificationKind, LaneNudgeState, LaneStallEpisode, QuestionResponder,
    QuestionRouting,
};
pub(super) fn questions(
    c: &Connection,
    old: &LaneRecord,
    new: &LaneRecord,
) -> Result<(), StorageError> {
    let mut s=c.prepare("SELECT r.payload,q.payload FROM lane_question_routes r JOIN user_questions q ON q.id=r.question_id WHERE r.lane_id=?1 AND q.answered=0 AND r.requester_run_id NOT IN (SELECT run_id FROM run_tombstones) AND r.requester_root_run_id NOT IN (SELECT run_id FROM run_tombstones) AND r.requester_thread_id NOT IN (SELECT thread_id FROM thread_tombstones)")?;
    let rows = s
        .query_map([&old.lane_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (route, q) in rows {
        let mut route: QuestionRouting = parse(&route)?;
        let mut q: UserQuestion = parse(&q)?;
        if matches!(route.responder, QuestionResponder::Lead { .. })
            && new.policy != LanePolicy::Attached
        {
            route.responder = QuestionResponder::User;
            route.routing_revision = route
                .routing_revision
                .checked_add(1)
                .ok_or(StorageError::OutOfRange("routing revision"))?;
        }
        route.management_revision = new.management_revision;
        q.responder = Some(route.responder.clone());
        q.routing_revision = route.routing_revision;
        q.management_revision = Some(route.management_revision);
        let responder = match &route.responder {
            QuestionResponder::Lead { thread_id } => Some(thread_id.as_str()),
            _ => None,
        };
        c.execute("UPDATE lane_question_routes SET responder_thread_id=?2,management_revision=?3,routing_revision=?4,payload=?5 WHERE question_id=?1",params![route.question_id,responder,route.management_revision,route.routing_revision,json(&route)?])?;
        c.execute(
            "UPDATE user_questions SET payload=?2 WHERE id=?1",
            params![q.id, json(&q)?],
        )?;
    }
    Ok(())
}
pub(super) fn notifications(
    c: &Connection,
    old: &LaneRecord,
    new: &LaneRecord,
) -> Result<(), StorageError> {
    // Tombstoned destinations are already fenced and must not be updated.
    let mut s=c.prepare("SELECT payload FROM lane_notifications WHERE source_kind='lane' AND lane_id=?1 AND state='pending' AND ((source_kind='lane' AND event_kind='answer') OR source_thread_id NOT IN (SELECT thread_id FROM thread_tombstones)) AND destination_thread_id NOT IN (SELECT thread_id FROM thread_tombstones)")?;
    let notices = s
        .query_map([&old.lane_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let set_policy = old.policy.is_managed() && new.policy.is_managed();
    for payload in notices {
        let mut n: LaneNotification = parse(&payload)?;
        // Release the entire touched batch so unrelated lanes are immediately
        // retryable. Ownership, destination epoch and Stop are unchanged.
        c.execute("UPDATE lane_delivery_destinations SET claim_epoch=claim_epoch+1,lease_token=NULL,lease_until_ns=NULL WHERE thread_id=?1 AND lease_token IS NOT NULL",[&n.destination_thread_id])?;
        c.execute("UPDATE lane_notifications SET claim_epoch=NULL WHERE destination_thread_id=?1 AND state='pending' AND ((source_kind='lane' AND event_kind='answer') OR source_thread_id NOT IN (SELECT thread_id FROM thread_tombstones))",[&n.destination_thread_id])?;
        let to_lane = n.destination_thread_id == new.thread_id;
        let keep = match &n.kind {
            LaneNotificationKind::Answer { .. } => to_lane,
            LaneNotificationKind::Nudge { .. } => set_policy && to_lane,
            LaneNotificationKind::Ready { .. }
            | LaneNotificationKind::Failed { .. }
            | LaneNotificationKind::Closed { .. }
            | LaneNotificationKind::Intervention { .. } => {
                set_policy && Some(&n.destination_thread_id) == new.lead_thread_id.as_ref()
            }
            LaneNotificationKind::Milestone => set_policy && new.policy == LanePolicy::Attached,
            _ => false,
        };
        if keep {
            n.management_revision = new.management_revision;
            // Same notification ID, semantic key and receipt; never duplicate input.
            c.execute("UPDATE lane_notifications SET management_revision=?2,payload=?3 WHERE notification_id=?1",params![n.notification_id,new.management_revision,json(&n)?])?;
        } else {
            c.execute(
                "UPDATE lane_notifications SET state='obsolete' WHERE notification_id=?1",
                [&n.notification_id],
            )?;
        }
    }
    Ok(())
}
pub(super) fn episodes(
    c: &Connection,
    old: &LaneRecord,
    new: &LaneRecord,
    at: i64,
) -> Result<(), StorageError> {
    let mut s=c.prepare("SELECT payload FROM lane_stall_episodes WHERE lane_id=?1 AND management_revision=?2 AND json_extract(payload,'$.resolved_at_ns') IS NULL")?;
    let rows = s
        .query_map(params![old.lane_id, old.management_revision], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for payload in rows {
        let mut e: LaneStallEpisode = parse(&payload)?;
        e.revision += 1;
        if old.policy.is_managed() && new.policy.is_managed() {
            e.management_revision = new.management_revision;
        } else {
            e.resolved_at_ns = Some(at);
            if e.nudge_state == LaneNudgeState::Pending {
                e.nudge_state = LaneNudgeState::Obsolete;
            }
        }
        c.execute(
            "UPDATE lane_stall_episodes SET management_revision=?2,payload=?3 WHERE episode_id=?1",
            params![e.episode_id, e.management_revision, json(&e)?],
        )?;
    }
    Ok(())
}
