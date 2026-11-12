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

/// `capture_id` is unique per attempt, including retries. Components commit
/// independently; on error inspect durable manifests before a fresh-ID retry.
/// After restart drain closes dispatch, the caller must exit, not fall back to
/// operator shutdown (generation-fenced reopen / close handoff belongs to PR 07).
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
