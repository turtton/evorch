use std::sync::Arc;

use super::{ImprovementCollector, ImprovementSettings, read_crash_spool};
use crate::AgentRuntime;

/// The receiver itself owns a bus sender, so dropping the bus cannot close it.
/// Abort on Shared drop instead; the task holds no strong runtime reference.
pub(crate) struct ObserverTask(tokio::task::JoinHandle<()>);

impl Drop for ObserverTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl AgentRuntime {
    pub fn self_improvement_settings(&self) -> Option<ImprovementSettings> {
        self.shared.self_improvement.get().cloned()
    }

    /// Opt in to passive collection. Reconfiguration is warn-ignored, not a panic.
    pub fn with_self_improvement(self, settings: ImprovementSettings) -> Self {
        if self.shared.self_improvement.set(settings).is_err() {
            tracing::warn!("self-improvement already configured; ignoring replacement");
        }
        self
    }

    /// Start once, using tokio::spawn like delegate_background. Call from an async
    /// Tokio context; no context, unset settings, or repeat starts are warn-only.
    /// Subscribe synchronously so events emitted immediately after start are seen.
    pub fn start_self_improvement(&self) {
        if self.shared.self_improvement.get().is_none() {
            tracing::warn!("self-improvement not configured; observer not started");
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::warn!("self-improvement observer requires a Tokio context");
            return;
        }
        let mut started = false;
        self.shared.self_improvement_task.get_or_init(|| {
            started = true;
            let receiver = self.shared.bus.subscribe();
            let weak = Arc::downgrade(&self.shared);
            let producers = self.producer_gate();
            ObserverTask(tokio::spawn(async move {
                let settings = weak
                    .upgrade()
                    .and_then(|shared| shared.self_improvement.get().cloned());
                if let Some(settings) = settings {
                    let _ = ImprovementCollector::new(settings)
                        .run_until_gated(receiver, std::future::pending(), |_| true, producers)
                        .await;
                }
            }))
        });
        if !started {
            tracing::warn!("self-improvement observer already started; ignoring repeat");
        }
    }

    /// Recover <resolved-draft-dir>/crash-spool, unlinking only after a successful
    /// writer acknowledgement. Returns the number removed, not the number stored
    /// (dedup/rate limits still apply). Failed/disabled intake leaves files intact.
    /// Composition resolves the default via from_config.
    pub fn ingest_spooled_crashes(&self) -> usize {
        let Some(settings) = self.shared.self_improvement.get() else {
            return 0;
        };
        if !settings.policy.collect_diagnostics {
            return 0;
        }
        let Some(dir) = settings.policy.draft_dir.as_deref() else {
            tracing::warn!("self-improvement crash spool directory not resolved");
            return 0;
        };
        let spool = dir.join("crash-spool");
        let collector = ImprovementCollector::new(settings.clone());
        let mut removed = 0;
        for crash in read_crash_spool(&spool) {
            let path = spool.join(&crash.file_name);
            if let Err(error) = collector.ingest_crash(crash) {
                tracing::warn!(%error, "crash intake failed; spool retained");
                continue;
            }
            match std::fs::remove_file(path) {
                Ok(()) => removed += 1,
                Err(error) => {
                    tracing::warn!(%error, "acknowledged crash spool could not be removed")
                }
            }
        }
        removed
    }
}
