//! ロール構成プロファイル (ロール割り当てとルーティングの名前付きセット) を定義します。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{AgentsConfig, Config, RoutingConfig};

/// トップレベルの `[agents]` / `[routing]` を指す予約済みプロファイル名。
pub const DEFAULT_ROLE_PROFILE: &str = "default";

/// 名前付きのロール構成プロファイル。
///
/// トップレベルの `[agents]` / `[routing]` と同じ形で、プロジェクトは
/// `role_profile = "<name>"` によっていずれかを選択する。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RoleProfileConfig {
    /// ロール別エージェントバインディング。
    pub agents: AgentsConfig,
    /// ルーティング設定。
    pub routing: RoutingConfig,
}

/// プロファイル名が `[a-z0-9_-]{1,64}` を満たすか。
pub fn is_valid_role_profile_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

impl Config {
    /// 選択可能なプロファイル名。先頭は常に [`DEFAULT_ROLE_PROFILE`]。
    pub fn role_profile_names(&self) -> Vec<String> {
        std::iter::once(DEFAULT_ROLE_PROFILE.to_owned())
            .chain(self.role_profiles.keys().cloned())
            .collect()
    }

    /// 指定プロファイルのロール構成。`default` と `None` はトップレベルを返す。
    pub fn role_profile_config(&self, name: Option<&str>) -> Option<RoleProfileConfig> {
        match name {
            None | Some(DEFAULT_ROLE_PROFILE) => Some(RoleProfileConfig {
                agents: self.agents.clone(),
                routing: self.routing.clone(),
            }),
            Some(name) => self.role_profiles.get(name).cloned(),
        }
    }

    /// 選択中のプロファイル名。未選択・未知の名前は `default` として扱う。
    pub fn active_role_profile(&self) -> &str {
        match self.role_profile.as_deref() {
            Some(name) if self.role_profiles.contains_key(name) => name,
            _ => DEFAULT_ROLE_PROFILE,
        }
    }

    /// `role_profile` の選択を実効の `agents` / `routing` に反映する。
    ///
    /// 未知の名前は警告してトップレベル (`default`) のまま扱う。
    pub(crate) fn apply_role_profile(&mut self) {
        let Some(name) = self.role_profile.as_deref() else {
            return;
        };
        if name == DEFAULT_ROLE_PROFILE {
            return;
        }
        match self.role_profiles.get(name) {
            Some(profile) => {
                self.agents = profile.agents.clone();
                self.routing = profile.routing.clone();
            }
            None => {
                tracing::warn!(role_profile = %name, "unknown role profile; using default");
            }
        }
    }
}
