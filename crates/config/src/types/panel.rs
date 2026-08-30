//! パネル UI に関する設定型を定義します。

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// パネル UI の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PanelConfig {
    /// レイアウト名。
    pub layout: String,
    /// キーバインド (アクション名からキーへのマップ)。
    pub keybinds: BTreeMap<String, String>,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            layout: "default".to_string(),
            keybinds: BTreeMap::new(),
        }
    }
}
