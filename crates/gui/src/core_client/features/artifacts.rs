//! Desktop feature of the `artifacts` area (PR 16b). Placeholder: the namespace is
//! reserved, so the feature is not available, draws nothing and sends nothing.
//! The PR that implements the area fills in this file (state, rendering, how it
//! reads events and replies, and its availability) and nothing shared.
use super::{Feature, FeatureId};

#[derive(Default)]
pub struct ArtifactsFeature {}

impl Feature for ArtifactsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Artifacts
    }

    fn title(&self) -> &'static str {
        "Artifacts"
    }
}
