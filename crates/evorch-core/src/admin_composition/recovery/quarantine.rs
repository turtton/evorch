//! Q1: the quarantine leaf, built from the host's own storage and registered
//! with the common admin service. Registering is not publishing: the service
//! refuses every operation until `capability` publishes a complete composition
//! (the switch is on, on Linux, with every leaf installed).
use super::super::Inputs;
use std::sync::Arc;
use workbench_host::{QuarantineFacade, StorageQuarantineStore};

pub(super) fn install(inputs: &Inputs<'_>) {
    // Reads open their own read-only connection from the production
    // configuration; discards go through the host's single writer.
    let store = StorageQuarantineStore::new(
        inputs.storage_config().clone(),
        inputs.storage_writer().clone(),
    );
    // The epoch the hello and the admin service stamp, so a binding read from
    // one is accepted by the other.
    let epoch = inputs.backend.projection.instance_epoch();
    let facade = QuarantineFacade::new(Arc::new(store), inputs.host.admin.reader().clone(), epoch);
    inputs.host.admin.install_quarantine(facade);
}

#[cfg(test)]
mod tests;
