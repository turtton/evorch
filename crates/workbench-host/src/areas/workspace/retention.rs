//! Shared archive transition plus bounded automatic wrappers. Automatic work runs under
//! the host mutex, so it never races the foreground handlers of this process.
use super::*;
use crate::thread_retention::{AutoArchiveCoordinator, RetentionReport};
use runtime::thread_retention::gate::GateStamp;
use storage::thread_retention::decision::{DecisionBudget, FamilyMember, Verdict};

fn members(
    sidebar: &workspace_state::SidebarState,
    key: &ThreadId,
) -> Result<Vec<FamilyMember>, &'static str> {
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
            out.push(FamilyMember {
                thread_id: thread.id.to_string(),
                created_ns: created,
                runs,
            });
        }
    }
    out.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
    if out.is_empty() {
        return Err("FamilyUnresolved");
    }
    Ok(out)
}

/// Admission registrations of the family: global, each thread, each run.
fn stamps(
    runtime: &AgentRuntime,
    family: &[FamilyMember],
    runs: &[String],
) -> Vec<(String, GateStamp)> {
    let mut scopes = vec![String::new()];
    scopes.extend(family.iter().map(|m| format!("t:{}", m.thread_id)));
    scopes.extend(runs.iter().map(|r| format!("r:{r}")));
    scopes
        .into_iter()
        .map(|scope| {
            let stamp = runtime.retention_gate_stamp(&scope);
            (scope, stamp)
        })
        .collect()
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
    /// One tick, one family: the sidebar lock is held from the read to the save, the
    /// verdict comes from the canonical sources, and the admission stamps taken before
    /// the verdict are compared after it. An admission after the last comparison is an
    /// ordinary operation on an archived family.
    fn automatic(
        &self,
        state: &mut AutoArchiveCoordinator,
        db: &storage::Database,
        now_ns: i64,
        report: &mut RetentionReport,
    ) -> Result<(), String> {
        let runtime = self.runtime.as_ref().ok_or("runtime unavailable")?;
        let Some(cutoff) = state.policy.cutoff(now_ns) else {
            report.hold("AutoArchiveDisabled");
            return Ok(());
        };
        let deleted = self.deleted().map_err(|_| "tombstone read failed")?;
        let lock = self.store.lock().map_err(|_| "SidebarBusy")?;
        let mut document = self.store.read(&lock).map_err(|_| "sidebar read failed")?;
        document.prune_threads(&*deleted);
        let snapshot = self.repair_membership(&mut document.sidebar)?;
        let resolved = document.sidebar.resolve_states();
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
        let Some(key) = chosen else {
            state.after_family = None;
            report.hold("ScanComplete");
            return Ok(());
        };
        state.after_family = Some(key.to_string());
        if resolved[&key].pinned {
            report.hold("Pinned");
            return Ok(());
        }
        let family = members(&document.sidebar, &key).map_err(str::to_owned)?;
        let budget = DecisionBudget::default();
        let runs = match db
            .family_runs(&family, &budget)
            .map_err(|_| "family runs read failed")?
        {
            Ok(runs) => runs,
            Err(hold) => {
                report.hold(hold.reason());
                return Ok(());
            }
        };
        report.inspected_runs = runs.len();
        let before = stamps(runtime, &family, &runs);
        if before.iter().any(|(_, s)| s.failed != 0) {
            report.hold("EventPersistenceFailed");
            return Ok(());
        }
        if before.iter().any(|(_, s)| s.pending != 0) {
            report.hold("ForegroundPending");
            return Ok(());
        }
        for run in &runs {
            let parsed = run.parse().map_err(|_| "FamilyUnresolved")?;
            if runtime.retention_run_protected(parsed) {
                report.hold("LiveRun");
                return Ok(());
            }
        }
        if family.iter().any(|m| {
            runtime
                .thread_goal(&m.thread_id)
                .is_some_and(|g| g.phase != event_bus::ThreadGoalPhase::Complete)
        }) {
            report.hold("UnfinishedGoal");
            return Ok(());
        }
        let decision = db
            .decide_family_archive(&family, cutoff, &budget)
            .map_err(|_| "archive decision failed")?;
        report.decision = Some(decision.stats);
        if let Verdict::Hold(hold) = decision.verdict {
            report.hold(hold.reason());
            return Ok(());
        }
        #[cfg(feature = "test-support")]
        if let Some(hook) = state.after_decision.as_mut() {
            hook();
        }
        if stamps(runtime, &family, &runs) != before {
            report.hold("StaleRevision");
            return Ok(());
        }
        let done = Self::archive_transition(
            &mut document.sidebar,
            &key,
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
