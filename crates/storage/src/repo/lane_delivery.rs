//! Transactional lane source writes. No runtime scheduling policy lives here.
use crate::{StorageError, lane_delivery::*};
use event_bus::{AnswerOrigin, UserQuestion};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

mod delivery;
mod guards;
pub(crate) use delivery::*;
pub(crate) use guards::*;

fn transaction(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
    Ok(Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    )?)
}
fn json(value: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|e| StorageError::Serialization(e.to_string()))
}
fn parse<T: DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    serde_json::from_str(value).map_err(|e| StorageError::Serialization(e.to_string()))
}
fn conflict(reason: &'static str) -> StorageError {
    StorageError::LaneDeliveryConflict(reason)
}
fn id(value: &str) -> Result<(), StorageError> {
    if value.is_empty() || value.len() > 256 {
        return Err(conflict("invalid_id"));
    }
    Ok(())
}
fn live_thread(conn: &Connection, thread: &str) -> Result<(), StorageError> {
    id(thread)?;
    let deleted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id=?1)",
        [thread],
        |r| r.get(0),
    )?;
    if deleted {
        return Err(StorageError::Tombstoned {
            thread_id: Some(thread.into()),
            run_id: None,
        });
    }
    Ok(())
}
fn lane(conn: &Connection, lane_id: &str) -> Result<crate::lanes::LaneRecord, StorageError> {
    crate::repo::lanes::get(conn, lane_id)?.ok_or_else(|| StorageError::LaneNotFound {
        lane_id: lane_id.into(),
    })
}
fn management(current: u64, expected: u64) -> Result<(), StorageError> {
    if current != expected {
        return Err(StorageError::StaleRevision {
            kind: "management",
            expected,
            current,
        });
    }
    Ok(())
}

/// Semantic identity is independent of report sequence, summary and delivery batch.
fn semantic(kind: &LaneNotificationKind) -> Result<(&'static str, String), StorageError> {
    Ok(match kind {
        LaneNotificationKind::Question {
            question_id,
            routing_revision,
        } => ("question", json(&(question_id, routing_revision))?),
        LaneNotificationKind::Answer {
            question_id,
            routing_revision,
        } => ("answer", json(&(question_id, routing_revision))?),
        LaneNotificationKind::Ready { revision } => (
            "ready",
            revision
                .as_ref()
                .and_then(ArtifactRevision::fingerprint)
                .unwrap_or_else(|| "unknown".into()),
        ),
        LaneNotificationKind::Failed { episode, revision } => {
            id(episode)?;
            (
                "failed",
                json(&(
                    episode,
                    revision.as_ref().and_then(ArtifactRevision::fingerprint),
                ))?,
            )
        }
        LaneNotificationKind::Milestone => ("milestone", "pending".into()),
        LaneNotificationKind::Intervention { input_id } => {
            id(input_id)?;
            ("intervention", input_id.clone())
        }
        _ => return Err(conflict("unsupported_notification")),
    })
}

/// The semantic key and outbox row are one write. Returns the original stable ID
/// on a retry, even when that notification has already been ACKed.
fn enqueue(conn: &Connection, mut notice: LaneNotification) -> Result<String, StorageError> {
    live_thread(conn, &notice.source_thread_id)?;
    live_thread(conn, &notice.destination_thread_id)?;
    let current = lane(conn, &notice.lane_id)?;
    management(current.management_revision, notice.management_revision)?;
    let (event_kind, mut key) = semantic(&notice.kind)?;
    if matches!(notice.kind, LaneNotificationKind::Milestone) {
        // Coalesce only work never claimed: an in-flight/received notice is an
        // immutable snapshot reference and must not be rewritten under a claimant.
        let unsent: Option<String> = conn.query_row(
            "SELECT notification_id FROM lane_notifications WHERE lane_id=?1 AND management_revision=?2 AND destination_thread_id=?3 AND event_kind='milestone' AND state='pending' AND claim_epoch IS NULL LIMIT 1",
            params![notice.lane_id,notice.management_revision,notice.destination_thread_id],|r|r.get(0)).optional()?;
        if let Some(original) = unsent {
            notice.notification_id = original.clone();
            conn.execute(
                "UPDATE lane_notifications SET payload=?2 WHERE notification_id=?1",
                params![original, json(&notice)?],
            )?;
            return Ok(original);
        }
        // Sequence is only a grouping boundary for non-important milestones;
        // it never resets progress or creates a new ready/failed semantic event.
        key = format!("milestone:{}", notice.report_seq.unwrap_or(0));
    }
    if let Some(revision) = match &notice.kind {
        LaneNotificationKind::Ready { revision }
        | LaneNotificationKind::Failed { revision, .. } => revision.as_ref(),
        _ => None,
    } && revision.fingerprint().is_none()
    {
        return Err(conflict("invalid_artifact_revision"));
    }
    let original: Option<String> = conn.query_row(
        "SELECT notification_id FROM lane_notifications WHERE lane_id=?1 AND management_revision=?2 AND event_kind=?3 AND semantic_key=?4",
        params![notice.lane_id, notice.management_revision, event_kind, key], |r| r.get(0)).optional()?;
    if let Some(original) = original {
        return Ok(original);
    }
    notice.notification_id = format!(
        "ln-{:x}",
        Sha256::digest(
            json(&(
                &notice.lane_id,
                notice.management_revision,
                event_kind,
                &key
            ))?
            .as_bytes()
        )
    );
    let payload = json(&notice)?;
    if payload.len() > 4096 {
        return Err(conflict("notification_too_large"));
    }
    crate::entity::SecretGuard::from_env().check_text("lane_notification", "payload", &payload)?;
    conn.execute("INSERT OR IGNORE INTO lane_delivery_keys(lane_id,management_revision,event_kind,dedupe_key,ledger_revision,created_at_ns) VALUES (?1,?2,?3,?4,0,?5)", params![notice.lane_id, notice.management_revision, event_kind, key, notice.created_at_ns])?;
    conn.execute("INSERT INTO lane_notifications(notification_id,lane_id,management_revision,event_kind,semantic_key,source_thread_id,destination_thread_id,payload) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)", params![notice.notification_id,notice.lane_id,notice.management_revision,event_kind,key,notice.source_thread_id,notice.destination_thread_id,payload])?;
    Ok(notice.notification_id)
}

pub(crate) fn routing(
    conn: &Connection,
    question_id: &str,
) -> Result<Option<QuestionRouting>, StorageError> {
    let payload: Option<String> = conn.query_row("SELECT r.payload FROM lane_question_routes r WHERE question_id=?1 AND requester_run_id NOT IN (SELECT run_id FROM run_tombstones) AND requester_root_run_id NOT IN (SELECT run_id FROM run_tombstones) AND requester_thread_id NOT IN (SELECT thread_id FROM thread_tombstones) AND (responder_thread_id IS NULL OR responder_thread_id NOT IN (SELECT thread_id FROM thread_tombstones)) AND EXISTS(SELECT 1 FROM lanes l WHERE l.lane_id=r.lane_id AND l.thread_id NOT IN (SELECT thread_id FROM thread_tombstones))", [question_id], |r| r.get(0)).optional()?;
    payload.as_deref().map(parse).transpose()
}
fn project(question: &mut UserQuestion, route: &QuestionRouting) {
    question.responder = Some(route.responder.clone());
    question.routing_revision = route.routing_revision;
    question.management_revision = Some(route.management_revision);
}
fn save_projection(conn: &Connection, question: &UserQuestion) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE user_questions SET payload=?2 WHERE id=?1",
        params![question.id, json(question)?],
    )?;
    Ok(())
}
fn responder_thread(responder: &QuestionResponder) -> Result<Option<&str>, StorageError> {
    match responder {
        QuestionResponder::User => Ok(None),
        QuestionResponder::Lead { thread_id } => Ok(Some(thread_id)),
        _ => Err(conflict("unsupported_responder")),
    }
}

pub(crate) fn create_question(
    conn: &Connection,
    lane_id: &str,
    mut question: UserQuestion,
    requester: QuestionRequester,
    binding: LaneDeliveryBinding,
    at_ns: i64,
) -> Result<LaneQuestionCreated, StorageError> {
    let tx = transaction(conn)?;
    let current = lane(&tx, lane_id)?;
    if requester.run_id != question.run_id
        || requester.root_run_id != question.root_run_id
        || requester.thread_id != current.thread_id
    {
        return Err(conflict("requester_mismatch"));
    }
    if let Some(old) = crate::repo::user_questions::get(&tx, &question.id)? {
        let route = routing(&tx, &question.id)?.ok_or_else(|| conflict("question_id_reused"))?;
        if route.lane_id != lane_id
            || route.requester != requester
            || old.title != question.title
            || old.options != question.options
            || old.blocking != question.blocking
        {
            return Err(conflict("question_id_reused"));
        }
        return Ok(LaneQuestionCreated::Created(Box::new(old)));
    }
    question.responder = None;
    question.routing_revision = 0;
    question.management_revision = None;
    match crate::repo::user_questions::create(&tx, &question) {
        Ok(()) => (),
        Err(StorageError::QuestionCapacity(capacity)) => {
            let previous: Option<String> = tx.query_row(
                "SELECT question_capacity_blocked_json FROM lanes WHERE lane_id=?1",
                [lane_id],
                |r| r.get(0),
            )?;
            let mut diagnostic = previous
                .as_deref()
                .map(parse::<QuestionCapacityBlocked>)
                .transpose()?
                .unwrap_or(QuestionCapacityBlocked {
                    capacity,
                    attempts: 0,
                    first_at_ns: at_ns,
                    last_at_ns: at_ns,
                });
            diagnostic.capacity = capacity;
            diagnostic.attempts = diagnostic.attempts.saturating_add(1);
            diagnostic.last_at_ns = at_ns;
            tx.execute(
                "UPDATE lanes SET question_capacity_blocked_json=?2 WHERE lane_id=?1",
                params![lane_id, json(&diagnostic)?],
            )?;
            tx.commit()?;
            return Ok(LaneQuestionCreated::CapacityBlocked(diagnostic));
        }
        Err(error) => return Err(error),
    }
    let route = QuestionRouting {
        question_id: question.id.clone(),
        lane_id: lane_id.into(),
        requester,
        responder: binding.responder(current.policy, current.lead_thread_id.as_deref()),
        management_revision: current.management_revision,
        routing_revision: 1,
    };
    tx.execute("INSERT INTO lane_question_routes(question_id,lane_id,requester_run_id,requester_root_run_id,requester_thread_id,responder_thread_id,management_revision,routing_revision,payload) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![route.question_id,lane_id,route.requester.run_id,route.requester.root_run_id,route.requester.thread_id,responder_thread(&route.responder)?,route.management_revision,route.routing_revision,json(&route)?])?;
    project(&mut question, &route);
    // Raw UTF-8 limits do not bound JSON escaping. A successfully created
    // question must fit the first slot of a lane_questions page; rejecting here
    // rolls back the question and route before an outbox notice can be queued.
    let lead_projection = UserQuestion {
        recipient_run_ids: Vec::new(),
        ..question.clone()
    };
    if json(&(Vec::<UserQuestion>::new(), &lead_projection))?.len()
        > LANE_DELIVERY_PAGE_BYTES - 1024
    {
        return Err(conflict("question_exceeds_page_bytes"));
    }
    save_projection(&tx, &question)?;
    if let QuestionResponder::Lead { thread_id } = &route.responder {
        enqueue(
            &tx,
            LaneNotification {
                notification_id: String::new(),
                lane_id: lane_id.into(),
                management_revision: route.management_revision,
                source_thread_id: current.thread_id,
                destination_thread_id: thread_id.clone(),
                kind: LaneNotificationKind::Question {
                    question_id: question.id.clone(),
                    routing_revision: 1,
                },
                report_seq: None,
                created_at_ns: at_ns,
            },
        )?;
    }
    tx.commit()?;
    Ok(LaneQuestionCreated::Created(Box::new(question)))
}

pub(crate) fn mutate_question(
    conn: &Connection,
    request: LaneQuestionMutation,
) -> Result<UserQuestion, StorageError> {
    id(&request.operation_id)?;
    let tx = transaction(conn)?;
    let current = lane(&tx, &request.lane_id)?;
    management(
        current.management_revision,
        request.expected_management_revision,
    )?;
    if current.lead_thread_id.as_deref() != Some(&request.lead_thread_id) {
        return Err(conflict("not_current_lead"));
    }
    let mut route =
        routing(&tx, &request.question_id)?.ok_or_else(|| conflict("missing_routing"))?;
    if route.lane_id != request.lane_id {
        return Err(conflict("wrong_lane"));
    }
    management(route.management_revision, current.management_revision)?;
    let operation = json(&(
        &request.lead_thread_id,
        request.expected_management_revision,
        request.expected_routing_revision,
        &request.action,
    ))?;
    let (old_id, old_operation): (Option<String>, Option<String>) = tx.query_row(
        "SELECT operation_id,operation_json FROM lane_question_routes WHERE question_id=?1",
        [&request.question_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut question = crate::repo::user_questions::get(&tx, &request.question_id)?
        .ok_or_else(|| conflict("unknown_question"))?;
    if old_id.as_deref() == Some(&request.operation_id) {
        if old_operation.as_deref() != Some(&operation) {
            return Err(conflict("operation_id_reused"));
        }
        return Ok(question);
    }
    // An answer advances routing_revision too, but leaves the lead responder
    // unchanged. Classify a competing answer from the same listing as answered,
    // not as a route change. A User responder still takes the stale-routing path.
    if question.answer.is_some()
        && responder_thread(&route.responder)? == Some(&request.lead_thread_id)
    {
        return Err(StorageError::QuestionAlreadyAnswered);
    }
    if route.routing_revision != request.expected_routing_revision {
        return Err(StorageError::StaleRevision {
            kind: "routing",
            expected: request.expected_routing_revision,
            current: route.routing_revision,
        });
    }
    if responder_thread(&route.responder)? != Some(&request.lead_thread_id) {
        return Err(conflict("not_current_responder"));
    }
    if question.answer.is_some() {
        return Err(StorageError::QuestionAlreadyAnswered);
    }
    tx.execute("UPDATE lane_notifications SET state='obsolete' WHERE lane_id=?1 AND event_kind='question' AND json_extract(payload,'$.kind.question_id')=?2 AND state='pending'", params![request.lane_id,request.question_id])?;
    route.routing_revision = route
        .routing_revision
        .checked_add(1)
        .ok_or(StorageError::OutOfRange("routing revision"))?;
    match &request.action {
        LaneQuestionAction::RouteToUser => route.responder = QuestionResponder::User,
        LaneQuestionAction::Answer { text, origin } => {
            match origin {
                AnswerOrigin::LaneLead { run_id, thread_id }
                    if thread_id == &request.lead_thread_id =>
                {
                    id(run_id)?
                }
                _ => return Err(conflict("invalid_lane_answer_origin")),
            }
            crate::repo::user_questions::answer(
                &tx,
                &question.id,
                text,
                origin,
                crate::repo::user_questions::AnswerMode::FirstOnly,
            )?;
            question = crate::repo::user_questions::get(&tx, &question.id)?
                .ok_or_else(|| conflict("unknown_question"))?;
            enqueue(
                &tx,
                LaneNotification {
                    notification_id: String::new(),
                    lane_id: request.lane_id.clone(),
                    management_revision: route.management_revision,
                    source_thread_id: request.lead_thread_id.clone(),
                    destination_thread_id: route.requester.thread_id.clone(),
                    kind: LaneNotificationKind::Answer {
                        question_id: question.id.clone(),
                        routing_revision: route.routing_revision,
                    },
                    report_seq: None,
                    created_at_ns: request.at_ns,
                },
            )?;
        }
    }
    project(&mut question, &route);
    tx.execute("UPDATE lane_question_routes SET responder_thread_id=?2,routing_revision=?3,payload=?4,operation_id=?5,operation_json=?6 WHERE question_id=?1 AND routing_revision=?7", params![route.question_id,responder_thread(&route.responder)?,route.routing_revision,json(&route)?,request.operation_id,operation,request.expected_routing_revision])?;
    save_projection(&tx, &question)?;
    tx.commit()?;
    Ok(question)
}

/// Snapshot-cutoff keyset pagination; every page checks current management and
/// rejects a cursor if its lead or management fingerprint changed.
pub(crate) fn questions(
    conn: &Connection,
    lead: &str,
    cursor: Option<LaneQuestionCursor>,
) -> Result<LaneQuestionPage, StorageError> {
    live_thread(conn, lead)?;
    let mut relations=conn.prepare("SELECT lane_id,management_revision FROM lanes WHERE lead_thread_id=?1 AND thread_id NOT IN (SELECT thread_id FROM thread_tombstones) ORDER BY lane_id")?;
    let relations = relations
        .query_map([lead], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let fingerprint = format!("{:x}", Sha256::digest(json(&relations)?.as_bytes()));
    let cursor = match cursor {
        Some(cursor)
            if cursor.lead_thread_id == lead && cursor.management_fingerprint == fingerprint =>
        {
            cursor
        }
        Some(_) => return Err(conflict("stale_cursor")),
        None => LaneQuestionCursor {
            lead_thread_id: lead.into(),
            management_fingerprint: fingerprint,
            snapshot_max_rowid: conn.query_row(
                "SELECT COALESCE(MAX(rowid),0) FROM lane_question_routes",
                [],
                |r| r.get(0),
            )?,
            after_question_id: String::new(),
        },
    };
    let mut statement=conn.prepare("SELECT r.question_id FROM lane_question_routes r JOIN lanes l ON l.lane_id=r.lane_id JOIN user_questions q ON q.id=r.question_id WHERE r.responder_thread_id=?1 AND l.lead_thread_id=?1 AND r.management_revision=l.management_revision AND l.thread_id NOT IN (SELECT thread_id FROM thread_tombstones) AND q.answered=0 AND r.question_id>?2 AND r.rowid<=?3 ORDER BY r.question_id LIMIT 21")?;
    let ids = statement
        .query_map(
            params![lead, cursor.after_question_id, cursor.snapshot_max_rowid],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut page = LaneQuestionPage {
        questions: Vec::new(),
        next_cursor: None,
        has_more: false,
    };
    for question_id in ids {
        let Some(mut question) = crate::repo::user_questions::get(conn, &question_id)? else {
            continue;
        };
        // Continuation links are not lead-inbox routing. Their growth across
        // restores must not change this bounded, responder-only projection.
        question.recipient_run_ids.clear();
        let bytes = json(&(&page.questions, &question))?.len();
        if page.questions.is_empty() && bytes > LANE_DELIVERY_PAGE_BYTES - 1024 {
            // Escaping may expand otherwise-valid UTF-8 source fields. Never
            // return an empty has_more page without a usable continuation.
            return Err(conflict("question_exceeds_page_bytes"));
        }
        if page.questions.len() >= LANE_DELIVERY_PAGE_MAX || bytes > LANE_DELIVERY_PAGE_BYTES - 1024
        {
            page.has_more = true;
            break;
        }
        page.questions.push(question);
    }
    if page.has_more {
        page.next_cursor = page.questions.last().map(|q| LaneQuestionCursor {
            after_question_id: q.id.clone(),
            ..cursor
        });
    }
    Ok(page)
}

pub(crate) fn append_report(
    conn: &Connection,
    lane_id: &str,
    input: crate::lanes::LaneReportInput,
    at_ns: i64,
) -> Result<crate::lanes::LaneReportAppended, StorageError> {
    let tx = transaction(conn)?;
    let appended = crate::repo::lanes::append_report_in_transaction(&tx, lane_id, &input, at_ns)?;
    let current = &appended.lane;
    if let Some(destination) = &current.lead_thread_id {
        let kind = match input.status {
            event_bus::LaneStatus::Ready => Some(LaneNotificationKind::Ready {
                revision: input.artifact_revision.clone(),
            }),
            event_bus::LaneStatus::Failed => Some(LaneNotificationKind::Failed {
                episode: input
                    .failure_episode
                    .clone()
                    .ok_or_else(|| conflict("missing_failure_episode"))?,
                revision: input.artifact_revision.clone(),
            }),
            _ if current.policy == event_bus::LanePolicy::Attached => {
                Some(LaneNotificationKind::Milestone)
            }
            _ => None,
        };
        if let Some(kind) = kind {
            enqueue(
                &tx,
                LaneNotification {
                    notification_id: String::new(),
                    lane_id: lane_id.into(),
                    management_revision: current.management_revision,
                    source_thread_id: current.thread_id.clone(),
                    destination_thread_id: destination.clone(),
                    kind,
                    report_seq: Some(appended.report.report_seq),
                    created_at_ns: at_ns,
                },
            )?;
        }
    }
    tx.commit()?;
    Ok(appended)
}

/// Receipt of actual lane input + reference-only intervention notification.
pub(crate) fn accept_input(
    conn: &Connection,
    lane_id: &str,
    receipt: crate::HostInputReceipt,
) -> Result<(), StorageError> {
    if receipt.provenance != crate::HostInputProvenance::User {
        return Err(conflict("not_user_input"));
    }
    let tx = transaction(conn)?;
    let current = lane(&tx, lane_id)?;
    if current.root_run_id.as_deref() != Some(&receipt.root_run_id) {
        return Err(conflict("input_lane_mismatch"));
    }
    let existing =
        crate::repo::host_input_receipt::for_ids(&tx, std::slice::from_ref(&receipt.prompt_id))?;
    if let Some(previous) = existing.first() {
        if previous.run_id != receipt.run_id
            || previous.text != receipt.text
            || previous.provenance != receipt.provenance
        {
            return Err(conflict("input_id_reused"));
        }
    } else {
        crate::repo::host_input_receipt::insert(&tx, &receipt)?;
    }
    if let Some(destination) = current.lead_thread_id {
        enqueue(
            &tx,
            LaneNotification {
                notification_id: String::new(),
                lane_id: lane_id.into(),
                management_revision: current.management_revision,
                source_thread_id: current.thread_id,
                destination_thread_id: destination,
                kind: LaneNotificationKind::Intervention {
                    input_id: receipt.prompt_id,
                },
                report_seq: None,
                created_at_ns: receipt.accepted_at_ns,
            },
        )?;
    }
    tx.commit()?;
    Ok(())
}
