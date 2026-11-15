//! Desktop feature of the `browser` area (PR 16b). Placeholder: the namespace is
//! reserved, so the feature is not available, draws nothing and sends nothing.
//! The PR that implements the area fills in this file (state, rendering, how it
//! reads events and replies, and its availability) and nothing shared.
use super::{Feature, FeatureId};

#[derive(Default)]
pub struct BrowserFeature {}

impl Feature for BrowserFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Browser
    }

    fn title(&self) -> &'static str {
        "Browser"
    }
}
