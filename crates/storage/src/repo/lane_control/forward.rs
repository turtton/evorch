use super::*;
use event_bus::UserQuestion;
pub(crate) fn forward(
    conn: &Connection,
    r: ForwardLaneQuestion,
) -> Result<QuestionRouting, StorageError> {
    id(&r.caller_root)?;
    id(&r.question_id)?;
    id(&r.operation_id)?;
    let tx = tx(conn)?;
    let current = crate::repo::lanes::get_by_thread(
        &tx,
        &tx.query_row(
            "SELECT thread_id FROM lanes WHERE root_run_id=?1",
            [&r.caller_root],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| err(LaneOperationError::OwnershipLost))?,
    )?
    .ok_or_else(|| err(LaneOperationError::OwnershipLost))?;
    let l = managed(&tx, &current.lane_id, r.expected_management_revision, None)?;
    if l.policy != LanePolicy::Attached {
        return Err(err(LaneOperationError::NotAttached));
    }
    if l.root_run_id.as_deref() != Some(&r.caller_root)
        || l.generation != r.expected_lane_generation
    {
        return Err(err(LaneOperationError::OwnershipLost));
    }
    let request = json(&(
        &r.question_id,
        r.expected_lane_generation,
        r.expected_management_revision,
    ))?;
    let old:Option<(String,String)>=tx.query_row("SELECT request_json,result_json FROM lane_forward_receipts WHERE caller_root=?1 AND operation_id=?2",params![r.caller_root,r.operation_id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    if let Some((previous, result)) = old {
        if previous != request {
            return Err(err(LaneOperationError::OperationReused));
        }
        // Returning a historical receipt does not restore an old Lead route.
        return parse(&result);
    }
    let mut q = crate::repo::user_questions::get(&tx, &r.question_id)?
        .ok_or_else(|| err(LaneOperationError::InvalidInput))?;
    if q.answer.is_some() {
        return Err(err(LaneOperationError::AlreadyAnswered));
    }
    let owned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM run_contexts WHERE run_id=?1 AND parent_run_id=?2 AND run_id NOT IN (SELECT run_id FROM run_tombstones))",params![q.run_id,r.caller_root],|row|row.get(0))?;
    if !owned || q.root_run_id != r.caller_root {
        return Err(err(LaneOperationError::OwnershipLost));
    }
    if crate::repo::lane_delivery::routing(&tx, &q.id)?.is_some() {
        return Err(err(LaneOperationError::StaleRouting));
    }
    let lead = l
        .lead_thread_id
        .clone()
        .ok_or_else(|| err(LaneOperationError::NotAttached))?;
    let route = QuestionRouting {
        question_id: q.id.clone(),
        lane_id: l.lane_id.clone(),
        requester: QuestionRequester {
            run_id: q.run_id.clone(),
            root_run_id: q.root_run_id.clone(),
            thread_id: l.thread_id.clone(),
        },
        responder: QuestionResponder::Lead {
            thread_id: lead.clone(),
        },
        management_revision: l.management_revision,
        routing_revision: 1,
    };
    q.responder = Some(route.responder.clone());
    q.routing_revision = 1;
    q.management_revision = Some(l.management_revision);
    // JSON escaping must not make the question impossible to list.
    let projection = UserQuestion {
        recipient_run_ids: vec![],
        ..q.clone()
    };
    if json(&projection)?.len() > crate::lane_delivery::LANE_DELIVERY_PAGE_BYTES - 1024 {
        return Err(err(LaneOperationError::InvalidInput));
    }
    tx.execute("INSERT INTO lane_question_routes(question_id,lane_id,requester_run_id,requester_root_run_id,requester_thread_id,responder_thread_id,management_revision,routing_revision,payload) VALUES (?1,?2,?3,?4,?5,?6,?7,1,?8)",params![q.id,l.lane_id,q.run_id,q.root_run_id,l.thread_id,lead,l.management_revision,bounded(&route,4096)?])?;
    tx.execute(
        "UPDATE user_questions SET payload=?2 WHERE id=?1 AND answered=0",
        params![q.id, json(&q)?],
    )?;
    crate::repo::lane_delivery::enqueue(
        &tx,
        LaneNotification {
            notification_id: String::new(),
            lane_id: l.lane_id.clone(),
            management_revision: l.management_revision,
            source_thread_id: l.thread_id,
            destination_thread_id: lead,
            kind: LaneNotificationKind::Question {
                question_id: q.id.clone(),
                routing_revision: 1,
            },
            report_seq: None,
            created_at_ns: r.at_ns,
        },
    )?;
    tx.execute("INSERT INTO lane_forward_receipts(caller_root,operation_id,lane_id,question_id,request_json,result_json) VALUES (?1,?2,?3,?4,?5,?6)",params![r.caller_root,r.operation_id,l.lane_id,q.id,request,json(&route)?])?;
    tx.commit()?;
    Ok(route)
}
