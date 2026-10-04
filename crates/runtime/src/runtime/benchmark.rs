use super::*;
use crate::benchmark::{
    BenchmarkCheckpoint, BenchmarkRecorder, BenchmarkSelector, Recording, unsupported,
};

pub(crate) fn production_executor(
    bus: Arc<event_bus::EventBus>,
    root: PathBuf,
) -> Result<Arc<tools::ToolExecutor>, RuntimeError> {
    // Global output artifacts may contain later baseline evidence. Benchmark
    // executors return artifacts inside the trial workspace instead.
    let sandbox = sandbox::BwrapSandbox::detect(
        sandbox::BwrapConfig::new(root.clone())
            .allow_network(false)
            .isolate_processes(),
    )
    .map_err(unsupported)?;
    tools::ToolExecutor::with_standard_tools_in(bus, Arc::new(sandbox), Some(root))
        .with_web_tools()
        .map(Arc::new)
        .map_err(unsupported)
}

impl AgentRuntime {
    pub fn with_benchmark_recorder(
        self,
        selector: BenchmarkSelector,
        recorder: Arc<dyn BenchmarkRecorder>,
    ) -> Result<Self, RuntimeError> {
        if selector.occurrence == 0 || self.shared.next_run_id.load(Ordering::Relaxed) != 1 {
            return Err(unsupported(
                "attach a one-based selector before starting any runs",
            ));
        }
        self.shared
            .benchmark_recording
            .set(Recording {
                selector,
                recorder,
                seen: Mutex::new(0),
            })
            .map_err(|_| unsupported("recorder already attached"))?;
        self.set_sandbox_escalation(config::EscalationApproval::Off, false);
        Ok(self)
    }

    /// Replay after restoring the workspace snapshot, using a fresh runtime.
    /// The host must never reuse a workspace from an interrupted trial until
    /// process teardown has been independently confirmed.
    pub fn replay_benchmark(
        &self,
        mut checkpoint: BenchmarkCheckpoint,
        workspace_root: PathBuf,
        candidate: crate::ModelPreference,
    ) -> Result<RunId, RuntimeError> {
        if self.shared.next_run_id.load(Ordering::Relaxed) != 1
            || self.shared.benchmark_recording.get().is_some()
        {
            return Err(unsupported(
                "replay requires a fresh runtime without a recorder",
            ));
        }
        let root = workspace_root.canonicalize().map_err(unsupported)?;
        if checkpoint.version != 1
            || root != checkpoint.workspace_root
            || checkpoint.messages.is_empty()
        {
            return Err(unsupported(
                "checkpoint version, workspace path or messages mismatch",
            ));
        }
        let config = RunConfig {
            category: checkpoint.category.clone(),
            budget: checkpoint.budget.clone(),
            load_skills: checkpoint.load_skills.clone(),
            model_preference: Some(candidate.clone()),
            ..RunConfig::default()
        };
        crate::benchmark::validate_config(&config, checkpoint.role)?;
        checkpoint.model.preference = candidate;
        // Validate the provider/selection before reserving a run or mutating runtime state.
        Arc::clone(&self.shared.model).freeze_for_benchmark(checkpoint.model.clone())?;
        self.set_default_cwd(root)?;
        self.set_sandbox_escalation(config::EscalationApproval::Off, false);
        let run_id = self.reserve_run_id();
        let role = checkpoint.role;
        let prompt = checkpoint.prompt.clone();
        self.shared
            .benchmark_replays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run_id, checkpoint);
        Ok(self.spawn_reserved(run_id, None, role, prompt, config))
    }

    pub(crate) fn validate_benchmark_capture(
        &self,
        run_id: RunId,
        parent: RunId,
    ) -> Result<(), RuntimeError> {
        let runs = lock_runs(&self.shared.runs);
        let entry = runs.get(&run_id).ok_or_else(|| unknown_run(run_id))?;
        // Only awaited children have a parent that is synchronously fenced by wait().
        if !entry.completion_relayed {
            return Err(unsupported("background delegation"));
        }
        for (id, other) in runs.iter() {
            if *id != run_id
                && *id != parent
                && matches!(
                    *other.phase_rx.borrow(),
                    AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
                )
            {
                return Err(unsupported("parallel or nested active agents"));
            }
            if self
                .shared
                .benchmark_executors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(id)
                .is_some_and(|executor| executor.has_unobserved_shell_jobs(&id.to_string()))
            {
                return Err(unsupported("live or unobserved shell jobs"));
            }
        }
        Ok(())
    }
}
