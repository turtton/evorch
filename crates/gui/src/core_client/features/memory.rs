//! Desktop feature of the `memory` area (PR 13, 14). Placeholder: the namespace is
//! reserved, so the feature is not available, draws nothing and sends nothing.
//! The PR that implements the area fills in this file (state, rendering, how it
//! reads events and replies, and its availability) and nothing shared.
use super::{Feature, FeatureId};

#[derive(Default)]
pub struct MemoryFeature {}

impl Feature for MemoryFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Memory
    }

    fn title(&self) -> &'static str {
        "Memory"
    }
}
