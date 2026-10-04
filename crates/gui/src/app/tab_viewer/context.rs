use super::{AgentRunSource, WorkbenchTabViewer};
use crate::model::telemetry::TelemetryOverlay;
use crate::panes::context_inspector::{RunActuals, RunChoice};

impl<S: AgentRunSource> WorkbenchTabViewer<'_, S> {
    /// The active thread's runs newest first, then other runs observed this session.
    pub(super) fn context_run_choices(&self) -> Vec<RunChoice> {
        let thread_runs = self
            .sidebar
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == self.sidebar.active_thread.as_ref())
            .map(|thread| thread.run_ids.clone())
            .unwrap_or_default();
        let mut runs: Vec<String> = thread_runs.into_iter().rev().collect();
        for run in self.telemetry.run_ids() {
            if !runs.iter().any(|known| known == run) {
                runs.push(run.to_owned());
            }
        }
        runs.into_iter()
            .map(|run_id| {
                let role = crate::model::tasks::role_for_run(self.tasks.rows(), &run_id);
                let model = self
                    .telemetry
                    .row(&run_id)
                    .and_then(|row| row.model.as_deref());
                let label = [Some(run_id.as_str()), role, model]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ");
                RunChoice { run_id, label }
            })
            .collect()
    }
}

/// Provider usage and the runtime's latest estimate for `run`, as far as this session saw them.
pub(super) fn context_actuals(telemetry: &TelemetryOverlay, run: &str) -> RunActuals {
    let Some(row) = telemetry.row(run) else {
        return RunActuals::default();
    };
    let usage = row.latest_request_usage();
    RunActuals {
        latest_input: usage.map(|usage| usage.input),
        latest_cache_read: usage.map(|usage| usage.cache_read),
        estimate: row.context_composition.clone(),
    }
}
