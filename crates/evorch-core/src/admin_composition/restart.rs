//! Register the public adapter over the lifecycle's existing coordinator.
use super::Inputs;
#[cfg(target_os = "linux")]
mod finalization;
pub mod recipes;
pub mod targets;

pub(super) fn install(inputs: &Inputs<'_>) {
    #[cfg(not(target_os = "linux"))]
    inputs.host.admin.install_restart(std::sync::Arc::new(
        crate::restart::public::PublicRestartCoordinator,
    ));
    #[cfg(target_os = "linux")]
    if let Err(error) = install_linux(inputs) {
        // Registration is fail-closed. Do not publish a partly composed facade.
        tracing::warn!(%error, "restart leaf not installed");
    }
}
#[cfg(target_os = "linux")]
fn install_linux(inputs: &Inputs<'_>) -> Result<(), workbench_host::admin::AdminFailure> {
    use crate::restart::public::{LiveUndisposedAdmission, PublicRestartCoordinator};
    use std::sync::Arc;
    let (handle, context) = inputs
        .finalization()
        .ok_or_else(finalization::unavailable)?;
    let current = Arc::new(
        core_launcher::handoff::CurrentElf::record().map_err(|_| finalization::unavailable())?,
    );
    let targets = targets::build(context.options().config_dir().to_path_buf(), current)
        .map_err(|_| finalization::unavailable())?;
    let owner = finalization::ProductFinalizationOwner::new(inputs, handle.clone(), context)?;
    handle
        .install_environment_builder(finalization::environment)
        .map_err(|_| finalization::unavailable())?;
    let core = handle.coordinator().clone();
    let epoch = inputs.host.admin.epoch();
    inputs
        .host
        .admin
        .install_restart(Arc::new(PublicRestartCoordinator::new(
            core.clone(),
            epoch,
            targets,
            Arc::new(recipes::HostRecipeSource::new(inputs.host)),
            Arc::new(LiveUndisposedAdmission {
                collector: inputs.graph_collector().clone(),
                core,
                epoch,
            }),
            Arc::new(owner),
        )));
    Ok(())
}

#[cfg(test)]
mod tests;
