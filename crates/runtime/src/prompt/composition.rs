//! config から production 用の [`SystemPromptCatalog`] を組み立てる composition
//! root (issue #49 / AC3, AC10)。
//!
//! [`resolve_prompt_sources`] による fail-closed なソース解決を最初に行い、その
//! 結果をカタログへ写像する。解決に失敗した場合はランタイム・モデル・provider
//! 呼び出しが存在する前にエラーで失敗する (AC10)。エラーの Display は識別子の
//! みを含み、プロンプト本文を一切含まない ([`PromptCompositionError`] は
//! `#[error(transparent)]` のみで構成する)。

use std::path::Path;

use agents::Role;
use config::{AgentPromptSources, Config, RoleBindingConfig, resolve_prompt_sources};

use crate::prompt::key_triggers::{AvailableAgent, AvailableSkill, triggers_from_availability};
use crate::prompt::{SystemPromptCatalog, SystemPromptCatalogError, default_role_triggers};

/// config から production 用 [`SystemPromptCatalog`] を組み立てる入力。
pub struct CatalogBuildInput<'a> {
    /// 解決対象の設定 (agents binding の preset 参照を含む)。
    pub config: &'a Config,
    /// ユーザー設定ディレクトリ (`presets` サブディレクトリから上書きを解決)。
    pub user_presets_dir: Option<&'a Path>,
    /// keyTriggers に載せる利用可能 agent のメタデータ (AC9)。
    pub available_agents: &'a [AvailableAgent],
    /// keyTriggers に載せる利用可能 skill のメタデータ (AC9)。
    pub available_skills: &'a [AvailableSkill],
}

/// カタログ組み立てのエラー。Display は識別子のみを含み、プロンプト本文を
/// 一切含まない。
#[derive(Debug, thiserror::Error)]
pub enum PromptCompositionError {
    /// プリセット解決に失敗した (fail-closed: カタログは構築されない)。
    #[error(transparent)]
    PresetResolution(#[from] config::ConfigError),
    /// カタログの必須部品が不完全だった。
    #[error(transparent)]
    Catalog(#[from] SystemPromptCatalogError),
}

/// 固定 4 ロールと agents 設定フィールドの対応表。
fn role_bindings(config: &Config) -> [(Role, &RoleBindingConfig); 4] {
    [
        (Role::Orchestrator, &config.agents.orchestrator),
        (Role::Explorer, &config.agents.explorer),
        (Role::Worker, &config.agents.worker),
        (Role::Reviewer, &config.agents.reviewer),
    ]
}

/// 解決済み appendix をプリセット名で引く。
///
/// 不変条件: `resolve_prompt_sources` は agents 設定が参照する全 appendix
/// プリセットを [`AgentPromptSources::appendices`] に収集して返す
/// (config クレート `resolve_appendices`)。欠落は内部不変条件の破壊のみで
/// 起こり、通常の設定値では到達しない。
fn appendix_body<'a>(sources: &'a AgentPromptSources, preset: &str) -> &'a str {
    sources
        .appendices
        .get(preset)
        .map(String::as_str)
        .expect("resolver は参照プリセットを必ず収集する")
}

/// config から production 用の [`SystemPromptCatalog`] を組み立てる (AC3, AC10)。
///
/// ソースからカタログへの写像規約:
///
/// - role baseline: config 側のキーは小文字ロール名のため、固定 4 ロールを
///   ループして小文字名で lookup する。欠落キーは builder の完全性検証で
///   型付きエラーになる。
/// - family section: config のキーは素のファミリー名、カタログキーは
///   `family-` 接頭辞付きのため `format!("family-{key}")` で写像する。
/// - category overlay: カテゴリ名をそのまま引き渡す。
/// - appendix: ロール直下の `preset` 参照をロールレベル appendix へ、
///   `categories.<name>.preset` 参照をカテゴリスコープ appendix へ登録する
///   (カテゴリスコープは `system_prompt_for` でロールレベルに優先する)。
///
/// # Errors
/// プリセット解決またはカタログ完全性検証の失敗を [`PromptCompositionError`]
/// で返す。
pub fn build_catalog(
    input: &CatalogBuildInput<'_>,
) -> Result<SystemPromptCatalog, PromptCompositionError> {
    let sources = resolve_prompt_sources(input.config, input.user_presets_dir)?;
    let mut builder = SystemPromptCatalog::builder();
    for (role, binding) in role_bindings(input.config) {
        let baseline_key = role.name().to_lowercase();
        if let Some(body) = sources.role_baselines.get(baseline_key.as_str()) {
            builder = builder.role_baseline(role, body.as_str());
        }
        if let Some(preset) = &binding.preset {
            builder = builder.appendix(role, appendix_body(&sources, preset));
        }
        for (category, category_binding) in &binding.categories {
            if let Some(preset) = &category_binding.preset {
                builder = builder.category_appendix(
                    role,
                    category.as_str(),
                    appendix_body(&sources, preset),
                );
            }
        }
    }
    for (family_key, body) in &sources.family_sections {
        builder = builder.family_section(format!("family-{family_key}"), body.as_str());
    }
    for (category, body) in &sources.category_overlays {
        builder = builder.category_overlay(category.as_str(), body.as_str());
    }
    // 既定ロール トリガーに利用可能 agent/skill のメタデータを合成する。
    // 重複排除とソートは renderer が担当するため、ここでは順序を気にしない。
    let mut triggers = default_role_triggers();
    triggers.extend(triggers_from_availability(
        input.available_agents,
        input.available_skills,
    ));
    builder
        .triggers(triggers)
        .build()
        .map_err(PromptCompositionError::from)
}
