//! Destination single flight, lease fencing and durable receipt/ACK repair.
use super::*;
pub(crate) const LIVE_NOTICE: &str = "n.source_kind='lane' AND (n.event_kind='answer' OR n.source_thread_id NOT IN (SELECT thread_id FROM thread_tombstones)) AND n.destination_thread_id NOT IN (SELECT thread_id FROM thread_tombstones) AND EXISTS(SELECT 1 FROM lanes l WHERE l.lane_id=n.lane_id AND l.management_revision=n.management_revision AND l.thread_id NOT IN (SELECT thread_id FROM thread_tombstones))";
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
pub(crate) fn destination(
    conn: &Connection,
    thread: &str,
) -> Result<Option<LaneDeliveryDestination>, StorageError> {
    live_thread(conn, thread)?;
    let row: Option<(u64, bool, Option<String>)> = conn
        .query_row(
            "SELECT epoch,stopped,owner_json FROM lane_delivery_destinations WHERE thread_id=?1",
            [thread],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    row.map(|(epoch, stopped, owner)| {
        Ok(LaneDeliveryDestination {
            thread_id: thread.into(),
            epoch,
            stopped,
            owner: owner.as_deref().map(parse).transpose()?,
        })
    })
    .transpose()
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
pub(crate) fn claim<N: OutboxNotice>(
    conn: &Connection,
    request: LaneClaimRequest,
) -> Result<LaneClaimResult<N>, StorageError> {
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
    // A committed receipt is enough to repair sender ACK state without a wake.
    // Do not touch a current claimant: owner/epoch and the lease were checked above.
    // Limit the work and require the exact non-user receipt, not just a matching ID.
    let repaired = tx.execute(
        &format!(
            "UPDATE lane_notifications SET state='acked' WHERE notification_id IN (
         SELECT n.notification_id FROM lane_notifications n
         JOIN host_input_receipts r ON r.prompt_id=n.receipt_prompt_id
         WHERE n.destination_thread_id=?1 AND n.state='pending' AND {live_notice}
           AND r.prompt_id='{receipt_prefix}'||n.notification_id AND r.status='delivered'
           AND json_extract(r.provenance_json,'$.kind')='{provenance_kind}'
           AND json_extract(r.provenance_json,'$.notification_id')=n.notification_id
           AND json_extract(r.provenance_json,'$.destination_thread_id')=n.destination_thread_id
           AND r.run_id NOT IN (SELECT run_id FROM run_tombstones)
           AND r.root_run_id NOT IN (SELECT run_id FROM run_tombstones)
         ORDER BY n.notification_id LIMIT 20)",
            live_notice = N::LIVE_NOTICE,
            receipt_prefix = N::RECEIPT_PREFIX,
            provenance_kind = N::PROVENANCE_KIND
        ),
        [&request.destination_thread_id],
    )?;
    let mut statement = tx.prepare(N::ELIGIBLE_QUERY)?;
    let values = statement
        .query_map(
            params![request.destination_thread_id, request.allow_non_waking],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut notifications = Vec::new();
    for value in values {
        let notice: N = parse(&value)?;
        if json(&(&notifications, &notice))?.len() > LANE_DELIVERY_PAGE_BYTES - 1024 {
            break;
        }
        notifications.push(notice);
    }
    if notifications.is_empty() {
        if repaired != 0 {
            tx.commit()?;
        }
        return Ok(LaneClaimResult::Empty);
    }
    let claim_epoch = claim_epoch
        .checked_add(1)
        .ok_or(StorageError::OutOfRange("claim epoch"))?;
    tx.execute("UPDATE lane_delivery_destinations SET claim_epoch=?2,owner_json=?3,lease_token=?4,lease_until_ns=?5,last_nack=NULL WHERE thread_id=?1",params![request.destination_thread_id,claim_epoch,json(&request.owner)?,request.token,request.lease_until_ns])?;
    for notice in &notifications {
        tx.execute(
            "UPDATE lane_notifications SET claim_epoch=?2 WHERE notification_id=?1",
            params![notice.notification_id(), claim_epoch],
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
        completed_root_wake: !request.allow_non_waking,
        notifications,
    }))
}
/// Read one consistent snapshot; unlike accept/ACK, this creates no receipt or
/// delivery progress. The runtime holds its restore gate across wake admission.
pub(crate) fn validate_claim(
    conn: &Connection,
    claim: &LaneDeliveryClaim,
    now_ns: i64,
) -> Result<(), StorageError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    check_claim(&tx, claim, now_ns)?;
    tx.commit()?;
    Ok(())
}
fn check_claim<N: OutboxNotice>(
    conn: &Connection,
    claim: &LaneDeliveryClaim<N>,
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
        notice.validate(conn, claim, received(conn, notice)?)?;
        let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM lane_notifications WHERE notification_id=?1 AND claim_epoch=?2 AND destination_thread_id=?3 AND payload=?4 AND state='pending')",params![notice.notification_id(),claim.claim_epoch,claim.destination_thread_id,json(notice)?],|r|r.get(0))?;
        if !valid {
            return Err(conflict("stale_notification"));
        }
    }
    Ok(())
}
pub(crate) fn nack<N: OutboxNotice>(
    conn: &Connection,
    claim: &LaneDeliveryClaim<N>,
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
fn receipt_id<N: OutboxNotice>(notice: &N) -> String {
    format!("{}{}", N::RECEIPT_PREFIX, notice.notification_id())
}
fn received<N: OutboxNotice>(conn: &Connection, notice: &N) -> Result<bool, StorageError> {
    let receipts = crate::repo::host_input_receipt::for_ids(conn, &[receipt_id(notice)])?;
    Ok(receipts.iter().any(|receipt| {
        receipt.status == crate::HostInputStatus::Delivered
            && receipt.provenance == notice.provenance()
    }))
}
/// Host snapshot contains only the IDs missing from receipt_ids(). Fully received
/// retries do not overwrite a snapshot. Notifications are never host_inputs_json.
pub(crate) fn accept_delivery<N: OutboxNotice>(
    conn: &Connection,
    claim: &LaneDeliveryClaim<N>,
    write: crate::thread_index::SnapshotWrite,
    now_ns: i64,
    auto_fix_enabled: bool,
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
    let mut wake_sources = std::collections::HashSet::<String>::new();
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
            provenance: notice.provenance(),
        };
        crate::repo::host_input_receipt::insert(&tx, &receipt)?;
        tx.execute(
            "UPDATE lane_notifications SET receipt_prompt_id=?2 WHERE notification_id=?1",
            params![notice.notification_id(), receipt.prompt_id],
        )?;
        notice.adopt(&tx, claim, now_ns, auto_fix_enabled, &mut wake_sources)?;
    }
    tx.commit()?;
    Ok(Some(snapshot))
}
pub(crate) fn ack<N: OutboxNotice>(
    conn: &Connection,
    claim: &LaneDeliveryClaim<N>,
    now_ns: i64,
) -> Result<(), StorageError> {
    let tx = transaction(conn)?;
    let mut all_acked = !claim.notifications.is_empty();
    for notice in &claim.notifications {
        let acked:Option<bool>=tx.query_row("SELECT state='acked' AND destination_thread_id=?2 AND payload=?3 FROM lane_notifications WHERE notification_id=?1",params![notice.notification_id(),claim.destination_thread_id,json(notice)?],|r|r.get(0)).optional()?;
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
            [notice.notification_id()],
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
    let mut statement = conn.prepare(&format!(
        "SELECT thread_id FROM (
           SELECT n.destination_thread_id AS thread_id FROM lane_notifications n
           WHERE n.state='pending' AND {LIVE_NOTICE}
           UNION
           SELECT d.thread_id FROM lane_delivery_destinations d
           JOIN run_contexts c ON c.run_id=json_extract(d.owner_json,'$.root_run_id')
           WHERE d.stopped=0 AND c.parent_run_id IS NULL
             AND c.terminal_phase IN ('Checkpoint','Error','Done')
             AND c.run_id NOT IN (SELECT run_id FROM run_tombstones)
             AND d.thread_id NOT IN (SELECT thread_id FROM thread_tombstones)
             AND CASE WHEN json_valid(c.config_json)
               THEN json_array_length(c.config_json,'$.spawn_credits.lane_presentations')>0 ELSE 0 END
         ) WHERE thread_id>?1 ORDER BY thread_id LIMIT 20"
    ))?;
    Ok(statement
        .query_map([after], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}
pub(crate) fn receipt_ids<N: OutboxNotice>(
    conn: &Connection,
    claim: &LaneDeliveryClaim<N>,
) -> Result<Vec<String>, StorageError> {
    let mut ids = Vec::new();
    for notice in &claim.notifications {
        if received(conn, notice)? {
            ids.push(notice.notification_id().to_owned());
        }
    }
    Ok(ids)
}

/// One source per claim: the concrete notice type fixes all SQL selection and
/// receipt identity. Destination lease, fencing, snapshot adoption and ACK are shared.
pub(crate) trait OutboxNotice: Serialize + DeserializeOwned + Sized {
    const LIVE_NOTICE: &'static str;
    const ELIGIBLE_QUERY: &'static str;
    const RECEIPT_PREFIX: &'static str;
    const PROVENANCE_KIND: &'static str;
    fn notification_id(&self) -> &str;
    fn provenance(&self) -> crate::HostInputProvenance;
    fn validate(
        &self,
        conn: &Connection,
        claim: &LaneDeliveryClaim<Self>,
        received: bool,
    ) -> Result<(), StorageError>;
    fn adopt(
        &self,
        conn: &Connection,
        claim: &LaneDeliveryClaim<Self>,
        now_ns: i64,
        auto_fix_enabled: bool,
        wake_sources: &mut std::collections::HashSet<String>,
    ) -> Result<(), StorageError>;
}
impl OutboxNotice for LaneNotification {
    const LIVE_NOTICE: &'static str = LIVE_NOTICE;
    const RECEIPT_PREFIX: &'static str = "lane-notification:";
    const PROVENANCE_KIND: &'static str = "lane_notification";
    const ELIGIBLE_QUERY: &'static str = "WITH eligible AS NOT MATERIALIZED (
           SELECT n.payload,n.notification_id,n.event_kind FROM lane_notifications n
           JOIN lanes l ON l.lane_id=n.lane_id
           WHERE n.destination_thread_id=?1 AND n.state='pending' AND n.source_kind='lane' AND (n.event_kind='answer' OR n.source_thread_id NOT IN (SELECT thread_id FROM thread_tombstones)) AND n.destination_thread_id NOT IN (SELECT thread_id FROM thread_tombstones) AND EXISTS(SELECT 1 FROM lanes l WHERE l.lane_id=n.lane_id AND l.management_revision=n.management_revision AND l.thread_id NOT IN (SELECT thread_id FROM thread_tombstones))
             AND (n.event_kind='answer' OR COALESCE(json_extract(l.wake_guard_json,'$.suppressed'),0)=0)
         )
         SELECT payload FROM eligible
         WHERE ?2 OR EXISTS(SELECT 1 FROM eligible WHERE event_kind IN ('question','ready','failed','answer','nudge'))
         ORDER BY CASE WHEN ?2 OR event_kind IN ('question','ready','failed','answer','nudge') THEN 0 ELSE 1 END,
                  notification_id LIMIT 20";
    fn notification_id(&self) -> &str {
        &self.notification_id
    }
    fn provenance(&self) -> crate::HostInputProvenance {
        crate::HostInputProvenance::LaneNotification {
            notification_id: self.notification_id.clone(),
            destination_thread_id: self.destination_thread_id.clone(),
        }
    }
    fn validate(
        &self,
        conn: &Connection,
        claim: &LaneDeliveryClaim,
        received: bool,
    ) -> Result<(), StorageError> {
        if !received {
            crate::repo::lane_control::check_nudge(conn, self)?;
        }
        // Accepted Answer provenance may outlive the old lead; the requester
        // and current lane/destination/claim remain authoritative.
        if !matches!(self.kind, LaneNotificationKind::Answer { .. }) {
            live_thread(conn, &self.source_thread_id)?;
        }
        let current = lane(conn, &self.lane_id)?;
        if matches!(self.kind, LaneNotificationKind::Nudge { .. })
            && (current.root_run_id.as_deref() != Some(&claim.owner.root_run_id)
                || current.generation != claim.owner.generation)
        {
            return Err(StorageError::LaneOperation(
                event_bus::lane::LaneOperationError::OwnershipLost,
            ));
        }
        management(current.management_revision, self.management_revision)
    }
    fn adopt(
        &self,
        conn: &Connection,
        claim: &LaneDeliveryClaim,
        now_ns: i64,
        _auto_fix_enabled: bool,
        wake_sources: &mut std::collections::HashSet<String>,
    ) -> Result<(), StorageError> {
        crate::repo::lane_control::accept_nudge(conn, self)?;
        let source = lane(conn, &self.lane_id)?;
        if source.thread_id == self.destination_thread_id {
            crate::repo::lane_control::input_progress(
                conn,
                &self.lane_id,
                true,
                matches!(
                    self.kind,
                    LaneNotificationKind::Intervention { .. } | LaneNotificationKind::Answer { .. }
                ),
                now_ns,
            )?;
        }
        if claim.completed_root_wake
            && !matches!(
                self.kind,
                LaneNotificationKind::Answer { .. } | LaneNotificationKind::Nudge { .. }
            )
            && wake_sources.insert(self.lane_id.clone())
        {
            let expected = guard(conn, &self.lane_id)?.revision;
            guards::update_guard_in_transaction(
                conn,
                &self.lane_id,
                expected,
                LaneGuardSignal::WakeBatch {
                    batch_id: format!("lane-batch:{}", self.notification_id),
                },
            )?;
        }
        Ok(())
    }
}
