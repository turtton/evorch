//! Keeps the storage usage ledger's prices and run ownership in step with the GUI.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use storage::usage::RunAttribution;

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use crate::model::telemetry::pricing::SharedUsagePricing;

const SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// Writer link for the ledger. Attribution is sent without waiting for the
/// writer, so the render thread never blocks on SQLite.
pub(crate) struct UsageLedgerLink {
    handle: storage::StorageHandle,
    pricing: SharedUsagePricing,
    attributed: HashMap<String, (String, Option<String>)>,
    next_sync: Option<Instant>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Share provider prices with the storage bridge and attribute runs to
    /// their thread and project as the sidebar binds them.
    pub fn with_usage_ledger(
        mut self,
        handle: storage::StorageHandle,
        pricing: SharedUsagePricing,
    ) -> Self {
        self.usage_ledger = Some(UsageLedgerLink {
            handle,
            pricing,
            attributed: HashMap::new(),
            next_sync: None,
        });
        self
    }

    pub(super) fn sync_usage_ledger(&mut self, now: Instant) {
        let Some(link) = self.usage_ledger.as_mut() else {
            return;
        };
        if link.next_sync.is_some_and(|next| now < next) {
            return;
        }
        link.next_sync = Some(now + SYNC_INTERVAL);
        let fresh = self.provider_settings.usage_pricing();
        {
            let mut pricing = link
                .pricing
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !pricing.same_source(&fresh) {
                *pricing = fresh;
            }
        }
        let mut changed = Vec::new();
        for thread in &self.sidebar.threads {
            let owner = (thread.id.to_string(), Some(thread.project_id.to_string()));
            for run_id in &thread.run_ids {
                if link.attributed.get(run_id) != Some(&owner) {
                    changed.push(RunAttribution {
                        run_id: run_id.clone(),
                        thread_id: owner.0.clone(),
                        project_id: owner.1.clone(),
                    });
                }
            }
        }
        if !changed.is_empty() && link.handle.try_attribute_usage_runs(changed.clone()) {
            for attribution in changed {
                link.attributed.insert(
                    attribution.run_id,
                    (attribution.thread_id, attribution.project_id),
                );
            }
        }
    }
}
