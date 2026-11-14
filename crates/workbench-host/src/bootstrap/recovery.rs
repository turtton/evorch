//! Read-only planning and explicit safe-point capture. No activation loop.
//! The caller stops ingress/producers before capture. 06b/c will acquire current
//! authorities and register the entire graph before activating any component.
#[path = "recovery_authority.rs"]
pub mod authority;
#[path = "recovery_bindings.rs"]
pub mod bindings;
#[path = "recovery_catalog.rs"]
pub mod catalog;
#[path = "recovery_current.rs"]
pub mod current;
use super::BootstrapError;
use runtime::{
    AgentRuntime, RunId,
    quiesce::{RecoveryPlan, RecoveryRecipe},
};
use std::collections::BTreeMap;

pub fn plan(runtime: &AgentRuntime) -> Result<RecoveryPlan, BootstrapError> {
    Ok(runtime.recovery_plan()?)
}

/// Explicit only: a read-only startup plan must never admit a provider task.
/// PR 06c supplies the lease-bound evidence after exclusive startup is complete.
pub fn prepare(
    runtime: &AgentRuntime,
    component: &runtime::quiesce::RecoveryComponent,
    startup: storage::RecoveryStartupComplete,
    ownership_root: &std::path::Path,
) -> Result<runtime::PausedRecoveryComponent, BootstrapError> {
    Ok(runtime.prepare_component(component, startup, ownership_root)?)
}

pub fn release(
    runtime: &AgentRuntime,
    component: &runtime::PausedRecoveryComponent,
    authority: &runtime::CurrentRecoveryAuthority,
    reason: &str,
) -> Result<(), BootstrapError> {
    Ok(runtime.release_prepared_component_with_authority(component, authority, reason)?)
}

/// Current authority is supplied explicitly; startup planning never calls this.
pub fn activate(
    runtime: &AgentRuntime,
    mut component: runtime::PausedRecoveryComponent,
    authority: runtime::CurrentRecoveryAuthority,
) -> Result<Vec<RunId>, BootstrapError> {
    runtime.restore_relations_and_continuations(&mut component)?;
    Ok(runtime.activate_component(component, authority)?)
}

/// `capture_id` is unique per attempt, including retries. Components commit
/// independently; on error inspect durable manifests before a fresh-ID retry.
/// Before capture, a restart attempt can be cancelled with durable checkpoint
/// invalidation. Once capture starts, only terminal-only shutdown/Stop is allowed;
/// that handoff invalidates the captured graph and never grants normal resume.
pub async fn capture(
    runtime: &AgentRuntime,
    fence: Option<&runtime::quiesce::DispatchDrainFence>,
    capture_id: &str,
    recipes: &BTreeMap<RunId, RecoveryRecipe>,
) -> Result<Vec<String>, BootstrapError> {
    let fence = fence.ok_or("recovery capture requires a producer fence")?;
    runtime.validate_restart_dispatch_fence(fence)?;
    runtime.drain_dispatch_tails(fence).await?;
    let records = runtime.capture_recovery_records(fence, recipes)?;
    Ok(runtime.save_recovery_graph(capture_id, &records)?)
}
