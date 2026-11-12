//! Desktop feature of the `mcp` area. A placeholder until that area's PR
//! implements it: it keeps no state, renders nothing and sends nothing, and
//! because it is not [`Availability::Implemented`](super::Availability) the
//! desktop does not offer it. Implement the [`Feature`] methods here and
//! return `Availability::Implemented { required }` from `availability`; see
//! [`super`] for the entry points and the rules.
use super::{Feature, FeatureId};

#[derive(Default)]
pub struct McpFeature {}

impl Feature for McpFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Mcp
    }

    fn title(&self) -> &'static str {
        "MCP"
    }
}
