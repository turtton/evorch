//! Offline renderer lifecycle. The protocol/provider remain disabled until F2.
use super::AreaHandles;
use crate::bootstrap::BootstrapError;
mod budget;
mod profiles;
pub mod renderer;
#[derive(Default)]
pub struct BrowserArea {
    pub renderer: Option<renderer::RendererOwner>,
}
impl BrowserArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let config = renderer::OwnerConfig::installed(handles.config_dir.join("renderer-profiles"))
            .map_err(|e| BootstrapError::from(e.to_string()))?;
        let renderer =
            renderer::RendererOwner::new(&handles.tasks, handles.spawn_ledger.clone(), config)
                .map_err(|e| BootstrapError::from(e.to_string()))?;
        Ok(Self {
            renderer: Some(renderer),
        })
    }
}
