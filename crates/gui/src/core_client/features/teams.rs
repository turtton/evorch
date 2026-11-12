//! Desktop feature of the `teams` area. A placeholder until that area's PR
//! implements it: it keeps no state, renders nothing and sends nothing, and
//! because it is not [`Availability::Implemented`](super::Availability) the
//! desktop does not offer it. Implement the [`Feature`] methods here and
//! return `Availability::Implemented { required }` from `availability`; see
//! [`super`] for the entry points and the rules.
use super::{Feature, FeatureId};

#[derive(Default)]
pub struct TeamsFeature {}

impl Feature for TeamsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Teams
    }

    fn title(&self) -> &'static str {
        "Teams"
    }
}
