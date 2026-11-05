use super::*;
pub(crate) fn current_close(
    conn: &Connection,
    lane_id: &str,
) -> Result<Option<LaneCloseResult>, StorageError> {
    let l = lane(conn, lane_id)?;
    managed(conn, lane_id, l.management_revision, None)?;
    let payload: Option<String> = conn.query_row("SELECT c.payload FROM lanes l JOIN lane_closes c ON c.close_id=l.current_close_id WHERE l.lane_id=?1 AND l.thread_id NOT IN (SELECT thread_id FROM thread_tombstones)", [lane_id], |row| row.get(0)).optional()?;
    payload.as_deref().map(parse).transpose()
}
pub(crate) fn target(
    conn: &Connection,
    lane_id: &str,
) -> Result<LaneVerificationTarget, StorageError> {
    let l = lane(conn, lane_id)?;
    managed(conn, lane_id, l.management_revision, None)?;
    let report =
        crate::repo::lanes::reports(conn, lane_id, l.latest_report_seq.saturating_sub(1), 1)?
            .into_iter()
            .next()
            .ok_or_else(|| err(LaneOperationError::StaleReady))?;
    if report.status != LaneStatus::Ready
        || report.management_revision != l.management_revision
        || report.brief_revision != l.brief_revision
    {
        return Err(err(LaneOperationError::StaleReady));
    }
    Ok(LaneVerificationTarget {
        lane_id: l.lane_id,
        management_revision: l.management_revision,
        brief_revision: l.brief_revision,
        ready_seq: report.report_seq,
        request_revision: conn.query_row(
            "SELECT request_revision FROM lanes WHERE lane_id=?1",
            [lane_id],
            |r| r.get(0),
        )?,
        artifact_fingerprint: report
            .artifact_revision
            .as_ref()
            .and_then(ArtifactRevision::fingerprint),
    })
}
fn compare(conn: &Connection, expected: &LaneVerificationTarget) -> Result<(), StorageError> {
    managed(conn, &expected.lane_id, expected.management_revision, None)?;
    let current = target(conn, &expected.lane_id)?;
    if current.artifact_fingerprint != expected.artifact_fingerprint {
        return Err(err(LaneOperationError::ArtifactChanged));
    }
    if current.ready_seq != expected.ready_seq {
        return Err(err(LaneOperationError::StaleReady));
    }
    if current != *expected {
        return Err(err(LaneOperationError::StaleVerification));
    }
    Ok(())
}
/// Current ADR 0043 facts binding, read within the caller's transaction (no nested
/// thread_state transaction, no network). Missing/last-failed facts are unavailable.
pub(crate) fn pr_evidence(
    conn: &Connection,
    thread_id: &str,
) -> Result<Option<LanePrEvidence>, StorageError> {
    let Some(link) = crate::repo::pull_requests::active_link(conn, thread_id)? else {
        return Ok(None);
    };
    let snapshot = crate::repo::pull_requests::snapshots::get(conn, &link.key)?
        .ok_or_else(|| err(LaneOperationError::EvidenceUnavailable))?;
    if snapshot
        .last_attempt
        .as_ref()
        .is_some_and(|a| a.failure.is_some())
    {
        return Err(err(LaneOperationError::EvidenceUnavailable));
    }
    let facts = snapshot
        .facts
        .ok_or_else(|| err(LaneOperationError::EvidenceUnavailable))?;
    let fetched_at_ns = snapshot
        .fetched_at_ns
        .ok_or_else(|| err(LaneOperationError::EvidenceUnavailable))?;
    let head:Option<(String,i64)>=conn.query_row("SELECT local_head_sha,local_head_observed_at_ns FROM thread_pull_request_state WHERE thread_id=?1 AND local_head_link_revision=?2 AND local_head_sha IS NOT NULL",params![thread_id,link.link_revision],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    Ok(Some(LanePrEvidence {
        repository: link.key.repository,
        number: link.key.number,
        link_id: link.link_id,
        link_revision: link.link_revision,
        head_sha: facts.head_sha.clone(),
        success_seq: snapshot.success_seq,
        last_attempt_seq: snapshot.last_attempt.map_or(0, |a| a.seq),
        fetched_at_ns,
        facts_digest: digest(&facts)?,
        local_head_sha: head.as_ref().map(|(s, _)| s.clone()),
        local_head_observed_at_ns: head.map(|(_, at)| at),
    }))
}
fn check_pr(
    conn: &Connection,
    thread: &str,
    expected: &Option<LanePrEvidence>,
) -> Result<(), StorageError> {
    if let Some(expected) = expected
        && pr_evidence(conn, thread)?.as_ref() != Some(expected)
    {
        return Err(err(LaneOperationError::EvidenceUnavailable));
    }
    Ok(())
}
pub(crate) fn verification(
    conn: &Connection,
    id: VerificationId,
) -> Result<Option<LaneVerification>, StorageError> {
    let s:Option<String>=conn.query_row("SELECT v.payload FROM lane_verifications v JOIN lanes l ON l.lane_id=v.lane_id WHERE verification_id=?1 AND l.thread_id NOT IN (SELECT thread_id FROM thread_tombstones) AND v.reviewer_run_id NOT IN (SELECT run_id FROM run_tombstones)",[id.to_string()],|r|r.get(0)).optional()?;
    s.as_deref().map(parse).transpose()
}
pub(crate) fn begin_verification(
    conn: &Connection,
    r: BeginLaneVerification,
) -> Result<LaneVerification, StorageError> {
    id(&r.reviewer_run_id)?;
    let tx = tx(conn)?;
    let l = managed(
        &tx,
        &r.target.lane_id,
        r.target.management_revision,
        Some(&r.lead_thread_id),
    )?;
    compare(&tx, &r.target)?;
    check_pr(&tx, &l.thread_id, &r.pr_evidence)?;
    if let Some(old) = verification(&tx, r.verification_id)? {
        if old.target != r.target
            || old.reviewer_run_id != r.reviewer_run_id
            || old.pr_evidence != r.pr_evidence
        {
            return Err(err(LaneOperationError::OperationReused));
        }
        return Ok(old);
    }
    let used: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM lane_verifications WHERE reviewer_run_id=?1)",
        [&r.reviewer_run_id],
        |row| row.get(0),
    )?;
    if used {
        return Err(err(LaneOperationError::OperationReused));
    }
    let v = LaneVerification {
        verification_id: r.verification_id,
        target: r.target,
        reviewer_run_id: r.reviewer_run_id,
        state: LaneVerificationState::Pending,
        pr_evidence: r.pr_evidence,
        submission: None,
        created_at_ns: r.at_ns,
        submitted_at_ns: None,
        completed_at_ns: None,
    };
    tx.execute("INSERT INTO lane_verifications(verification_id,lane_id,reviewer_run_id,payload) VALUES (?1,?2,?3,?4)",params![v.verification_id.to_string(),v.target.lane_id,v.reviewer_run_id,bounded(&v,32768)?])?;
    tx.commit()?;
    Ok(v)
}
fn save_verification(
    conn: &Connection,
    v: &LaneVerification,
    old: LaneVerificationState,
) -> Result<(), StorageError> {
    if conn.execute("UPDATE lane_verifications SET payload=?2 WHERE verification_id=?1 AND json_extract(payload,'$.state')=?3",params![v.verification_id.to_string(),bounded(v,32768)?,json(&old)?.trim_matches('"')])?!=1 {return Err(err(LaneOperationError::AlreadySubmitted));}
    Ok(())
}
pub(crate) fn submit(
    conn: &Connection,
    verification_id: VerificationId,
    run_id: &str,
    result: LaneVerificationSubmission,
    at: i64,
) -> Result<LaneVerification, StorageError> {
    let tx = tx(conn)?;
    let mut v = verification(&tx, verification_id)?
        .ok_or_else(|| err(LaneOperationError::VerificationMissing))?;
    if v.reviewer_run_id != run_id {
        return Err(err(LaneOperationError::NotResponder));
    }
    if v.state != LaneVerificationState::Pending {
        return Err(err(LaneOperationError::AlreadySubmitted));
    }
    compare(&tx, &v.target)?;
    let l = lane(&tx, &v.target.lane_id)?;
    check_pr(&tx, &l.thread_id, &v.pr_evidence)?;
    id(&result.result_ref)?;
    if result.result_digest.len() != 64
        || !result.result_digest.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(err(LaneOperationError::InvalidInput));
    }
    let mut indices = std::collections::HashSet::new();
    if result.checks.len() != l.brief.acceptance_criteria.len() {
        return Err(err(LaneOperationError::VerificationRejected));
    }
    for c in &result.checks {
        if c.criterion_index as usize >= l.brief.acceptance_criteria.len()
            || !indices.insert(c.criterion_index)
            || c.evidence_refs.len() > 8
            || (c.passed && c.evidence_refs.is_empty())
            || (result.verdict == LaneVerificationVerdict::Approved && !c.passed)
        {
            return Err(err(LaneOperationError::VerificationRejected));
        }
        for reference in &c.evidence_refs {
            id(reference)?;
        }
    }
    v.submission = Some(result);
    v.state = LaneVerificationState::Submitted;
    v.submitted_at_ns = Some(at);
    save_verification(&tx, &v, LaneVerificationState::Pending)?;
    tx.commit()?;
    Ok(v)
}
/// Requires the recorded Reviewer run's durable normal terminal snapshot, not
/// merely a successful submit. Runtime persists that snapshot before this call.
pub(crate) fn complete_verification(
    conn: &Connection,
    verification_id: VerificationId,
    run_id: &str,
    at: i64,
) -> Result<LaneVerification, StorageError> {
    let tx = tx(conn)?;
    let mut v = verification(&tx, verification_id)?
        .ok_or_else(|| err(LaneOperationError::VerificationMissing))?;
    if v.reviewer_run_id != run_id {
        return Err(err(LaneOperationError::NotResponder));
    }
    if v.state == LaneVerificationState::Completed {
        return Ok(v);
    }
    if v.state != LaneVerificationState::Submitted || v.submission.is_none() {
        return Err(err(LaneOperationError::ReviewerIncomplete));
    }
    let done:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM run_contexts WHERE run_id=?1 AND role='Reviewer' AND terminal_phase='Done' AND updated_at_ns>=?2)",params![run_id,v.submitted_at_ns],|r|r.get(0))?;
    if !done {
        return Err(err(LaneOperationError::ReviewerIncomplete));
    }
    v.state = LaneVerificationState::Completed;
    v.completed_at_ns = Some(at);
    save_verification(&tx, &v, LaneVerificationState::Submitted)?;
    tx.commit()?;
    Ok(v)
}
pub(crate) fn close_lane(conn: &Connection, r: CloseLane) -> Result<LaneCloseResult, StorageError> {
    let tx = tx(conn)?;
    let l = managed(
        &tx,
        &r.target.lane_id,
        r.target.management_revision,
        Some(&r.lead_thread_id),
    )?;
    compare(&tx, &r.target)?;
    let current: Option<String> = tx.query_row(
        "SELECT current_close_id FROM lanes WHERE lane_id=?1",
        [&l.lane_id],
        |row| row.get(0),
    )?;
    let prior = if let Some(current) = current {
        let s: String = tx.query_row(
            "SELECT payload FROM lane_closes WHERE close_id=?1",
            [current],
            |row| row.get(0),
        )?;
        let mut result: LaneCloseResult = parse(&s)?;
        result.outcome = LaneCloseOutcome::AlreadyClosed;
        Some(result)
    } else {
        None
    };
    let effective_id = prior
        .as_ref()
        .map_or(r.verification_id, |result| result.verification_ref);
    let v = verification(&tx, effective_id)?
        .ok_or_else(|| err(LaneOperationError::VerificationMissing))?;
    if v.target != r.target {
        return Err(err(LaneOperationError::StaleVerification));
    }
    if v.state != LaneVerificationState::Completed || v.completed_at_ns.is_none() {
        return Err(err(LaneOperationError::ReviewerIncomplete));
    }
    if v.submission.as_ref().is_none_or(|s| {
        s.verdict != LaneVerificationVerdict::Approved || s.checks.iter().any(|c| !c.passed)
    }) {
        return Err(err(LaneOperationError::VerificationRejected));
    }
    // Fresh same-content facts may replace the read version for close, while the
    // original verification evidence remains immutable.
    match (&v.pr_evidence, &r.pr_evidence) {
        (Some(old), Some(new))
            if old.link_id == new.link_id
                && old.link_revision == new.link_revision
                && old.head_sha == new.head_sha
                && old.facts_digest == new.facts_digest
                && old.local_head_sha == new.local_head_sha => {}
        (None, None) => (),
        _ => return Err(err(LaneOperationError::EvidenceUnavailable)),
    }
    check_pr(&tx, &l.thread_id, &r.pr_evidence)?;
    if let Some(result) = prior {
        return Ok(result);
    }
    let close_id = format!("lc-{}", digest(&(&r.target, r.verification_id))?);
    let result = LaneCloseResult {
        lane_id: l.lane_id.clone(),
        close_id,
        verification_ref: r.verification_id,
        ready_seq: r.target.ready_seq,
        closed_revision: r.target.request_revision,
        outcome: LaneCloseOutcome::Closed,
    };
    tx.execute("INSERT INTO lane_closes(close_id,lane_id,verification_id,payload,pr_evidence_json) VALUES (?1,?2,?3,?4,?5)",params![result.close_id,l.lane_id,r.verification_id.to_string(),bounded(&result,4096)?,r.pr_evidence.as_ref().map(|p|bounded(p,4096)).transpose()?])?;
    tx.execute(
        "UPDATE lanes SET current_close_id=?2 WHERE lane_id=?1",
        params![l.lane_id, result.close_id],
    )?;
    crate::repo::lane_delivery::enqueue(
        &tx,
        LaneNotification {
            notification_id: String::new(),
            lane_id: l.lane_id,
            management_revision: l.management_revision,
            source_thread_id: l.thread_id,
            destination_thread_id: r.lead_thread_id,
            kind: LaneNotificationKind::Closed {
                close_id: result.close_id.clone(),
            },
            report_seq: Some(result.ready_seq),
            created_at_ns: r.at_ns,
        },
    )?;
    tx.commit()?;
    Ok(result)
}
