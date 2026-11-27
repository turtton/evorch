//! One display-body unit before archive/delete early returns. Host/sidebar locks
//! are released before durable checks, writer roundtrip and commit notification.
use super::*;
use storage::thread_retention::{
    BindingInput,
    tool_bodies::{MAX_RUNS, Request, cutoff},
};
impl AutoArchiveCoordinator {
    pub fn with_tool_body_policy(mut self, after_days: u32) -> Self {
        self.tool_body_days = after_days;
        self
    }
    pub(super) fn tool_body_tick(
        &mut self,
        host: &Arc<Mutex<CommandHost>>,
        authority: &AutoArchiveAuthority<'_>,
        db: &storage::Database,
        now: i64,
        bound: bool,
        report: &mut RetentionReport,
    ) {
        if self.tool_body_days == 0 {
            return;
        }
        if !bound {
            report.hold("ToolBodyBindingMismatch");
            return;
        }
        if cutoff(now, self.tool_body_days).is_none() {
            report.hold("ToolBodyInvalidDeadline");
            return;
        }
        if self.tool_body_run.is_none() {
            match db.tool_body_candidate(&mut self.tool_body_cursor, now) {
                Ok(Some(c)) => {
                    self.tool_body_run = Some((c.thread_id, c.run_id, c.event_id.saturating_sub(1)))
                }
                Ok(None) => return,
                Err(e) => {
                    report.failure(e);
                    return;
                }
            }
        }
        let (thread, run, after) = self.tool_body_run.clone().unwrap();
        // Consume candidate even when held: a protected run cannot starve the next.
        self.tool_body_run = None;
        let canonical = match db.tool_body_canonical(&thread) {
            Ok(Some(c)) => c,
            Ok(None) => {
                report.hold("ToolBodyCanonicalUnresolved");
                return;
            }
            Err(e) => {
                report.failure(e);
                return;
            }
        };
        if canonical.runs.len() > MAX_RUNS {
            report.hold("ToolBodyCanonicalRunBudget");
            return;
        }
        let snapshot = {
            let Ok(host) = host.try_lock() else {
                report.hold("ForegroundPending");
                return;
            };
            host.areas
                .workspace
                .online_retention_snapshot(None, &self.root, now)
        };
        let snapshot = match snapshot {
            Ok(s) => s,
            Err(e) => {
                report.failure(e);
                return;
            }
        };
        let Some(sidebar) = snapshot
            .input
            .threads
            .iter()
            .find(|s| s.thread_id == thread)
        else {
            report.hold("ToolBodySidebarMissing");
            return;
        };
        if sidebar.pinned || canonical.pinned {
            report.hold("ToolBodyPinned");
            return;
        }
        if sidebar.pin_revision != canonical.pin_revision
            || sidebar.created_at_s.checked_mul(1_000_000_000) != Some(canonical.created_ns)
        {
            report.hold("ToolBodySidebarChanged");
            return;
        }
        // Read-only live snapshot. Unlike archive, EVERY Waiting phase and task tail
        // is protected; no Stop is sent, and unfinished goals are not inspected.
        for id in &canonical.runs {
            let Ok(id) = id.parse::<runtime::RunId>() else {
                report.hold("ToolBodyRunUnresolved");
                return;
            };
            report.inspected_runs += 1;
            if snapshot.runtime.retention_delete_protected(id) {
                report.hold("ToolBodyLiveRun");
                return;
            }
        }
        let identity = match db.retention_identity() {
            Ok(id) => id,
            Err(e) => {
                report.failure(e);
                return;
            }
        };
        let request = Request {
            db_identity: identity,
            binding: BindingInput {
                ownership_root: std::fs::canonicalize(&snapshot.input.ownership_root)
                    .unwrap_or_else(|_| PathBuf::from(&snapshot.input.ownership_root))
                    .to_string_lossy()
                    .into_owned(),
                sidebar_path: std::fs::canonicalize(&snapshot.input.sidebar_path)
                    .unwrap_or_else(|_| PathBuf::from(&snapshot.input.sidebar_path))
                    .to_string_lossy()
                    .into_owned(),
            },
            expected: canonical,
            run_id: run.clone(),
            after_event: after,
            now_ns: now,
            after_days: self.tool_body_days,
        };
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.after_decision.as_mut() {
            hook();
        }
        match snapshot.writer.retain_tool_bodies(authority, request) {
            Ok(p) => {
                if let Some(h) = p.hold {
                    report.hold(h);
                } else if !p.done {
                    self.tool_body_run = Some((thread, run, p.after_event));
                }
                report.tool_bodies = Some(p);
            }
            Err(e) => report.failure(e),
        }
    }
}
