//! Desktop feature of the `auth` area (PR 10). Placeholder: the namespace is
//! reserved, so the feature is not available, draws nothing and sends nothing.
//! The PR that implements the area fills in this file (state, rendering, how it
//! reads events and replies, and its availability) and nothing shared.
use super::{Feature, FeatureId};

#[derive(Default)]
pub struct AuthFeature {}

impl Feature for AuthFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Auth
    }

    fn title(&self) -> &'static str {
        "Authentication"
    }
}
