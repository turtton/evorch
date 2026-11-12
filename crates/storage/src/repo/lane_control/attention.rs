//! Derived unresolved state, not a second notifications ledger. Snapshot cursors
//! fail closed on any projection change and survive a restart unchanged.
use super::*;
#[derive(Serialize, serde::Deserialize)]
struct Cursor {
    filter: String,
    snapshot: String,
    position: usize,
}
fn preview(s: &str) -> String {
    let mut n = s.len().min(256);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].into()
}
fn item(
    l: &LaneRecord,
    key: &impl Serialize,
    revision: u64,
    since: i64,
    kind: LaneAttentionKind,
    summary: &str,
    actions: Vec<LaneAttentionAction>,
) -> Result<LaneAttentionItem, StorageError> {
    Ok(LaneAttentionItem {
        id: digest(&(&l.lane_id, l.management_revision, key))?,
        lane_id: l.lane_id.clone(),
        lead_thread_id: l.lead_thread_id.clone().unwrap_or_default(),
        management_revision: l.management_revision,
        revision,
        kind,
        since_at_ns: since,
        summary: preview(summary),
        evidence_refs: vec![],
        available_actions: actions,
    })
}
/// What the ledger knows of the review of a Ready report. Informative only: no state here
/// stops the lead (or a person) from closing. Derives only; reading changes nothing.
pub(crate) fn ready_readiness(
    conn: &Connection,
    lane_id: &str,
    report: &LaneReport,
) -> Result<LaneReadyState, StorageError> {
    let state:Option<String>=conn.query_row("SELECT payload FROM lane_verifications WHERE lane_id=?1 AND reviewer_run_id NOT IN (SELECT run_id FROM run_tombstones) ORDER BY rowid DESC LIMIT 1",[lane_id],|r|r.get(0)).optional()?;
    let verification = state
        .as_deref()
        .map(parse::<LaneVerification>)
        .transpose()?;
    let current = verification.filter(|v| target(conn, lane_id).is_ok_and(|t| v.target == t));
    Ok(match current {
        Some(v) => match (derived_state(conn, &v)?, v.submission.as_ref()) {
            (LaneVerificationState::Pending | LaneVerificationState::Submitted, _) => {
                LaneReadyState::ReviewInProgress
            }
            (LaneVerificationState::Incomplete, _) => LaneReadyState::ReviewIncomplete,
            (LaneVerificationState::Completed, Some(s))
                if s.verdict == LaneVerificationVerdict::Approved
                    && s.checks.iter().all(|c| c.passed) =>
            {
                LaneReadyState::AwaitingClose
            }
            _ => LaneReadyState::ReviewRejected,
        },
        None if report.artifact_revision.is_none() => LaneReadyState::NoEvidence,
        None => LaneReadyState::Unreviewed,
    })
}
fn items(
    conn: &Connection,
    f: &LaneAttentionFilter,
) -> Result<Vec<LaneAttentionItem>, StorageError> {
    let mut rows=conn.prepare("SELECT l.lane_id FROM lanes l JOIN threads t ON t.thread_id=l.thread_id WHERE l.lead_thread_id=?1 AND l.policy<>'detached' AND (?2 IS NULL OR t.project_id=?2) AND l.thread_id NOT IN (SELECT thread_id FROM thread_tombstones) AND l.lead_thread_id NOT IN (SELECT thread_id FROM thread_tombstones) ORDER BY l.lane_id")?;
    let ids = rows
        .query_map(params![f.lead_thread_id, f.project_id], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = Vec::new();
    for lane_id in ids {
        let l = lane(conn, &lane_id)?;
        let mut s=conn.prepare("SELECT r.payload,q.payload FROM lane_question_routes r JOIN user_questions q ON q.id=r.question_id WHERE r.lane_id=?1 AND r.management_revision=?2 AND q.answered=0 AND r.requester_run_id NOT IN (SELECT run_id FROM run_tombstones) AND r.requester_root_run_id NOT IN (SELECT run_id FROM run_tombstones) AND r.requester_thread_id NOT IN (SELECT thread_id FROM thread_tombstones) ORDER BY r.question_id")?;
        for row in s.query_map(params![lane_id, l.management_revision], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (route, q) = row?;
            let route: QuestionRouting = parse(&route)?;
            let q: event_bus::UserQuestion = parse(&q)?;
            let actions = if matches!(&route.responder,QuestionResponder::Lead{thread_id} if Some(thread_id)==l.lead_thread_id.as_ref())
            {
                vec![
                    LaneAttentionAction::Answer,
                    LaneAttentionAction::RouteToUser,
                ]
            } else {
                vec![LaneAttentionAction::Inspect]
            };
            result.push(item(
                &l,
                &("question", &q.id),
                route.routing_revision,
                l.created_at_ns,
                LaneAttentionKind::QuestionPending {
                    question_id: q.id.clone(),
                    responder: route.responder,
                    routing_revision: route.routing_revision,
                },
                &q.title,
                actions,
            )?);
        }
        if let Some(report) =
            crate::repo::lanes::reports(conn, &lane_id, l.latest_report_seq.saturating_sub(1), 1)?
                .into_iter()
                .next()
                .filter(|r| {
                    r.management_revision == l.management_revision
                        && r.brief_revision == l.brief_revision
                })
        {
            let mut readiness = None;
            // A lane a person or its lead has closed needs nothing more from anyone.
            let closed: bool = conn.query_row(
                "SELECT current_close_id IS NOT NULL FROM lanes WHERE lane_id=?1",
                [&lane_id],
                |r| r.get(0),
            )?;
            let kind = match report.status {
                _ if closed => None,
                LaneStatus::Blocked => Some(LaneAttentionKind::Blocked {
                    report_seq: report.report_seq,
                }),
                LaneStatus::Failed => Some(LaneAttentionKind::Failed {
                    report_seq: report.report_seq,
                    episode_id: report.failure_episode.clone(),
                }),
                LaneStatus::Ready => {
                    let state = ready_readiness(conn, &lane_id, &report)?;
                    readiness = Some(state);
                    Some(LaneAttentionKind::ReadyUnclosed {
                        ready_seq: report.report_seq,
                        readiness: state,
                    })
                }
                _ => None,
            };
            if let Some(kind) = kind {
                let actions = if report.status == LaneStatus::Ready {
                    // Close is always the lead's choice; a review is offered unless one
                    // is already running or has approved.
                    let mut actions = vec![LaneAttentionAction::Close];
                    if !matches!(
                        readiness,
                        Some(LaneReadyState::ReviewInProgress | LaneReadyState::AwaitingClose)
                    ) {
                        actions.push(LaneAttentionAction::Verify);
                    }
                    actions.push(LaneAttentionAction::Inspect);
                    actions
                } else {
                    vec![LaneAttentionAction::Inspect, LaneAttentionAction::Continue]
                };
                let mut entry = item(
                    &l,
                    &("report", report.report_seq),
                    report.report_seq,
                    report.created_at_ns,
                    kind,
                    &report.summary,
                    actions,
                )?;
                entry.evidence_refs = report
                    .artifact_refs
                    .into_iter()
                    .take(4)
                    .map(|s| preview(&s))
                    .collect();
                result.push(entry);
            }
        }
        if let Some(d) = crate::repo::lane_delivery::capacity_diagnostic(conn, &lane_id)? {
            let limit = d.capacity.limit();
            let count = match d.capacity {
                crate::lane_delivery::QuestionCapacity::Requester => {
                    crate::repo::user_questions::requester_question_count(
                        conn,
                        &d.requester_run_id,
                    )?
                }
            };
            if count >= u64::from(limit) {
                result.push(item(
                    &l,
                    &"capacity",
                    d.attempts,
                    d.first_at_ns,
                    LaneAttentionKind::QuestionCapacityBlocked {
                        capacity: match d.capacity {
                            crate::lane_delivery::QuestionCapacity::Requester => {
                                LaneQuestionCapacity::Requester
                            }
                        },
                        limit,
                    },
                    "question_capacity_blocked",
                    vec![LaneAttentionAction::Inspect],
                )?);
            }
        }
        let guard = crate::repo::lane_delivery::guard(conn, &lane_id)?;
        if guard.suppressed {
            result.push(item(
                &l,
                &"wake_guard",
                guard.revision,
                l.updated_at_ns,
                LaneAttentionKind::WakeSuppressed {
                    guard_revision: guard.revision,
                },
                "wake_suppressed",
                vec![LaneAttentionAction::Resume],
            )?);
        }
        // A suppressed accepted-input recovery can outlive its last outbox row.
        // Union durable episodes with pending destinations; NACK is only a legacy
        // nudge hint and claiming another batch must not hide the episode.
        let mut s=conn.prepare("SELECT d.thread_id,(SELECT COUNT(*) FROM lane_notifications n WHERE n.lane_id=?1 AND n.management_revision=?2 AND n.destination_thread_id=d.thread_id AND n.state='pending'),d.stopped,d.last_nack FROM lane_delivery_destinations d WHERE d.thread_id IN (SELECT destination_thread_id FROM lane_notifications WHERE lane_id=?1 AND management_revision=?2 AND state='pending' UNION SELECT destination_thread_id FROM lane_recovery_episodes WHERE state<>'resolved' AND attempts>0 AND destination_thread_id IN (?3,?4)) AND d.thread_id NOT IN (SELECT thread_id FROM thread_tombstones) ORDER BY d.thread_id")?;
        for row in s.query_map(
            params![
                lane_id,
                l.management_revision,
                l.thread_id,
                l.lead_thread_id
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )? {
            let (destination, count, stopped, nack) = row?;
            if stopped {
                result.push(item(
                    &l,
                    &("stop", &destination),
                    count,
                    l.updated_at_ns,
                    LaneAttentionKind::StopPending {
                        destination_thread_id: destination.clone(),
                        pending_count: count,
                    },
                    "stop_pending",
                    vec![LaneAttentionAction::Resume],
                )?);
            }
            let recovery = crate::repo::lane_delivery::active_recovery(conn, &destination)?;
            if recovery.as_ref().is_some_and(|e| e.attempts > 0)
                || nack.as_deref() == Some("\"restore_failed\"")
            {
                let suppressed = recovery.as_ref().is_some_and(|e| {
                    e.state == crate::lane_delivery::LaneRecoveryState::Suppressed
                });
                result.push(item(
                    &l,
                    &(
                        "restore",
                        &destination,
                        recovery.as_ref().map(|e| &e.episode_id),
                    ),
                    recovery.as_ref().map_or(count, |e| e.revision),
                    recovery
                        .as_ref()
                        .and_then(|e| e.last_attempt_at_ns)
                        .unwrap_or(l.updated_at_ns),
                    LaneAttentionKind::RestoreFailed {
                        destination_thread_id: destination.clone(),
                        failure_ref: recovery.as_ref().map_or_else(
                            || format!("lane_delivery:{destination}"),
                            |e| e.episode_id.clone(),
                        ),
                        attempts: recovery.as_ref().map_or(0, |e| e.attempts),
                        suppressed,
                    },
                    if suppressed {
                        "restore_retry_suppressed"
                    } else {
                        "restore_failed"
                    },
                    vec![
                        LaneAttentionAction::Inspect,
                        if suppressed {
                            LaneAttentionAction::Resume
                        } else {
                            LaneAttentionAction::Continue
                        },
                    ],
                )?);
            }
        }
        let mut s=conn.prepare("SELECT payload FROM lane_stall_episodes WHERE lane_id=?1 AND management_revision=?2 AND progress_epoch=(SELECT progress_epoch FROM lanes WHERE lane_id=?1) AND json_extract(payload,'$.resolved_at_ns') IS NULL ORDER BY episode_id")?;
        for row in s.query_map(params![lane_id, l.management_revision], |r| {
            r.get::<_, String>(0)
        })? {
            let e: LaneStallEpisode = parse(&row?)?;
            result.push(item(
                &l,
                &("stall", &e.episode_id),
                e.revision,
                e.first_detected_at_ns,
                LaneAttentionKind::Stall {
                    episode_id: e.episode_id.clone(),
                    classification: e.classification,
                    nudge_state: e.nudge_state,
                },
                "stall",
                vec![LaneAttentionAction::Inspect, LaneAttentionAction::Continue],
            )?);
        }
    }
    result.sort_by(|a, b| b.since_at_ns.cmp(&a.since_at_ns).then(a.id.cmp(&b.id)));
    Ok(result)
}
pub(crate) fn attention(
    conn: &Connection,
    filter: LaneAttentionFilter,
    cursor: Option<String>,
    limit: usize,
) -> Result<LaneAttentionPage, StorageError> {
    id(&filter.lead_thread_id)?;
    if let Some(project) = &filter.project_id {
        id(project)?;
    }
    // A read transaction fixes every source query to the same SQLite snapshot.
    let tx = conn.unchecked_transaction()?;
    let all = items(&tx, &filter)?;
    let filter_hash = digest(&filter)?;
    let mut source=tx.prepare("SELECT l.lane_id,l.management_revision,l.brief_revision,l.request_revision,l.progress_epoch FROM lanes l JOIN threads t ON t.thread_id=l.thread_id WHERE l.lead_thread_id=?1 AND l.policy<>'detached' AND (?2 IS NULL OR t.project_id=?2) ORDER BY l.lane_id")?;
    let versions = source
        .query_map(params![filter.lead_thread_id, filter.project_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, u64>(2)?,
                r.get::<_, u64>(3)?,
                r.get::<_, u64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let revision = digest(&(&all, versions))?;
    let mut position = 0;
    if let Some(token) = cursor {
        if token.len() > 512 {
            return Err(err(LaneOperationError::StaleCursor));
        }
        let c: Cursor = parse(&token).map_err(|_| err(LaneOperationError::StaleCursor))?;
        if c.filter != filter_hash || c.snapshot != revision || c.position > all.len() {
            return Err(err(LaneOperationError::StaleCursor));
        }
        position = c.position;
    }
    let mut page = LaneAttentionPage {
        items: vec![],
        next_cursor: None,
        snapshot_revision: revision.clone(),
    };
    while position < all.len() && page.items.len() < limit.clamp(1, LANE_ATTENTION_MAX) {
        page.items.push(all[position].clone());
        page.next_cursor = (position + 1 < all.len())
            .then(|| {
                json(&Cursor {
                    filter: filter_hash.clone(),
                    snapshot: revision.clone(),
                    position: position + 1,
                })
            })
            .transpose()?;
        if json(&page)?.len() > LANE_ATTENTION_BYTES {
            page.items.pop();
            break;
        }
        position += 1;
    }
    if position < all.len() && page.items.is_empty() {
        return Err(err(LaneOperationError::InvalidInput));
    }
    page.next_cursor = (position < all.len())
        .then(|| {
            json(&Cursor {
                filter: filter_hash,
                snapshot: revision,
                position,
            })
        })
        .transpose()?;
    Ok(page)
}
