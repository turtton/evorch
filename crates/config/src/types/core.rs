//! Standalone core and opt-in loopback Web delivery (ADR 0045 P7a).
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

pub const MAX_RESTART_TARGETS: usize = 32;
pub const MAX_RESTART_TARGET_ID_BYTES: usize = 64;
pub const MAX_RESTART_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RestartConfig {
    /// Fixed on acceptance. After capture this is a target, not a kill timer.
    pub timeout_secs: u64,
    /// Local files only. The ID current is reserved for the running ELF.
    pub targets: BTreeMap<String, RestartTargetConfig>,
}
impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 300,
            targets: BTreeMap::new(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RestartTargetConfig {
    pub path: PathBuf,
}
impl RestartConfig {
    /// None means the startup-recorded ELF, not PATH or a re-resolved wrapper.
    pub fn target_path(&self, id: &str) -> Result<Option<&std::path::Path>, crate::ConfigError> {
        self.validate()?;
        validate_restart_target_id(id)?;
        if id == "current" {
            return Ok(None);
        }
        self.targets
            .get(id)
            .map(|t| Some(t.path.as_path()))
            .ok_or_else(|| {
                crate::save::invalid_field("core.restart.targets", "unknown restart target ID")
            })
    }
    /// Filesystem identity is checked by the launcher at the three boundaries.
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        use crate::save::invalid_field;
        if !(1..=3600).contains(&self.timeout_secs) {
            return Err(invalid_field(
                "core.restart.timeout_secs",
                "must be in 1..=3600",
            ));
        }
        if self.targets.len() > MAX_RESTART_TARGETS {
            return Err(invalid_field(
                "core.restart.targets",
                "at most 32 configured targets",
            ));
        }
        for (id, target) in &self.targets {
            validate_restart_target_id(id)?;
            if id == "current" {
                return Err(invalid_field(
                    "core.restart.targets.current",
                    "current is reserved for the running ELF",
                ));
            }
            let field = format!("core.restart.targets.{id}.path");
            let Some(path) = target.path.to_str() else {
                return Err(invalid_field(&field, "must be a UTF-8 absolute path"));
            };
            if !target.path.is_absolute()
                || path.len() > MAX_RESTART_PATH_BYTES
                || path.chars().any(char::is_control)
            {
                return Err(invalid_field(
                    &field,
                    "must be an absolute path of at most 4096 bytes without control characters",
                ));
            }
        }
        Ok(())
    }
}
pub fn validate_restart_target_id(id: &str) -> Result<(), crate::ConfigError> {
    if id.is_empty()
        || id.len() > MAX_RESTART_TARGET_ID_BYTES
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(crate::save::invalid_field(
            "core.restart.targets",
            "ID must match [A-Za-z0-9_-]{1,64}",
        ));
    }
    Ok(())
}

/// Settings-save security boundary. Invoke under the write's revision guard,
/// against the effective full candidate after applying a patch. Missing targets
/// in a full replacement count as deletion; patches preserve omitted targets.
/// The later merger of PR10 and 4b-A must wire this into full/partial save APIs.
pub fn reject_restart_target_changes(
    current: &crate::Config,
    candidate: &crate::Config,
) -> Result<(), crate::ConfigError> {
    if current.core.restart.targets != candidate.core.restart.targets {
        return Err(crate::save::invalid_field(
            "core.restart.targets",
            "restart targets may only be changed in local config files",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CoreConfig {
    /// Idle grace in seconds. Web delivery disables idle shutdown entirely.
    pub idle_grace_secs: u64,
    pub restart: RestartConfig,
}
impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            idle_grace_secs: 60,
            restart: RestartConfig::default(),
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
