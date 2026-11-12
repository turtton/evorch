//! Core-only startup boundary, after shared leases/reload and before producers.
use super::{BootstrapError, BootstrapOptions, services::BackgroundServices};
use crate::projects::StaticProjects;
use runtime::{AgentRuntime, ownership::OwnerHost};
use std::{future::Future, pin::Pin, sync::Arc};

/// Owned current-process inputs. No recipe can manufacture this lease proof.
pub struct CoreStartupInputs {
    pub options: BootstrapOptions,
    pub runtime: AgentRuntime,
    pub owner: Arc<OwnerHost>,
    pub storage: storage::StorageConfig,
    pub writer: storage::StorageHandle,
    pub projects: StaticProjects,
    pub startup: storage::RecoveryStartupComplete,
    pub work_changed: Arc<tokio::sync::Notify>,
}
/// Payload-free diagnostics; callers must not include recipes or provider input.
pub trait CoreStartupDiagnostics: Send + Sync {
    fn snapshot(&self) -> serde_json::Value;
    fn has_pending_work(&self) -> bool;
}
pub type CoreStartupResult = Result<Arc<dyn CoreStartupDiagnostics>, BootstrapError>;
pub type CoreStartupFuture<'a> = Pin<Box<dyn Future<Output = CoreStartupResult> + Send + 'a>>;
/// Implemented by the core, never invoked by legacy/embedded bootstrap.
pub trait CoreStartupHook: Sync {
    fn start<'a>(
        &'a self,
        inputs: CoreStartupInputs,
        services: &'a mut BackgroundServices,
    ) -> CoreStartupFuture<'a>;
}
