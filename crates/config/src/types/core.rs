//! Standalone core and opt-in loopback Web delivery (ADR 0045 P7a).
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CoreConfig {
    /// Idle grace in seconds. Web delivery disables idle shutdown entirely.
    pub idle_grace_secs: u64,
}
impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            idle_grace_secs: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub enabled: bool,
    pub port: u16,
    /// Extra hosts without scheme or port, e.g. evorch.tailnet-name.ts.net.
    pub allowed_hosts: Vec<String>,
    /// Extra cross-origin frontends (scheme://host[:port]); not needed for same-origin Serve.
    pub allowed_origins: Vec<String>,
}
impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7878,
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
        }
    }
}
