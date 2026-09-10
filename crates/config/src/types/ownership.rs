use std::num::NonZeroU64;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OwnershipConfig {
    pub heartbeat_ms: NonZeroU64,
    pub lease_ms: NonZeroU64,
    pub grace_ms: NonZeroU64,
}

impl Default for OwnershipConfig {
    fn default() -> Self {
        Self {
            heartbeat_ms: NonZeroU64::MIN.saturating_add(999),
            lease_ms: NonZeroU64::MIN.saturating_add(4_999),
            grace_ms: NonZeroU64::MIN.saturating_add(9_999),
        }
    }
}
