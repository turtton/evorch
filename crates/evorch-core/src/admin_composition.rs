//! Composition root of the management leaves (restart, quarantine, owned graphs).
//!
//! Installing a leaf is separate from publishing management: `AdminService`
//! refuses every operation until its switch is on, and the only product code
//! that turns it on is [`capability`]. Both product entries install once.
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
    #[cfg(target_os = "linux")]
    finalization: Option<(
        crate::lifecycle::finalization::FinalizationHandle,
        &'a crate::lifecycle::product_exit::ProductExitContext,
    )>,
}
impl<'a> Inputs<'a> {
    pub fn new(host: &'a BootstrappedHost, backend: &'a HostBackend) -> Self {
        Self {
            host,
            backend,
            #[cfg(target_os = "linux")]
            finalization: None,
        }
    }
    /// Trusted entry-only wiring. Tests/shared callers may omit it; restart
    /// finalization then remains unavailable. Leaf installation uses the same
    /// coordinator, instance, signal reader and control-drain owner as the loop.
    #[cfg(target_os = "linux")]
    pub fn with_finalization(
        mut self,
        handle: crate::lifecycle::finalization::FinalizationHandle,
        context: &'a crate::lifecycle::product_exit::ProductExitContext,
    ) -> Self {
        self.finalization = Some((handle, context));
        self
    }
    #[cfg(target_os = "linux")]
    pub fn finalization(
        &self,
    ) -> Option<(
        &crate::lifecycle::finalization::FinalizationHandle,
        &'a crate::lifecycle::product_exit::ProductExitContext,
    )> {
        self.finalization
            .as_ref()
            .map(|(handle, context)| (handle, *context))
    }
    /// The exact collector installed before automatic recovery, shared by both leaves.
    pub fn graph_collector(
        &self,
    ) -> &'a std::sync::Arc<workbench_host::bootstrap::recovery_disposal::LiveGraphCollector> {
        self.host.graph_collector()
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
