//! The core-owned renderer and its private local API.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;
mod budget;
pub mod observation;
mod profiles;
pub mod renderer;
pub mod run_pages;
pub(crate) mod service;
pub use service::RendererReconnectGrace;
#[derive(Default)]
pub struct BrowserArea {
    pub data: renderer::data::DataPlane,
    pub observation: Option<std::sync::Arc<observation::service::ObservationService>>,
    pub renderer: Option<renderer::RendererOwner>,
    service: Option<std::sync::Arc<service::RendererService>>,
}
impl BrowserArea {
    pub(crate) fn service(
        &self,
    ) -> Result<&std::sync::Arc<service::RendererService>, workbench_protocol::errors::WorkbenchError>
    {
        self.service.as_ref().ok_or_else(service::unavailable)
    }
    /// The unexpired grace of a disconnected client's preview, as a copy.
    pub(super) fn reconnect_grace(&self) -> service::RendererReconnectGrace {
        self.service
            .as_ref()
            .map(|service| service.reconnect_grace())
            .unwrap_or_default()
    }
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let config = renderer::OwnerConfig::installed(handles.config_dir.join("renderer-profiles"))
            .map_err(|e| BootstrapError::from(e.to_string()))?;
        let data = renderer::data::DataPlane::default();
        let renderer = renderer::RendererOwner::new_with_data_plane(
            &handles.tasks,
            handles.spawn_ledger.clone(),
            config,
            data.clone(),
        )
        .map_err(|e| BootstrapError::from(e.to_string()))?;
        let service = service::RendererService::new(handles, renderer.clone())?;
        Ok(Self {
            observation: Some(observation::service::ObservationService::from_handles(
                handles,
                data.clone(),
            )?),
            data,
            renderer: Some(renderer),
            service: Some(service),
        })
    }
}
