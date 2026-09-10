use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TeamConfig {
    pub enabled: bool,
    #[serde(deserialize_with = "worker_limit")]
    #[schemars(range(min = 1, max = 3))]
    pub max_workers: u8,
}

impl Default for TeamConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_workers: 3,
        }
    }
}

fn worker_limit<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    let value = u8::deserialize(deserializer)?;
    match value {
        1..=3 => Ok(value),
        _ => Err(serde::de::Error::custom(
            "team.max_workers must be between 1 and 3",
        )),
    }
}
