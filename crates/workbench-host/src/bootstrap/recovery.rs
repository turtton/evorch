//! Read-only planning and explicit safe-point capture. No activation loop.
//! The caller stops ingress/producers before capture. 06b/c will acquire current
//! authorities and register the entire graph before activating any component.
use super::BootstrapError;
use runtime::{
    AgentRuntime, RunId,
    quiesce::{RecoveryPlan, RecoveryRecipe},
};
use std::collections::BTreeMap;

pub fn plan(runtime: &AgentRuntime) -> Result<RecoveryPlan, BootstrapError> {
    Ok(runtime.recovery_plan()?)
}

pub async fn capture(
    runtime: &AgentRuntime,
    fence: Option<&runtime::quiesce::DispatchDrainFence>,
    capture_id: &str,
    recipes: &BTreeMap<RunId, RecoveryRecipe>,
) -> Result<Vec<String>, BootstrapError> {
    let fence = fence.ok_or("recovery capture requires a producer fence")?;
    runtime.validate_restart_dispatch_fence(fence)?;
    runtime.drain_dispatch_tails(fence).await?;
    let records = runtime.capture_recovery_records(recipes)?;
    Ok(runtime.save_recovery_graph(capture_id, &records)?)
}
