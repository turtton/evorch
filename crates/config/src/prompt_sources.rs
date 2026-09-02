//! エージェントプロンプトの必要ソース集合を fail-closed で収集します。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::presets::PresetStore;
use crate::types::agents::{AgentsConfig, CATEGORY_NAMES, RoleBindingConfig};
use crate::{Config, ConfigError};

/// モデルファミリー名 (固定 6 種、generic を含む)。
const FAMILY_NAMES: &[&str] = &[
    "claude",
    "openai-reasoning",
    "gpt5",
    "gemini",
    "kimi",
    "generic",
];

/// 収集したエージェントプロンプトのソース集合。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentPromptSources {
    /// ロール別ベースライン (キー: ロール名)。
    pub role_baselines: BTreeMap<String, String>,
    /// モデルファミリー別セクション (キー: ファミリー名、generic を含む全 6 種)。
    pub family_sections: BTreeMap<String, String>,
    /// カテゴリ別オーバーレイ (キー: カテゴリ名、全 6 種)。
    pub category_overlays: BTreeMap<String, String>,
    /// agents 設定が参照する appendix プリセット本文 (キー: プリセット名)。
    pub appendices: BTreeMap<String, String>,
}

/// 設定からエージェントプロンプトに必要な全文ソースをすべて収集する。
///
/// ロール別ベースライン 4 種、モデルファミリー別セクション 6 種 (generic を
/// 含む)、カテゴリ別オーバーレイ 6 種、および agents 設定の `preset` 参照が
/// 指す appendix 本文を解決する。1 つでも解決できなければプロバイダに到達
/// 可能になる前にエラーで失敗する (fail-closed)。エラーにプリセット本文は
/// 含まれない。
///
/// `user_config_dir` はユーザー設定ディレクトリ (例: `~/.config/evorch`) で、
/// プリセット上書きはその `presets` サブディレクトリから解決する。
///
/// # Errors
/// 必要なプリセットが 1 つでも解決できなければ [`ConfigError`] を返す。
pub fn resolve_prompt_sources(
    config: &Config,
    user_config_dir: Option<&Path>,
) -> Result<AgentPromptSources, ConfigError> {
    let presets_dir = user_config_dir.map(|dir| dir.join("presets"));
    Ok(AgentPromptSources {
        role_baselines: resolve_role_baselines(&config.agents, presets_dir.as_deref())?,
        family_sections: resolve_families(presets_dir.as_deref())?,
        category_overlays: resolve_categories(presets_dir.as_deref())?,
        appendices: resolve_appendices(&config.agents, presets_dir.as_deref())?,
    })
}

/// ロールとバインディングの対応表を返す。
fn role_bindings(agents: &AgentsConfig) -> [(&'static str, &RoleBindingConfig); 4] {
    [
        ("orchestrator", &agents.orchestrator),
        ("explorer", &agents.explorer),
        ("worker", &agents.worker),
        ("reviewer", &agents.reviewer),
    ]
}

/// ロール別ベースライン (`role-<rolename>`) を解決する。
fn resolve_role_baselines(
    agents: &AgentsConfig,
    presets_dir: Option<&Path>,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut sources = BTreeMap::new();
    for (role, _) in role_bindings(agents) {
        let name = format!("role-{role}");
        let body = PresetStore::resolve(&name, presets_dir)?;
        sources.insert(role.to_string(), body);
    }
    Ok(sources)
}

/// モデルファミリー別セクション (`family-<key>`) を解決する。
fn resolve_families(presets_dir: Option<&Path>) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut sources = BTreeMap::new();
    for family in FAMILY_NAMES {
        let name = format!("family-{family}");
        let body = PresetStore::resolve(&name, presets_dir)?;
        sources.insert((*family).to_string(), body);
    }
    Ok(sources)
}

/// カテゴリ別オーバーレイ (`category-<name>`) を解決する。
fn resolve_categories(presets_dir: Option<&Path>) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut sources = BTreeMap::new();
    for category in CATEGORY_NAMES {
        let name = format!("category-{category}");
        let body = PresetStore::resolve(&name, presets_dir)?;
        sources.insert((*category).to_string(), body);
    }
    Ok(sources)
}

/// agents 設定が参照する appendix プリセット本文を重複なく解決する。
fn resolve_appendices(
    agents: &AgentsConfig,
    presets_dir: Option<&Path>,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut names = BTreeSet::new();
    for (_, binding) in role_bindings(agents) {
        if let Some(preset) = &binding.preset {
            names.insert(preset.clone());
        }
        for category in binding.categories.values() {
            if let Some(preset) = &category.preset {
                names.insert(preset.clone());
            }
        }
    }
    let mut sources = BTreeMap::new();
    for name in names {
        let body = PresetStore::resolve(&name, presets_dir)?;
        sources.insert(name, body);
    }
    Ok(sources)
}
