//! Optimistic sidebar snapshots for online confirmation. No writer waits here.
use super::*;
use runtime::thread_retention::gate::GateStamp;
use storage::thread_retention::{RetentionInput, online::OnlineClosure};

pub(crate) struct OnlineSnapshot {
    pub input: RetentionInput,
    pub before: Vec<(String, GateStamp)>,
    pub runtime: AgentRuntime,
    pub writer: storage::StorageHandle,
}
impl WorkspaceArea {
    pub(crate) fn online_retention_snapshot(
        &self,
        closure: Option<&OnlineClosure>,
        root: &Path,
        now_ns: i64,
    ) -> Result<OnlineSnapshot, String> {
        let service = self.inner.as_ref().ok_or("WorkspaceUnavailable")?;
        let runtime = service.runtime.as_ref().ok_or("RuntimeUnavailable")?;
        let writer = service.mirror.clone().ok_or("WriterUnavailable")?;
        let guard = runtime
            .tombstone_guard()
            .ok_or("TombstoneGuardUnavailable")?;
        // Capture BEFORE the file read, not after: an admission that saves a pin
        // between snapshot and stamps must not appear to be part of the snapshot.
        let before = closure
            .map(|c| {
                let mut scopes = vec![String::new()];
                scopes.extend(c.threads.iter().map(|id| format!("t:{id}")));
                scopes.extend(c.runs.iter().map(|id| format!("r:{id}")));
                scopes
                    .into_iter()
                    .map(|s| {
                        let v = runtime.retention_gate_stamp(&s);
                        (s, v)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let lock = service.store.lock().map_err(|_| "SidebarBusy")?;
        let mut document = service
            .store
            .read(&lock)
            .map_err(|_| "SidebarUnavailable")?;
        let old_count = document.sidebar.threads.len();
        document.prune_threads(&|id| guard.check_thread(id).is_err());
        if old_count != document.sidebar.threads.len() {
            service
                .store
                .write(&lock, &document)
                .map_err(|_| "SidebarCleanupPending")?;
            service.changes.fetch_add(1, Ordering::Relaxed);
            service
                .registered
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|id| guard.check_thread(id).is_ok());
        }
        let mut threads = sidebar_threads(&document.sidebar);
        if let Some(closure) = closure {
            threads.retain(|t| closure.threads.contains(&t.thread_id));
        }
        Ok(OnlineSnapshot {
            input: RetentionInput {
                threads,
                sidebar_path: service.store.path().to_string_lossy().into_owned(),
                ownership_root: root.to_string_lossy().into_owned(),
                now_ns,
            },
            before,
            runtime: runtime.clone(),
            writer,
        })
    }
}
