use super::*;
pub(crate) fn current_close(
    conn: &Connection,
    lane_id: &str,
) -> Result<Option<LaneCloseResult>, StorageError> {
    let l = lane(conn, lane_id)?;
    if !l.policy.is_managed() {
        return Ok(None);
    }
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
/// The lane's PR as the ledger stores it, read within the caller's transaction (no nested
/// thread_state transaction, no network). It reports what is missing instead of failing,
/// so a close can record the state it was decided in; it never decides anything.
pub(crate) fn pr_context(
    conn: &Connection,
    thread_id: &str,
) -> Result<LaneClosePrContext, StorageError> {
    let context = |status, evidence| LaneClosePrContext { status, evidence };
    let Some(link) = crate::repo::pull_requests::active_link(conn, thread_id)? else {
        return Ok(context(LanePrContextStatus::NotLinked, None));
    };
    let Some(snapshot) = crate::repo::pull_requests::snapshots::get(conn, &link.key)? else {
        return Ok(context(LanePrContextStatus::NoSnapshot, None));
    };
    let failed = snapshot
        .last_attempt
        .as_ref()
        .is_some_and(|a| a.failure.is_some());
    let (Some(facts), Some(fetched_at_ns)) = (snapshot.facts.as_ref(), snapshot.fetched_at_ns)
    else {
        return Ok(context(
            if failed {
                LanePrContextStatus::LastFetchFailed
            } else {
                LanePrContextStatus::NoSnapshot
            },
            None,
        ));
    };
    let head:Option<(String,i64)>=conn.query_row("SELECT local_head_sha,local_head_observed_at_ns FROM thread_pull_request_state WHERE thread_id=?1 AND local_head_link_revision=?2 AND local_head_sha IS NOT NULL",params![thread_id,link.link_revision],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let evidence = LanePrEvidence {
        repository: link.key.repository,
        number: link.key.number,
        link_id: link.link_id,
        link_revision: link.link_revision,
        head_sha: facts.head_sha.clone(),
        success_seq: snapshot.success_seq,
        last_attempt_seq: snapshot.last_attempt.as_ref().map_or(0, |a| a.seq),
        fetched_at_ns,
        facts_digest: digest(facts)?,
        local_head_sha: head.as_ref().map(|(s, _)| s.clone()),
        local_head_observed_at_ns: head.map(|(_, at)| at),
    };
    Ok(context(
        if failed {
            LanePrContextStatus::LastFetchFailed
        } else {
            LanePrContextStatus::Available
        },
        Some(evidence),
    ))
}
/// Current ADR 0043 facts binding for a review. Missing or last-failed facts are
/// unavailable here; a close uses [`pr_context`] and is never refused for them.
pub(crate) fn pr_evidence(
    conn: &Connection,
    thread_id: &str,
) -> Result<Option<LanePrEvidence>, StorageError> {
    let context = pr_context(conn, thread_id)?;
    match context.status {
        LanePrContextStatus::NotLinked => Ok(None),
        LanePrContextStatus::Available => Ok(context.evidence),
        LanePrContextStatus::NoSnapshot | LanePrContextStatus::LastFetchFailed => {
            Err(err(LaneOperationError::EvidenceUnavailable))
        }
    }
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
    if r.caveats.len() > LANE_CAVEATS_MAX {
        return Err(err(LaneOperationError::InvalidInput));
    }
    for caveat in &r.caveats {
        id(caveat)?;
    }
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
        criteria_count: u32::try_from(l.brief.acceptance_criteria.len()).ok(),
        caveats: r.caveats,
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
    // A review is advisory. It is judged against the criteria it was started with and is
    // kept even when the lane's request, brief or artifact moved on while it ran; the
    // reviewer identity, state and tombstone checks above still apply.
    let l = lane(&tx, &v.target.lane_id)?;
    let criteria = v
        .criteria_count
        .map_or(l.brief.acceptance_criteria.len(), |n| n as usize);
    id(&result.result_ref)?;
    if result.result_digest.len() != 64
        || !result.result_digest.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(err(LaneOperationError::InvalidInput));
    }
    let mut indices = std::collections::HashSet::new();
    if result.checks.len() != criteria {
        return Err(err(LaneOperationError::VerificationRejected));
    }
    for c in &result.checks {
        if c.criterion_index as usize >= criteria
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
/// Root of the lead thread must be the caller's, checked inside the close transaction
/// so a Stop or restart that replaced the root cannot be raced.
fn is_lead_root(conn: &Connection, thread: &str, root: &str) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM threads t WHERE t.thread_id=?1 AND t.root_run_id=?2
         AND t.thread_id NOT IN (SELECT thread_id FROM thread_tombstones)
         AND ?2 NOT IN (SELECT run_id FROM run_tombstones))",
        params![thread, root],
        |r| r.get(0),
    )?)
}
/// The latest report, which is the only Ready a close may be about.
fn latest_report(conn: &Connection, l: &LaneRecord) -> Result<Option<LaneReport>, StorageError> {
    if l.latest_report_seq == 0 {
        return Ok(None);
    }
    Ok(
        crate::repo::lanes::reports(conn, &l.lane_id, l.latest_report_seq - 1, 1)?
            .into_iter()
            .next(),
    )
}
/// A Reviewer that ended without a complete submission (failed, cancelled, or finished
/// without submitting) leaves its verification Incomplete, so the lane is not shown as
/// still under review. A completed review is never changed.
pub(crate) fn incomplete_verification(
    conn: &Connection,
    verification_id: VerificationId,
    run_id: &str,
) -> Result<LaneVerification, StorageError> {
    let tx = tx(conn)?;
    let mut v = verification(&tx, verification_id)?
        .ok_or_else(|| err(LaneOperationError::VerificationMissing))?;
    if v.reviewer_run_id != run_id {
        return Err(err(LaneOperationError::NotResponder));
    }
    let old = v.state;
    if !matches!(
        old,
        LaneVerificationState::Pending | LaneVerificationState::Submitted
    ) {
        return Ok(v);
    }
    v.state = LaneVerificationState::Incomplete;
    save_verification(&tx, &v, old)?;
    tx.commit()?;
    Ok(v)
}
pub(crate) fn close_context(
    conn: &Connection,
    lane_id: &str,
) -> Result<LaneCloseContext, StorageError> {
    let l = lane(conn, lane_id)?;
    managed(conn, lane_id, l.management_revision, None)?;
    let ready = latest_report(conn, &l)?.filter(|r| {
        r.status == LaneStatus::Ready && r.management_revision == l.management_revision
    });
    Ok(LaneCloseContext {
        lane_id: l.lane_id,
        management_revision: l.management_revision,
        request_revision: conn.query_row(
            "SELECT request_revision FROM lanes WHERE lane_id=?1",
            [lane_id],
            |r| r.get(0),
        )?,
        brief_revision: l.brief_revision,
        latest_ready_seq: ready.as_ref().map(|r| r.report_seq),
        ready_brief_revision: ready.map(|r| r.brief_revision),
    })
}
/// What a verification's durable state means now. A Reviewer that already ended is not
/// "under way" even when the termination was never written back (a crash between the
/// run's final snapshot and the update), so this reads the run's own record. It only
/// derives: reading attention changes nothing.
pub(crate) fn derived_state(
    conn: &Connection,
    v: &LaneVerification,
) -> Result<LaneVerificationState, StorageError> {
    if !matches!(
        v.state,
        LaneVerificationState::Pending | LaneVerificationState::Submitted
    ) {
        return Ok(v.state);
    }
    let phase: Option<String> = conn
        .query_row(
            "SELECT terminal_phase FROM run_contexts WHERE run_id=?1 AND role='Reviewer'",
            [&v.reviewer_run_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match (phase.as_deref(), v.state, v.submission.is_some()) {
        (Some("Done"), LaneVerificationState::Submitted, true) => LaneVerificationState::Completed,
        (Some("Done" | "Error" | "Stopped"), _, _) => LaneVerificationState::Incomplete,
        _ => v.state,
    })
}
fn review_snapshot(
    conn: &Connection,
    lane_id: &str,
    id: VerificationId,
    ready: Option<&LaneReport>,
    brief_revision: u64,
) -> Result<LaneCloseReview, StorageError> {
    let v = verification(conn, id)?.ok_or_else(|| err(LaneOperationError::VerificationMissing))?;
    if v.target.lane_id != lane_id {
        return Err(err(LaneOperationError::InvalidInput));
    }
    let targets_this_ready = ready.is_some_and(|r| {
        v.target.ready_seq == r.report_seq && v.target.brief_revision == brief_revision
    });
    Ok(LaneCloseReview {
        verification_id: v.verification_id,
        reviewer_run_id: v.reviewer_run_id.clone(),
        // As lane attention sees it: a Reviewer that already ended (failed, cancelled,
        // stopped, or finished without submitting) is not recorded as still running.
        state: derived_state(conn, &v)?,
        verdict: v.submission.as_ref().map(|s| s.verdict),
        result_ref: v.submission.as_ref().map(|s| s.result_ref.clone()),
        result_digest: v.submission.as_ref().map(|s| s.result_digest.clone()),
        targets_this_ready,
        pr_evidence: v.pr_evidence,
        caveats: v.caveats,
        artifact_rechecked_at_close: false,
    })
}
pub(crate) fn close_lane(conn: &Connection, r: CloseLane) -> Result<LaneCloseResult, StorageError> {
    let note = r.note.trim();
    if note.is_empty() || note.len() > LANE_CLOSE_NOTE_MAX_BYTES {
        return Err(err(LaneOperationError::InvalidInput));
    }
    let valid_shape = match &r.closer {
        LaneCloser::Lead { .. } => r.kind == LaneCloseKind::LeadClosed && r.ready_seq.is_some(),
        LaneCloser::Human { .. } => r.kind != LaneCloseKind::LeadClosed,
    };
    if !valid_shape || r.ready_seq == Some(0) {
        return Err(err(LaneOperationError::InvalidInput));
    }
    let tx = tx(conn)?;
    let (lead, closed_by) = match &r.closer {
        LaneCloser::Lead {
            thread_id,
            root_run_id,
        } => (
            Some(thread_id.as_str()),
            LaneCloseActor::Lead {
                thread_id: thread_id.clone(),
                root_run_id: root_run_id.clone(),
            },
        ),
        LaneCloser::Human { surface, client_id } => {
            id(surface)?;
            if let Some(client_id) = client_id {
                id(client_id)?;
            }
            (
                None,
                LaneCloseActor::Human {
                    surface: surface.clone(),
                    client_id: client_id.clone(),
                },
            )
        }
    };
    let l = managed(&tx, &r.lane_id, r.expected_management_revision, lead)?;
    if let LaneCloser::Lead {
        thread_id,
        root_run_id,
    } = &r.closer
        && !is_lead_root(&tx, thread_id, root_run_id)?
    {
        return Err(err(LaneOperationError::NotCurrentLead));
    }
    // The report the close is about. A lead may only close the latest Ready that was
    // reported for the current brief; a human may also end a lane that has none.
    let report = latest_report(&tx, &l)?;
    let ready = match (r.ready_seq, report) {
        (Some(seq), Some(report)) => {
            if report.report_seq != seq
                || report.status != LaneStatus::Ready
                || report.management_revision != l.management_revision
            {
                return Err(err(LaneOperationError::StaleReady));
            }
            if r.kind == LaneCloseKind::LeadClosed && report.brief_revision != l.brief_revision {
                return Err(err(LaneOperationError::StaleBrief));
            }
            Some(report)
        }
        (Some(_), None) => return Err(err(LaneOperationError::StaleReady)),
        (None, _) => None,
    };
    let request_revision: u64 = tx.query_row(
        "SELECT request_revision FROM lanes WHERE lane_id=?1",
        [&l.lane_id],
        |row| row.get(0),
    )?;
    if request_revision != r.expected_request_revision {
        return Err(err(LaneOperationError::StaleRequest));
    }
    let review = r
        .verification_id
        .map(|id| review_snapshot(&tx, &l.lane_id, id, ready.as_ref(), l.brief_revision))
        .transpose()?;
    let current: Option<String> = tx.query_row(
        "SELECT current_close_id FROM lanes WHERE lane_id=?1",
        [&l.lane_id],
        |row| row.get(0),
    )?;
    // A retry (same revisions, same Ready) returns the close that stands; any later
    // input, report, brief or management change has cleared it and bumped a revision,
    // so a late retry is rejected above instead of closing again. The first closer's
    // record is returned unchanged, whoever retries.
    if let Some(current) = current {
        let payload: String = tx.query_row(
            "SELECT payload FROM lane_closes WHERE close_id=?1",
            [current],
            |row| row.get(0),
        )?;
        let mut prior: LaneCloseResult = parse(&payload)?;
        prior.outcome = LaneCloseOutcome::AlreadyClosed;
        return Ok(prior);
    }
    let close_id = format!(
        "lc-{}",
        digest(&(
            &l.lane_id,
            r.kind.as_str(),
            l.management_revision,
            request_revision,
            r.ready_seq
        ))?
    );
    let result = LaneCloseResult {
        lane_id: l.lane_id.clone(),
        close_id,
        kind: r.kind,
        ready_seq: ready.as_ref().map(|x| x.report_seq),
        closed_revision: request_revision,
        management_revision: l.management_revision,
        brief_revision: l.brief_revision,
        report_brief_revision: ready.as_ref().map(|x| x.brief_revision),
        artifact_refs: ready
            .as_ref()
            .map(|x| x.artifact_refs.clone())
            .unwrap_or_default(),
        artifact_revision: ready.as_ref().and_then(|x| x.artifact_revision.clone()),
        note: note.to_owned(),
        review,
        pr_context: Some(pr_context(&tx, &l.thread_id)?),
        closed_by: Some(closed_by),
        at_ns: r.at_ns,
        outcome: LaneCloseOutcome::Closed,
    };
    tx.execute(
        "INSERT INTO lane_closes(close_id,lane_id,verification_id,kind,payload) VALUES (?1,?2,?3,?4,?5)",
        params![
            result.close_id,
            l.lane_id,
            r.verification_id.map(|id| id.to_string()),
            r.kind.as_str(),
            bounded(&result, 32768)?
        ],
    )?;
    tx.execute(
        "UPDATE lanes SET current_close_id=?2 WHERE lane_id=?1",
        params![l.lane_id, result.close_id],
    )?;
    // The lead is told whoever closed. The close wakes nothing else and stops nothing.
    let destination = l
        .lead_thread_id
        .clone()
        .ok_or_else(|| err(LaneOperationError::NotAttached))?;
    crate::repo::lane_delivery::enqueue(
        &tx,
        LaneNotification {
            notification_id: String::new(),
            lane_id: l.lane_id,
            management_revision: l.management_revision,
            source_thread_id: l.thread_id,
            destination_thread_id: destination,
            kind: LaneNotificationKind::Closed {
                close_id: result.close_id.clone(),
            },
            report_seq: result.ready_seq,
            created_at_ns: r.at_ns,
        },
    )?;
    tx.commit()?;
    Ok(result)
}
