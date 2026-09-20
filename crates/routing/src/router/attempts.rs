use super::{LogicalModelId, ResolvedRoute, Router};

impl Router {
    /// Excludes a completed attempt from a request-local router, preventing fallback cycles.
    pub fn exclude_attempt(&mut self, logical: &LogicalModelId, failed: &ResolvedRoute) {
        for (name, candidates) in &mut self.routes {
            if let Some(index) = candidates.iter().position(|candidate| {
                candidate.profile == failed.profile
                    && self
                        .profiles
                        .get(&candidate.profile)
                        .is_some_and(|profile| {
                            candidate.model.as_deref().unwrap_or(&profile.default_model)
                                == failed.model_id
                        })
            }) && name == logical.as_str()
            {
                candidates.drain(..=index);
            }
            candidates.retain(|candidate| {
                candidate.profile != failed.profile
                    || self.profiles.get(&candidate.profile).is_none_or(|profile| {
                        candidate.model.as_deref().unwrap_or(&profile.default_model)
                            != failed.model_id
                    })
            });
        }
    }
}
