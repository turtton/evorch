//! Desktop feature of the `maintenance` area (PR 17d). Placeholder: the namespace is
//! reserved, so the feature is not available, draws nothing and sends nothing.
//! The PR that implements the area fills in this file (state, rendering, how it
//! reads events and replies, and its availability) and nothing shared.
use super::{Feature, FeatureId};

#[derive(Default)]
pub struct MaintenanceFeature {}

impl Feature for MaintenanceFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Maintenance
    }

    fn title(&self) -> &'static str {
        "Maintenance"
    }
}
