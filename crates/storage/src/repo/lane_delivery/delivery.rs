//! Destination single flight, lease fencing and durable receipt/ACK repair.
use super::*;
const LIVE_NOTICE: &str = "n.source_thread_id NOT IN (SELECT thread_id FROM thread_tombstones) AND n.destination_thread_id NOT IN (SELECT thread_id FROM thread_tombstones) AND EXISTS(SELECT 1 FROM lanes l WHERE l.lane_id=n.lane_id AND l.management_revision=n.management_revision AND l.thread_id NOT IN (SELECT thread_id FROM thread_tombstones))";
pub(crate) fn inbox(
    conn: &Connection,
    destination: &str,
    after: &str,
) -> Result<Vec<LaneNotification>, StorageError> {
    live_thread(conn, destination)?;
    let mut statement=conn.prepare(&format!("SELECT n.payload FROM lane_notifications n WHERE n.destination_thread_id=?1 AND n.notification_id>?2 AND n.state!='obsolete' AND {LIVE_NOTICE} ORDER BY n.notification_id LIMIT 20"))?;
    let values = statement
        .query_map(params![destination, after], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = Vec::new();
    for value in values {
        let notice: LaneNotification = parse(&value)?;
        if json(&(&result, &notice))?.len() > LANE_DELIVERY_PAGE_BYTES - 512 {
            break;
        }
        result.push(notice);
    }
    Ok(result)
}
/// Binding or Stop changes fence every previous claimant. Only the authenticated
/// host's explicit resume may clear Stop; caller holds its ownership mutation guard.
pub(crate) fn bind_destination(
    conn: &Connection,
    thread: &str,
    expected: u64,
    owner: LaneDeliveryOwner,
    action: LaneDestinationAction,
) -> Result<u64, StorageError> {
    live_thread(conn, thread)?;
    id(&owner.root_run_id)?;
    let tx = transaction(conn)?;
    tx.execute(
        "INSERT OR IGNORE INTO lane_delivery_destinations(thread_id) VALUES (?1)",
        [thread],
    )?;
    let changed=tx.execute("UPDATE lane_delivery_destinations SET epoch=epoch+1,owner_json=?3,stopped=CASE ?4 WHEN 'stop' THEN 1 WHEN 'resume' THEN 0 ELSE stopped END,lease_token=NULL,lease_until_ns=NULL WHERE thread_id=?1 AND epoch=?2",params![thread,expected,json(&owner)?,match action { LaneDestinationAction::Bind=>"bind",LaneDestinationAction::Stop=>"stop",LaneDestinationAction::ExplicitUserResume=>"resume" }])?;
    if changed != 1 {
        return Err(conflict("stale_destination_epoch"));
    }
    let epoch = expected
        .checked_add(1)
        .ok_or(StorageError::OutOfRange("destination epoch"))?;
    tx.commit()?;
    Ok(epoch)
}
pub(crate) fn claim(
    conn: &Connection,
    request: LaneClaimRequest,
) -> Result<LaneClaimResult, StorageError> {
    live_thread(conn, &request.destination_thread_id)?;
    id(&request.token)?;
    id(&request.owner.root_run_id)?;
    if request.lease_until_ns <= request.now_ns {
        return Err(conflict("invalid_lease"));
    }
    let tx = transaction(conn)?;
    tx.execute(
        "INSERT OR IGNORE INTO lane_delivery_destinations(thread_id) VALUES (?1)",
        [&request.destination_thread_id],
    )?;
    let (epoch,stopped,owner,claim_epoch,until):(u64,bool,Option<String>,u64,Option<i64>)=tx.query_row("SELECT epoch,stopped,owner_json,claim_epoch,lease_until_ns FROM lane_delivery_destinations WHERE thread_id=?1",[&request.destination_thread_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    if epoch != request.expected_destination_epoch {
        return Err(conflict("stale_destination_epoch"));
    }
    if stopped {
        return Ok(LaneClaimResult::Stopped);
    }
    if owner
        .as_deref()
        .map(parse::<LaneDeliveryOwner>)
        .transpose()?
        .is_some_and(|owner| owner != request.owner)
    {
        return Err(conflict("stale_owner"));
    }
    let deleted: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",
        [&request.owner.root_run_id],
        |r| r.get(0),
    )?;
    if deleted {
        return Err(StorageError::Tombstoned {
            run_id: Some(request.owner.root_run_id),
            thread_id: None,
        });
    }
    if until.is_some_and(|until| until > request.now_ns) {
        return Ok(LaneClaimResult::Busy);
    }
    let mut statement=tx.prepare(&format!("SELECT n.payload FROM lane_notifications n JOIN lanes l ON l.lane_id=n.lane_id WHERE n.destination_thread_id=?1 AND n.state='pending' AND {LIVE_NOTICE} AND (n.event_kind='answer' OR COALESCE(json_extract(l.wake_guard_json,'$.suppressed'),0)=0) AND (?2 OR n.event_kind IN ('question','ready','failed','answer')) ORDER BY n.notification_id LIMIT 20"))?;
    let values = statement
        .query_map(
            params![request.destination_thread_id, request.allow_non_waking],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut notifications = Vec::new();
    for value in values {
        let notice: LaneNotification = parse(&value)?;
        if json(&(&notifications, &notice))?.len() > LANE_DELIVERY_PAGE_BYTES - 1024 {
            break;
        }
        notifications.push(notice);
    }
    if notifications.is_empty() {
        return Ok(LaneClaimResult::Empty);
    }
    let claim_epoch = claim_epoch
        .checked_add(1)
        .ok_or(StorageError::OutOfRange("claim epoch"))?;
    tx.execute("UPDATE lane_delivery_destinations SET claim_epoch=?2,owner_json=?3,lease_token=?4,lease_until_ns=?5,last_nack=NULL WHERE thread_id=?1",params![request.destination_thread_id,claim_epoch,json(&request.owner)?,request.token,request.lease_until_ns])?;
    for notice in &notifications {
        tx.execute(
            "UPDATE lane_notifications SET claim_epoch=?2 WHERE notification_id=?1",
            params![notice.notification_id, claim_epoch],
        )?;
    }
    tx.commit()?;
    Ok(LaneClaimResult::Claimed(LaneDeliveryClaim {
        destination_thread_id: request.destination_thread_id,
        destination_epoch: epoch,
        claim_epoch,
        token: request.token,
        lease_until_ns: request.lease_until_ns,
        owner: request.owner,
        notifications,
    }))
}
fn check_claim(
    conn: &Connection,
    claim: &LaneDeliveryClaim,
    now_ns: i64,
) -> Result<(), StorageError> {
    live_thread(conn, &claim.destination_thread_id)?;
    let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM lane_delivery_destinations WHERE thread_id=?1 AND epoch=?2 AND claim_epoch=?3 AND lease_token=?4 AND lease_until_ns>?5 AND stopped=0 AND owner_json=?6)",params![claim.destination_thread_id,claim.destination_epoch,claim.claim_epoch,claim.token,now_ns,json(&claim.owner)?],|r|r.get(0))?;
    if !valid {
        return Err(conflict("stale_claim"));
    }
    if claim.notifications.is_empty() || claim.notifications.len() > LANE_DELIVERY_PAGE_MAX {
        return Err(conflict("invalid_claim_members"));
    }
    for notice in &claim.notifications {
        live_thread(conn, &notice.source_thread_id)?;
        management(
            lane(conn, &notice.lane_id)?.management_revision,
            notice.management_revision,
        )?;
        let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM lane_notifications WHERE notification_id=?1 AND claim_epoch=?2 AND destination_thread_id=?3 AND payload=?4 AND state='pending')",params![notice.notification_id,claim.claim_epoch,claim.destination_thread_id,json(notice)?],|r|r.get(0))?;
        if !valid {
            return Err(conflict("stale_notification"));
        }
    }
    Ok(())
}
pub(crate) fn nack(
    conn: &Connection,
    claim: &LaneDeliveryClaim,
    reason: LaneDeliveryNack,
    now_ns: i64,
) -> Result<(), StorageError> {
    let tx = transaction(conn)?;
    // NACK must release the lease even if one member became obsolete meanwhile.
    let changed=tx.execute("UPDATE lane_delivery_destinations SET lease_token=NULL,lease_until_ns=NULL,last_nack=?6 WHERE thread_id=?1 AND epoch=?2 AND claim_epoch=?3 AND lease_token=?4 AND lease_until_ns>?5",params![claim.destination_thread_id,claim.destination_epoch,claim.claim_epoch,claim.token,now_ns,json(&reason)?])?;
    if changed != 1 {
        return Err(conflict("stale_claim"));
    }
    tx.commit()?;
    Ok(())
}
fn receipt_id(notice: &LaneNotification) -> String {
    format!("lane-notification:{}", notice.notification_id)
}
fn received(conn: &Connection, notice: &LaneNotification) -> Result<bool, StorageError> {
    let receipts = crate::repo::host_input_receipt::for_ids(conn, &[receipt_id(notice)])?;
    Ok(receipts.iter().any(|receipt| {
        receipt.status == crate::HostInputStatus::Delivered
            && receipt.provenance
                == crate::HostInputProvenance::LaneNotification {
                    notification_id: notice.notification_id.clone(),
                    destination_thread_id: notice.destination_thread_id.clone(),
                }
    }))
}
/// Host snapshot contains only the IDs missing from receipt_ids(). Fully received
/// retries do not overwrite a snapshot. Notifications are never host_inputs_json.
pub(crate) fn accept_delivery(
    conn: &Connection,
    claim: &LaneDeliveryClaim,
    write: crate::thread_index::SnapshotWrite,
    now_ns: i64,
) -> Result<Option<crate::thread_index::SnapshotReceipt>, StorageError> {
    let tx = transaction(conn)?;
    check_claim(&tx, claim, now_ns)?;
    let record = &write.record;
    if record.run_id != claim.owner.root_run_id || record.parent_run_id.is_some() {
        return Err(conflict("snapshot_destination_mismatch"));
    }
    let mut missing = Vec::new();
    for notice in &claim.notifications {
        if !received(&tx, notice)? {
            missing.push(notice);
        }
    }
    if missing.is_empty() {
        return Ok(None);
    }
    let snapshot = crate::repo::thread_index::stage_snapshot(&tx, &write)
        .map_err(crate::repo::thread_index::into_storage_error)?;
    for notice in missing {
        let receipt = crate::HostInputReceipt {
            prompt_id: receipt_id(notice),
            run_id: record.run_id.clone(),
            root_run_id: record.run_id.clone(),
            text: String::new(),
            image_count: 0,
            status: crate::HostInputStatus::Delivered,
            reason: None,
            accepted_at_ns: now_ns,
            status_at_ns: now_ns,
            evidence_id: None,
            provenance: crate::HostInputProvenance::LaneNotification {
                notification_id: notice.notification_id.clone(),
                destination_thread_id: notice.destination_thread_id.clone(),
            },
        };
        crate::repo::host_input_receipt::insert(&tx, &receipt)?;
        tx.execute(
            "UPDATE lane_notifications SET receipt_prompt_id=?2 WHERE notification_id=?1",
            params![notice.notification_id, receipt.prompt_id],
        )?;
    }
    tx.commit()?;
    Ok(Some(snapshot))
}
pub(crate) fn ack(
    conn: &Connection,
    claim: &LaneDeliveryClaim,
    now_ns: i64,
) -> Result<(), StorageError> {
    let tx = transaction(conn)?;
    let mut all_acked = !claim.notifications.is_empty();
    for notice in &claim.notifications {
        let acked:Option<bool>=tx.query_row("SELECT state='acked' AND destination_thread_id=?2 AND payload=?3 FROM lane_notifications WHERE notification_id=?1",params![notice.notification_id,claim.destination_thread_id,json(notice)?],|r|r.get(0)).optional()?;
        all_acked &= acked == Some(true);
    }
    if all_acked {
        return Ok(());
    }
    check_claim(&tx, claim, now_ns)?;
    for notice in &claim.notifications {
        if !received(&tx, notice)? {
            return Err(conflict("receipt_required"));
        }
        tx.execute(
            "UPDATE lane_notifications SET state='acked' WHERE notification_id=?1",
            [&notice.notification_id],
        )?;
    }
    tx.execute("UPDATE lane_delivery_destinations SET lease_token=NULL,lease_until_ns=NULL WHERE thread_id=?1",[&claim.destination_thread_id])?;
    tx.commit()?;
    Ok(())
}
pub(crate) fn pending_destinations(
    conn: &Connection,
    after: &str,
) -> Result<Vec<String>, StorageError> {
    let mut statement=conn.prepare(&format!("SELECT DISTINCT n.destination_thread_id FROM lane_notifications n WHERE n.state='pending' AND n.destination_thread_id>?1 AND {LIVE_NOTICE} ORDER BY n.destination_thread_id LIMIT 20"))?;
    Ok(statement
        .query_map([after], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}
pub(crate) fn receipt_ids(
    conn: &Connection,
    claim: &LaneDeliveryClaim,
) -> Result<Vec<String>, StorageError> {
    let mut ids = Vec::new();
    for notice in &claim.notifications {
        if received(conn, notice)? {
            ids.push(notice.notification_id.clone());
        }
    }
    Ok(ids)
}
