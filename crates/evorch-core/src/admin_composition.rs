//! Composition root of the management leaves (restart, quarantine, owned graphs).
//!
//! Installing a leaf is separate from publishing management: `AdminService`
//! refuses every operation until its switch is on, and the only product code
//! that turns it on is [`capability`]. Nothing calls [`install`] yet; the
//! entrypoint hook belongs to a later change.
use crate::backend::HostBackend;
use storage::{StorageConfig, StorageHandle};
use workbench_host::bootstrap::BootstrappedHost;

pub mod capability;
mod recovery;
mod restart;

/// What the leaves are built from. Leaf changes read it and do not widen it
/// without the root's owner.
pub struct Inputs<'a> {
    pub host: &'a BootstrappedHost,
    pub backend: &'a HostBackend,
}
impl<'a> Inputs<'a> {
    pub fn new(host: &'a BootstrappedHost, backend: &'a HostBackend) -> Self {
        Self { host, backend }
    }
    /// The production database configuration, read-only.
    pub fn storage_config(&self) -> &'a StorageConfig {
        self.host.storage_config()
    }
    /// The host's storage writer handle, read-only; clone it to share the writer.
    pub fn storage_writer(&self) -> &'a StorageHandle {
        self.host.storage_writer()
    }
}

/// Register the leaves, then publish management if and only if the one switch
/// in [`capability`] allows it.
pub fn install(inputs: &Inputs<'_>) -> capability::Publication {
    recovery::install(inputs);
    restart::install(inputs);
    capability::publish(&inputs.host.admin, inputs.backend)
}
