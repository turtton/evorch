//! Q2: the owned-graphs leaf, built from the host's own runtime, owner,
//! storage, reader and the one live collector, and registered with the common
//! admin service. Registering is not publishing: the service refuses every
//! operation until `capability` publishes a complete composition (the switch is
//! on, on Linux, with every leaf installed).
use super::super::Inputs;
use workbench_host::bootstrap::recovery_disposal::OwnedGraphsFacade;

pub(super) fn install(inputs: &Inputs<'_>) {
    // The recovery diagnostics exist only once the host has started its
    // recovery (cold boot, or planned activation after durable Ready). Before
    // that the leaf stays unregistered, so management stays refused and
    // unadvertised instead of serving a facade that cannot ask recovery.
    let Some(diagnostics) = inputs.host.recovery_diagnostics.clone() else {
        tracing::warn!("owned-graphs leaf not installed: host recovery has not started");
        return;
    };
    let facade = OwnedGraphsFacade::new(
        inputs.host.runtime.clone(),
        inputs.host.owner.clone(),
        inputs.storage_config().clone(),
        inputs.host.admin.reader().clone(),
        diagnostics,
        // Shared with restart admission; a second collector would not see the
        // effects this facade records.
        inputs.graph_collector().clone(),
    );
    inputs.host.admin.install_owned_graphs(facade);
}

#[cfg(test)]
mod tests;
