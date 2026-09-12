//! `agents` セクションの安全な書き戻し。

use std::path::Path;

use toml_edit::{Item, Table, value};

use crate::ConfigError;
use crate::types::agents::{
    AgentsConfig, CategoryBindingConfig, GenerationOverridesConfig, RoleBindingConfig,
};

/// agents セクションを置き換え、ロールとカテゴリのバインディングを保存する。
///
/// 対象セクション以外の TOML 表現 (コメントを含む) は保持し、保存結果は通常の
/// [`Config::load`] と同じ strict 検証を通過させる。
pub fn save_agent_bindings(path: &Path, agents: &AgentsConfig) -> Result<(), ConfigError> {
    let mut doc = super::save::read_document(path)?;
    let mut agents_table = Table::new();
    for (name, binding) in [
        ("orchestrator", &agents.orchestrator),
        ("explorer", &agents.explorer),
        ("worker", &agents.worker),
        ("reviewer", &agents.reviewer),
    ] {
        insert_binding(&mut agents_table, name, binding);
    }

    let mut roles = Table::new();
    for (name, binding) in [
        ("librarian", &agents.roles.librarian),
        ("planner", &agents.roles.planner),
        ("oracle", &agents.roles.oracle),
        ("multimodal_looker", &agents.roles.multimodal_looker),
    ] {
        insert_binding(&mut roles, name, binding);
    }
    if !roles.is_empty() {
        agents_table.insert("roles", Item::Table(roles));
    }
    doc.insert("agents", Item::Table(agents_table));
    super::save::write_document(path, &doc)
}

fn insert_binding(table: &mut Table, name: &str, binding: &RoleBindingConfig) {
    let binding_table = binding_table(binding);
    if !binding_table.is_empty() {
        table.insert(name, Item::Table(binding_table));
    }
}

fn binding_table(binding: &RoleBindingConfig) -> Table {
    let mut table = Table::new();
    if let Some(logical_model) = &binding.logical_model {
        table.insert("logical_model", value(logical_model.as_str()));
    }
    if let Some(preset) = &binding.preset {
        table.insert("preset", value(preset.as_str()));
    }
    insert_generation(&mut table, &binding.generation);
    if !binding.categories.is_empty() {
        let mut categories = Table::new();
        for (name, binding) in &binding.categories {
            categories.insert(name, Item::Table(category_table(binding)));
        }
        table.insert("categories", Item::Table(categories));
    }
    table
}

fn category_table(binding: &CategoryBindingConfig) -> Table {
    let mut table = Table::new();
    if let Some(logical_model) = &binding.logical_model {
        table.insert("logical_model", value(logical_model.as_str()));
    }
    if let Some(preset) = &binding.preset {
        table.insert("preset", value(preset.as_str()));
    }
    insert_generation(&mut table, &binding.generation);
    table
}

fn insert_generation(table: &mut Table, generation: &GenerationOverridesConfig) {
    let mut overrides = Table::new();
    if let Some(temperature) = generation.temperature {
        overrides.insert("temperature", value(temperature));
    }
    if let Some(top_p) = generation.top_p {
        overrides.insert("top_p", value(top_p));
    }
    if let Some(max_tokens) = generation.max_tokens {
        overrides.insert("max_tokens", value(i64::from(max_tokens)));
    }
    if let Some(reasoning_effort) = generation.reasoning_effort {
        let name = match reasoning_effort {
            crate::ReasoningEffortConfig::Low => "low",
            crate::ReasoningEffortConfig::Medium => "medium",
            crate::ReasoningEffortConfig::High => "high",
        };
        overrides.insert("reasoning_effort", value(name));
    }
    if !overrides.is_empty() {
        table.insert("generation", Item::Table(overrides));
    }
}
