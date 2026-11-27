//! One candidate; no host lock across SQL, Stop, queue waits or ACK.
use super::*;
use crate::areas::workspace::OnlineSnapshot;
use std::time::Instant;
use storage::thread_retention::{
    BindingInput, NANOS_PER_DAY, ThreadRetentionPolicy,
    online::{OnlineBudget, OnlineClosure, OnlineOutcome, OnlineRequest},
};
impl AutoArchiveCoordinator {
    pub fn with_delete_policy(mut self, policy: ThreadRetentionPolicy) -> Self {
        self.online_policy = Some(policy);
        self
    }
    pub fn with_send_slot(mut self, slot: Arc<Mutex<()>>) -> Self {
        self.send_slot = slot;
        self
    }
    fn snapshot(
        &self,
        host: &Arc<Mutex<CommandHost>>,
        closure: Option<&OnlineClosure>,
        now: i64,
        report: &mut RetentionReport,
    ) -> Option<OnlineSnapshot> {
        let Ok(host) = host.try_lock() else {
            report.hold("ForegroundPending");
            return None;
        };
        let start = Instant::now();
        let result = host
            .areas
            .workspace
            .online_retention_snapshot(closure, &self.root, now);
        drop(host);
        report.host_lock_ns = report
            .host_lock_ns
            .max(start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        match result {
            Ok(s) => Some(s),
            Err(e) => {
                report.failure(e);
                None
            }
        }
    }
    fn reclaim_deleted_attachment(
        &mut self,
        snapshot: &OnlineSnapshot,
        db: &storage::Database,
        report: &mut RetentionReport,
    ) {
        let id =
            match db.retention_tombstoned_thread_after(self.deleted_attachment_after.as_deref()) {
                Ok(Some(id)) => id,
                Ok(None) => {
                    self.deleted_attachment_after = None;
                    return;
                }
                Err(error) => {
                    report.failure(error);
                    return;
                }
            };
        // Advance even on failure; a busy/oversized bucket cannot starve another.
        self.deleted_attachment_after = Some(id.clone());
        // The caller checked the binding; attachment failure must not suppress this hint.
        if let Some(hint) = &snapshot.scratch_hint {
            hint(&id);
            report.scratch_hints += 1;
        }
        let result = (|| -> Result<(), String> {
            let root = snapshot
                .runtime
                .attachment_root()
                .map_err(|e| e.to_string())?;
            let _lock = root.try_lock_thread(&id).map_err(|e| e.to_string())?;
            report.attachment_calls += 1;
            root.remove_thread(&id).map_err(|e| e.to_string())
        })();
        if let Err(error) = result {
            report.hold("DeletedAttachmentCleanupPending");
            tracing::debug!(thread = id, %error, "deleted attachment cleanup will retry");
        }
    }
    fn online_hold(&mut self, seed: &str, reason: &'static str, report: &mut RetentionReport) {
        if reason.ends_with("Budget") && !self.permanent_holds.contains_key(seed) {
            tracing::warn!(
                thread = seed,
                reason,
                "online retention candidate permanently held; no startup fallback"
            );
            self.permanent_holds.insert(seed.into(), reason);
        }
        report.hold(reason);
    }
    pub(super) fn online_tick(
        &mut self,
        host: &Arc<Mutex<CommandHost>>,
        authority: &AutoArchiveAuthority<'_>,
        db: &storage::Database,
        now: i64,
        bound: bool,
        report: &mut RetentionReport,
    ) {
        if !bound {
            report.hold("BindingMismatch");
            return;
        }
        let Some(policy) = self.online_policy else {
            return;
        };
        // Tombstones remain the durable cleanup intent after sidebar removal,
        // including when new confirmations have since been disabled.
        let Some(initial) = self.snapshot(host, None, now, report) else {
            return;
        };
        self.reclaim_deleted_attachment(&initial, db, report);
        if policy.archived_delete_after_days == 0 {
            report.hold("DeletionDisabled");
            return;
        }
        let Some(period) = i64::from(policy.archived_delete_after_days).checked_mul(NANOS_PER_DAY)
        else {
            report.hold("PolicyOverflow");
            return;
        };
        let mut candidates = initial
            .input
            .threads
            .iter()
            .filter(|t| {
                t.archived
                    && !t.pinned
                    && t.archived_at_s.is_none_or(|s| {
                        s.checked_mul(1_000_000_000)
                            .and_then(|s| s.checked_add(period))
                            .is_some_and(|due| due <= now)
                    })
            })
            .map(|t| t.thread_id.clone())
            .collect::<Vec<_>>();
        candidates.sort();
        if candidates.is_empty() {
            self.delete_after = None;
            report.hold("NoDeleteCandidate");
            return;
        }
        let seed = if let Some(after) = &self.delete_after {
            candidates
                .iter()
                .find(|s| *s > after)
                .unwrap_or(&candidates[0])
                .clone()
        } else if !self.delete_started {
            self.delete_started = true;
            candidates[(now.unsigned_abs() / 1_000_000_000) as usize % candidates.len()].clone()
        } else {
            candidates[0].clone()
        };
        self.delete_after = Some(seed.clone());
        if let Some(reason) = self.permanent_holds.get(&seed) {
            report.hold(reason);
            return;
        }
        let budget = OnlineBudget::default();
        let closure = match db.retention_online_closure(&seed, budget) {
            Ok(OnlineOutcome::Ready(c)) => c,
            Ok(OnlineOutcome::Hold(h)) => {
                self.online_hold(&seed, h, report);
                return;
            }
            Ok(_) => {
                report.hold("ClosureUnavailable");
                return;
            }
            Err(e) => {
                report.failure(e);
                return;
            }
        };
        let Some(mut snapshot) = self.snapshot(host, Some(&closure), now, report) else {
            return;
        };
        if snapshot
            .before
            .iter()
            .any(|(_, v)| v.pending != 0 || v.failed != 0)
        {
            report.hold("AdmissionPending");
            return;
        }
        let mut idle = Vec::new();
        for id in &closure.runs {
            let Ok(run) = id.parse::<runtime::RunId>() else {
                report.hold("RunUnresolved");
                return;
            };
            report.inspected_runs += 1;
            if snapshot.runtime.retention_run_protected(run) {
                report.hold("LiveRun");
                return;
            }
            if snapshot.runtime.retention_delete_protected(run) {
                if snapshot.runtime.is_between_chat_turns(run) {
                    idle.push(run);
                } else {
                    report.hold("TaskNotQuiescent");
                    return;
                }
            }
        }
        for thread in &closure.threads {
            if snapshot
                .runtime
                .thread_goal(thread)
                .is_some_and(|g| g.phase != event_bus::ThreadGoalPhase::Complete)
            {
                report.hold("ActiveGoal");
                return;
            }
        }
        snapshot.input.sidebar_path = std::fs::canonicalize(&snapshot.input.sidebar_path)
            .unwrap_or_else(|_| PathBuf::from(&snapshot.input.sidebar_path))
            .to_string_lossy()
            .into_owned();
        snapshot.input.ownership_root = std::fs::canonicalize(&self.root)
            .unwrap_or_else(|_| self.root.clone())
            .to_string_lossy()
            .into_owned();
        let binding = BindingInput {
            ownership_root: snapshot.input.ownership_root.clone(),
            sidebar_path: snapshot.input.sidebar_path.clone(),
        };
        let db_identity = match db.retention_identity() {
            Ok(id) => id,
            Err(e) => {
                report.failure(e);
                return;
            }
        };
        let request = OnlineRequest {
            db_identity,
            seed: seed.clone(),
            expected: closure,
            input: snapshot.input,
            policy,
            binding,
        };
        match db.retention_online_prepare(&request, budget) {
            Ok(OnlineOutcome::Ready(_)) => {}
            Ok(OnlineOutcome::Hold(h)) => {
                self.online_hold(&seed, h, report);
                return;
            }
            Ok(_) => {
                report.hold("PreflightUnavailable");
                return;
            }
            Err(e) => {
                report.failure(e);
                return;
            }
        }
        // Producer permit covers the existing durable Stop, outside the host lock.
        if !idle.is_empty() {
            let start = Instant::now();
            for run in idle.into_iter().take(1) {
                match snapshot.runtime.stop_retention_idle(run, &snapshot.before) {
                    Ok(()) => report.stopped_runs += 1,
                    Err(e) => {
                        report.failure(e);
                        break;
                    }
                }
            }
            report.idle_stop_ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            report.hold("IdleStopping");
            return;
        }
        let fence = snapshot.runtime.retention_commit_fence(snapshot.before);
        let start = Instant::now();
        match snapshot
            .writer
            .confirm_retention_online(authority, request, budget, fence)
        {
            Ok(OnlineOutcome::Confirmed(family)) => {
                report.confirmed = family.threads.len();
                self.snapshot(host, None, now, report); // Later snapshots repair failures.
            }
            Ok(OnlineOutcome::Hold(h)) => self.online_hold(&seed, h, report),
            Ok(_) => report.hold("ConfirmationUnavailable"),
            Err(e) => report.failure(e),
        }
        report.writer_roundtrip_ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        report.seal_ns = snapshot.runtime.retention_max_seal_ns();
    }
}
