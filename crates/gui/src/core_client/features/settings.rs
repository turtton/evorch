//! Desktop feature of the `settings` area (PR 10). Placeholder: the namespace is
//! reserved, so the feature is not available, draws nothing and sends nothing.
//! The PR that implements the area fills in this file (state, rendering, how it
//! reads events and replies, and its availability) and nothing shared.
use super::{Feature, FeatureId};

#[derive(Default)]
pub struct SettingsFeature {}

impl Feature for SettingsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Settings
    }

    fn title(&self) -> &'static str {
        "Settings"
    }
}
