//! Desktop feature of the `diagnostics` area (PR 13). Placeholder: the namespace is
//! reserved, so the feature is not available, draws nothing and sends nothing.
//! The PR that implements the area fills in this file (state, rendering, how it
//! reads events and replies, and its availability) and nothing shared.
use super::{Feature, FeatureId};

#[derive(Default)]
pub struct DiagnosticsFeature {}

impl Feature for DiagnosticsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Diagnostics
    }

    fn title(&self) -> &'static str {
        "Diagnostics"
    }
}
