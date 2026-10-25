//! モデルへ提示するファイル編集ツールの選択を定義します。
//!
//! 解決順は model entry の `edit_tool` > プロファイルの `edit_tool` >
//! [`EditToolConfig::default_for`] による既定判定です。設定型
//! ([`super::ProviderProfileConfig`]) と routing へ渡す [`EditToolSelection`] は
//! 同じ解決関数を共有します。

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::provider::{ProviderTypeConfig, parse_model_speed};

/// モデルへ提示するファイル編集ツール (設定ファイル上の表現)。
///
/// シリアライズ識別子はケバブケース (`apply-patch` / `edit`) です。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum EditToolConfig {
    /// Codex 準拠の `apply_patch` (patch 形式でファイルを追加・更新・削除する)。
    ApplyPatch,
    /// 文字列置換によるファイル編集 (`edit`)。
    Edit,
}

impl EditToolConfig {
    /// 設定ファイルで受理するすべての値。
    pub const ALL: [Self; 2] = [Self::ApplyPatch, Self::Edit];

    /// 設定ファイル上のシリアライズ識別子を返す。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApplyPatch => "apply-patch",
            Self::Edit => "edit",
        }
    }

    /// 設定ファイル上の識別子を解釈する。受理しない値は `None`。
    pub fn from_config_str(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.as_str() == value)
    }

    /// 明示指定がないときの編集ツール。
    ///
    /// OpenAI API と OpenAI Codex で、`base_model_id` が `gpt-<N>` (N≥5) のときだけ
    /// apply-patch を選ぶ。`codex-mini-latest`・`o3`・`gpt-oss-120b` のように
    /// [`gpt_generation`] が世代を読めない ID や、他のプロバイダ種別は edit。
    pub fn default_for(provider_type: ProviderTypeConfig, base_model_id: &str) -> Self {
        let openai = matches!(
            provider_type,
            ProviderTypeConfig::OpenAi | ProviderTypeConfig::OpenAiCodex
        );
        if openai && gpt_generation(base_model_id).is_some_and(|generation| generation >= 5) {
            Self::ApplyPatch
        } else {
            Self::Edit
        }
    }
}

/// `gpt-<N>` 形式のモデル ID から世代 N を読む。
///
/// `gpt-` の直後に続く数字列 (小数点より前) だけを読む: `gpt-5.1-codex` は 5、
/// `gpt-6-astra` は 6。`gpt-` で始まらない ID、直後が数字でない ID (`gpt-oss-120b`)
/// は `None`。runtime の model family 判定 (`prompt::family::classify`) と同じく
/// 大文字小文字を区別しないため、`GPT-5` も 5。
pub fn gpt_generation(model_id: &str) -> Option<u32> {
    let model_id = model_id.to_ascii_lowercase();
    let rest = model_id.strip_prefix("gpt-")?;
    let digits = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..digits].parse().ok()
}

/// プロバイダプロファイルで明示された編集ツールの上書き。
///
/// routing の検証済みプロファイルへ渡す値で、プロバイダ種別は持たない。
/// 種別は [`EditToolSelection::for_model`] の呼び出し側が渡すため、既定値
/// (上書きなし) はどの種別でも既定判定になる。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditToolSelection {
    /// プロファイル全体の `edit_tool`。
    pub profile: Option<EditToolConfig>,
    /// `edit_tool` を指定したモデルエントリ (モデル ID ごと)。
    pub models: BTreeMap<String, EditToolConfig>,
}

impl EditToolSelection {
    /// `model_id` (プロファイル名を除いたモデル ID) に提示する編集ツールを返す。
    pub fn for_model(&self, provider_type: ProviderTypeConfig, model_id: &str) -> EditToolConfig {
        resolve_edit_tool(provider_type, self.profile, model_id, |id| {
            self.models.get(id).copied()
        })
    }
}

/// model entry > プロファイル > 既定判定の順に編集ツールを決める。
///
/// model entry は ID の完全一致を先に引き、なければ `+fast` を除いた base ID で
/// 引き直す。既定判定にも base ID を渡す。
pub(crate) fn resolve_edit_tool(
    provider_type: ProviderTypeConfig,
    profile: Option<EditToolConfig>,
    model_id: &str,
    model_override: impl Fn(&str) -> Option<EditToolConfig>,
) -> EditToolConfig {
    let (base, _) = parse_model_speed(model_id);
    model_override(model_id)
        .or_else(|| model_override(base))
        .or(profile)
        .unwrap_or_else(|| EditToolConfig::default_for(provider_type, base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_identifiers_match_serde() {
        for tool in EditToolConfig::ALL {
            assert_eq!(
                serde_json::to_value(tool).expect("edit tool serializes"),
                tool.as_str()
            );
            assert_eq!(EditToolConfig::from_config_str(tool.as_str()), Some(tool));
        }
        assert_eq!(EditToolConfig::from_config_str("patch"), None);
        assert_eq!(EditToolConfig::from_config_str("apply_patch"), None);
    }

    #[test]
    fn gpt_generation_ignores_overflowing_and_non_numeric_versions() {
        assert_eq!(gpt_generation("gpt-10-pro"), Some(10));
        assert_eq!(gpt_generation("gpt-4o"), Some(4));
        assert_eq!(gpt_generation("gpt-99999999999"), None);
        assert_eq!(gpt_generation("gpt-.5"), None);
        assert_eq!(gpt_generation("openai/gpt-5"), None);
        // 先頭 4 byte が文字境界でない ID でも panic しない。
        assert_eq!(gpt_generation("gpté-5"), None);
    }
}
