//! Shared archive transition plus bounded automatic wrappers; no arbitration layer.
use super::*;
use crate::thread_retention::{
    AutoArchiveCoordinator, Candidate, Member, RetentionReport, RunProof,
};
const INSPECTION_BUDGET: usize = 64;
fn members(
    sidebar: &workspace_state::SidebarState,
    key: &ThreadId,
) -> Result<Vec<Member>, &'static str> {
    let states = sidebar.resolve_states();
    let mut out = Vec::new();
    for thread in &sidebar.threads {
        if states.get(&thread.id).and_then(|s| s.family.as_ref().ok()) == Some(key) {
            let created = thread
                .created_at
                .checked_mul(1_000_000_000)
                .ok_or("FamilyUnresolved")?;
            let mut runs = thread.run_ids.clone();
            if let Some(root) = &thread.root_run_id {
                runs.push(root.clone());
            }
            runs.sort();
            runs.dedup();
            out.push(Member {
                id: thread.id.clone(),
                runs,
                created,
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    if out.is_empty() {
        return Err("FamilyUnresolved");
    }
    Ok(out)
}
impl Service {
    /// The same numbered family mutation is used by manual and automatic archive.
    pub(super) fn archive_transition(
        sidebar: &mut workspace_state::SidebarState,
        id: &ThreadId,
        archived: bool,
        at: i64,
    ) -> Result<Done, WorkspaceRejection> {
        let changed = sidebar
            .set_family_archived(id, archived, at)
            .map_err(thread_rejection)?;
        let revision = sidebar
            .resolve_states()
            .get(id)
            .map_or(0, |s| s.archive_revision);
        Ok(Done {
            applied: Applied {
                revision: Revision(revision),
                changed,
                project: None,
            },
            access: false,
        })
    }
    fn automatic(
        &self,
        state: &mut AutoArchiveCoordinator,
        db: &storage::Database,
        now_ns: i64,
        report: &mut RetentionReport,
    ) -> Result<(), String> {
        let runtime = self.runtime.as_ref().ok_or("runtime unavailable")?;
        let deleted = self.deleted().map_err(|_| "tombstone read failed")?;
        let lock = self.store.lock().map_err(|_| "SidebarBusy")?;
        let mut document = self.store.read(&lock).map_err(|_| "sidebar read failed")?;
        document.prune_threads(&*deleted);
        let snapshot = self.repair_membership(&mut document.sidebar)?;
        let resolved = document.sidebar.resolve_states();
        if state.candidate.is_none() && state.policy.after_days != 0 {
            let mut keys: Vec<_> = resolved
                .iter()
                .filter_map(|(id, s)| {
                    (!s.archived && s.family.as_ref().ok() == Some(id)).then_some(id.clone())
                })
                .collect();
            keys.sort();
            let chosen = keys.into_iter().find(|id| {
                state
                    .after_family
                    .as_ref()
                    .is_none_or(|after| id.to_string() > *after)
            });
            if let Some(key) = chosen {
                let current = &resolved[&key];
                if current.pinned {
                    report.hold("Pinned");
                    state.after_family = Some(key.to_string());
                    return Ok(());
                }
                let family = members(&document.sidebar, &key).map_err(str::to_owned)?;
                let activity = family.iter().map(|m| m.created).max().unwrap_or(i64::MIN);
                state.candidate = Some(Candidate {
                    key,
                    members: family,
                    archive_revision: current.archive_revision,
                    pin_revision: current.pin_revision,
                    member: 0,
                    run: 0,
                    db_after: 0,
                    binding_revision: db
                        .activity_binding_revision()
                        .map_err(|_| "binding revision read failed")?,
                    seen: Default::default(),
                    activity,
                    proofs: vec![],
                    global_gate: runtime.retention_gate_stamp(""),
                    thread_gates: Default::default(),
                });
            } else {
                state.after_family = None;
            }
        }
        let Some(mut candidate) = state.candidate.take() else {
            return Ok(());
        };
        let current = resolved.get(&candidate.key).ok_or("FamilyUnresolved")?;
        if current.archived {
            report.hold("AlreadyApplied");
            state.after_family = Some(candidate.key.to_string());
            return Ok(());
        }
        if current.pinned {
            report.hold("Pinned");
            state.after_family = Some(candidate.key.to_string());
            return Ok(());
        }
        if current.archive_revision != candidate.archive_revision
            || current.pin_revision != candidate.pin_revision
            || members(&document.sidebar, &candidate.key).map_err(str::to_owned)?
                != candidate.members
        {
            report.hold("StaleRevision");
            state.after_family = Some(candidate.key.to_string());
            return Ok(());
        }
        let mut work = 0;
        while candidate.member < candidate.members.len() && work < INSPECTION_BUDGET {
            let member = candidate.members[candidate.member].clone();
            if candidate.run == 0 && candidate.db_after == 0 {
                work += 1;
                let stamp = runtime.retention_gate_stamp(&format!("t:{}", member.id));
                if stamp.pending != 0 {
                    report.hold("ForegroundPending");
                    state.after_family = Some(candidate.key.to_string());
                    return Ok(());
                }
                candidate.thread_gates.insert(member.id.to_string(), stamp);
                if let Some(hold) = db
                    .activity_thread_hold(&member.id.to_string())
                    .map_err(|_| "protection read failed")?
                {
                    report.hold(hold);
                    state.after_family = Some(candidate.key.to_string());
                    return Ok(());
                }
                if runtime
                    .thread_goal(&member.id.to_string())
                    .is_some_and(|g| g.phase != event_bus::ThreadGoalPhase::Complete)
                {
                    report.hold("UnfinishedGoal");
                    state.after_family = Some(candidate.key.to_string());
                    return Ok(());
                }
            }
            while candidate.run < member.runs.len() && work < INSPECTION_BUDGET {
                let run = &member.runs[candidate.run];
                work += 1;
                candidate.run += 1;
                if let Some(hold) = Self::inspect_run(runtime, db, run, &mut candidate, report)? {
                    report.hold(hold);
                    state.after_family = Some(candidate.key.to_string());
                    return Ok(());
                }
            }
            if candidate.run < member.runs.len() || work == INSPECTION_BUDGET {
                break;
            }
            let remaining = INSPECTION_BUDGET - work;
            let rows = db
                .activity_thread_run_page(&member.id.to_string(), candidate.db_after, remaining)
                .map_err(|_| "member run page failed")?;
            let complete = rows.len() < remaining;
            for (position, run) in rows {
                work += 1;
                candidate.db_after = position;
                if let Some(hold) = Self::inspect_run(runtime, db, &run, &mut candidate, report)? {
                    report.hold(hold);
                    state.after_family = Some(candidate.key.to_string());
                    return Ok(());
                }
            }
            if complete {
                candidate.member += 1;
                candidate.run = 0;
                candidate.db_after = 0;
            } else {
                break;
            }
        }
        if candidate.member < candidate.members.len() {
            report.hold("FamilyInspectionPending");
            state.candidate = Some(candidate);
            return Ok(());
        }
        state.after_family = Some(candidate.key.to_string());
        if !state.policy.due(candidate.activity, now_ns) {
            report.hold("NotDue");
            return Ok(());
        }
        if db
            .activity_binding_revision()
            .map_err(|_| "binding revision read failed")?
            != candidate.binding_revision
        {
            report.hold("StaleRevision");
            return Ok(());
        }
        // Final canonical checks are under host + sidebar locks. No run-map lock
        // spans these reads. Admissions after this check are ordinary archived work.
        for member in &candidate.members {
            if let Some(hold) = db
                .activity_thread_hold(&member.id.to_string())
                .map_err(|_| "protection read failed")?
            {
                report.hold(hold);
                return Ok(());
            }
            if runtime
                .thread_goal(&member.id.to_string())
                .is_some_and(|g| g.phase != event_bus::ThreadGoalPhase::Complete)
            {
                report.hold("UnfinishedGoal");
                return Ok(());
            }
        }
        for proof in &candidate.proofs {
            let activity = db
                .run_activity(&proof.run)
                .map_err(|_| "activity read failed")?;
            if !activity.ready || activity.revision != proof.revision {
                report.hold("SourceRevisionBehind");
                return Ok(());
            }
            if db
                .activity_question_protected(&proof.run)
                .map_err(|_| "question read failed")?
            {
                report.hold("UnansweredQuestion");
                return Ok(());
            }
        }
        // Memory-only live/gate inspection is last, after all I/O above.
        for proof in &candidate.proofs {
            let gate = runtime.retention_gate_stamp(&format!("r:{}", proof.run));
            if gate != proof.gate || gate.pending != 0 {
                report.hold("StaleRevision");
                return Ok(());
            }
            if runtime.retention_run_protected(proof.run.parse().map_err(|_| "FamilyUnresolved")?) {
                report.hold("LiveRun");
                return Ok(());
            }
        }
        if runtime.retention_gate_stamp("") != candidate.global_gate
            || candidate.thread_gates.iter().any(|(thread, stamp)| {
                runtime.retention_gate_stamp(&format!("t:{thread}")) != *stamp
            })
        {
            report.hold("StaleRevision");
            return Ok(());
        }
        let done = Self::archive_transition(
            &mut document.sidebar,
            &candidate.key,
            true,
            now_ns.div_euclid(1_000_000_000),
        )
        .map_err(|_| "family archive failed")?;
        self.store
            .write(&lock, &document)
            .map_err(|_| "sidebar save failed")?;
        self.project_saved(&lock, &mut document, snapshot);
        drop(lock);
        self.publish(&done);
        report.published = 1;
        Ok(())
    }
    fn inspect_run(
        runtime: &AgentRuntime,
        db: &storage::Database,
        run: &str,
        candidate: &mut Candidate,
        report: &mut RetentionReport,
    ) -> Result<Option<&'static str>, String> {
        if !candidate.seen.insert(run.to_owned()) {
            return Ok(None);
        }
        report.inspected_runs += 1;
        let parsed = run.parse().map_err(|_| "FamilyUnresolved")?;
        let gate = runtime.retention_gate_stamp(&format!("r:{run}"));
        if gate.pending != 0 {
            return Ok(Some("ForegroundPending"));
        }
        if runtime.retention_run_protected(parsed) {
            return Ok(Some("LiveRun"));
        }
        if db
            .activity_question_protected(run)
            .map_err(|_| "question read failed")?
        {
            return Ok(Some("UnansweredQuestion"));
        }
        let activity = db.run_activity(run).map_err(|_| "activity read failed")?;
        if !activity.ready {
            return Ok(Some(if activity.failure.is_some() {
                "SourceRevisionBehind"
            } else {
                "BackfillPending"
            }));
        }
        if let Some(at) = activity
            .last_actual_activity_at
            .into_iter()
            .chain(activity.legacy_activity_upper_bound)
            .max()
        {
            candidate.activity = candidate.activity.max(at)
        }
        candidate.proofs.push(RunProof {
            run: run.to_owned(),
            revision: activity.revision,
            gate,
        });
        Ok(None)
    }
    fn automatic_cleanup(
        &self,
        state: &mut AutoArchiveCoordinator,
        report: &mut RetentionReport,
    ) -> Result<(), String> {
        let runtime = self.runtime.as_ref().ok_or("runtime unavailable")?;
        // The sidebar is the only durable cleanup intent. The cursor is expendable.
        let lock = self.store.lock().map_err(|_| "SidebarBusy")?;
        let document = self.store.read(&lock).map_err(|_| "sidebar read failed")?;
        let mut ids: Vec<_> = document
            .sidebar
            .resolve_states()
            .into_iter()
            .filter_map(|(id, s)| s.archived.then_some(id.to_string()))
            .collect();
        ids.sort();
        let chosen = ids
            .into_iter()
            .find(|id| state.cleanup_after.as_ref().is_none_or(|after| id > after));
        drop(lock);
        let Some(id) = chosen else {
            state.cleanup_after = None;
            return Ok(());
        };
        state.cleanup_after = Some(id.clone());
        let root = runtime
            .attachment_root()
            .map_err(|_| "AttachmentCleanupPending")?;
        let _attachment = root
            .try_lock_thread(&id)
            .map_err(|_| "AttachmentCleanupPending")?;
        let lock = self.store.lock().map_err(|_| "SidebarBusy")?;
        let current = self.store.read(&lock).map_err(|_| "sidebar read failed")?;
        let archived = current
            .sidebar
            .resolve_states()
            .get(&ThreadId::new(&id))
            .is_some_and(|s| s.archived);
        drop(lock);
        if archived {
            if let Some(hint) = &self.scratch_hint {
                hint(&id);
                report.scratch_hints = 1;
            }
            report.hold("ScratchHintBestEffort");
            report.attachment_calls = 1;
            root.remove_thread(&id)
                .map_err(|_| "AttachmentCleanupPending")?;
        }
        Ok(())
    }
}
impl WorkspaceArea {
    pub(crate) fn accepts_auto_archive_authority(
        &self,
        authority: &storage::thread_retention::AutoArchiveAuthority<'_>,
        root: &std::path::Path,
    ) -> bool {
        self.inner
            .as_ref()
            .and_then(|service| service.mirror.as_ref())
            .is_some_and(|writer| {
                writer
                    .validate_auto_archive_authority(authority, root)
                    .is_ok()
            })
    }
    pub(crate) fn auto_archive_tick(
        &self,
        state: &mut AutoArchiveCoordinator,
        db: &storage::Database,
        now_ns: i64,
        report: &mut RetentionReport,
    ) {
        let Some(service) = &self.inner else {
            report.hold("WorkspaceUnavailable");
            return;
        };
        if let Err(error) = service.automatic(state, db, now_ns, report) {
            match error.as_str() {
                "SidebarBusy" => report.hold("SidebarBusy"),
                "FamilyUnresolved" => report.hold("FamilyUnresolved"),
                _ => report.failure(error),
            }
        }
        if let Err(error) = service.automatic_cleanup(state, report) {
            match error.as_str() {
                "SidebarBusy" => report.hold("SidebarBusy"),
                "AttachmentCleanupPending" => report.hold("AttachmentCleanupPending"),
                _ => report.failure(error),
            }
        }
    }
}
