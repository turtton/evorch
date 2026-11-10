//! GUI-independent callbacks used by the Evaluator's host tools.
use std::sync::OnceLock;
pub type Reload = Box<dyn Fn() -> Result<(), String> + Send + Sync>;
#[derive(Default)]
pub struct EvaluatorHost {
    pub runtime: OnceLock<runtime::WeakAgentRuntime>,
    pub reload: OnceLock<Reload>,
}
impl eval::evaluator::EvaluatorHost for EvaluatorHost {
    fn authorize_run(&self, run_id: &str) -> Result<(), String> {
        let runtime = self
            .runtime
            .get()
            .and_then(runtime::WeakAgentRuntime::upgrade)
            .ok_or("the runtime is not running")?;
        let run = run_id.parse().map_err(|e| format!("{e}"))?;
        runtime
            .owns_background_run(run)
            .then_some(())
            .ok_or_else(|| "the initiating run is not owned by this host".into())
    }
    fn deliver_notice(&self, run_id: &str, notice: &str) -> Result<String, String> {
        self.authorize_run(run_id)?;
        let runtime = self
            .runtime
            .get()
            .and_then(runtime::WeakAgentRuntime::upgrade)
            .ok_or("the runtime is not running")?;
        runtime
            .deliver_notice(run_id.parse().map_err(|e| format!("{e}"))?, notice)
            .map(|run| run.to_string())
            .map_err(|error| error.to_string())
    }
    fn reload_config(&self) -> Result<(), String> {
        self.reload
            .get()
            .ok_or_else(|| "the models are not composed yet".to_owned())
            .and_then(|reload| reload())
    }
}
