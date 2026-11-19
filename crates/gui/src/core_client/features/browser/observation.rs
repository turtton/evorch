//! O-G root, composed inside BrowserFeature, not another feature/connection.
//! All request/data tags in this leaf must have the high bit set. Rendering,
//! selection, reset and connection invalidation are forwarded by the parent.
use super::super::*;
use workbench_protocol::namespaces::browser::observation::ObservationTarget;
pub const TAG_BASE: u64 = 1 << 63;
pub fn owns_tag(tag: u64) -> bool {
    tag >= TAG_BASE
}
#[derive(Default)]
pub struct ObservationFeature {
    pending: Option<ObservationTarget>,
}
impl ObservationFeature {
    pub fn observe_run(&mut self, target: ObservationTarget) {
        self.pending = Some(target);
    }
}
impl Feature for ObservationFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Browser
    }
    fn title(&self) -> &'static str {
        "Agent observation"
    }
    fn reset(&mut self) {
        self.pending = None;
    }
    fn disconnected(&mut self) {
        self.pending = None;
    }
    fn selection_changed(&mut self, _: &mut FeatureContext<'_>) {
        self.pending = None;
    }
    // Intentionally a placeholder until O-G: no subscription or frame upload.
}
