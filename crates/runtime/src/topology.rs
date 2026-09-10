use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CoordinationTopology {
    #[default]
    Single,
    DynamicTeam {
        max_workers: u8,
    },
}

impl CoordinationTopology {
    pub const fn worker_limit(self) -> Option<u8> {
        match self {
            Self::Single => None,
            Self::DynamicTeam { max_workers } => Some(match max_workers {
                0 => 1,
                1..=3 => max_workers,
                4..=u8::MAX => 3,
            }),
        }
    }

    pub const fn from_config(config: &config::types::team::TeamConfig) -> Self {
        if config.enabled {
            Self::DynamicTeam {
                max_workers: config.max_workers,
            }
        } else {
            Self::Single
        }
    }
}
