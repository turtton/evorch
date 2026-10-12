//! `routing` セクションの安全な書き戻し。

use std::path::Path;

use toml_edit::{ArrayOfTables, Item, Table, value};

use crate::{AgentsConfig, ConfigError, RoutingConfig};

/// 他セクションのコメントを保持し、候補をフォールバック順で保存する。
pub fn save_routing(path: &Path, routing: &RoutingConfig) -> Result<(), ConfigError> {
    let mut doc = super::save::read_document(path)?;
    doc.insert("routing", Item::Table(routing_document_table(routing)));
    super::save::write_document(path, &doc)
}

/// routing セクションと agents セクションを同一ドキュメントで置換し、1回の atomic write で保存する。
///
/// GUI の route rename 保存で使用する。agents 側の参照更新
/// ([`rename_route_refs`](crate::types::agents::rename_route_refs)) は呼び出し側が適用済みの値を渡すこと。
/// 対象セクション以外の TOML 表現 (コメントを含む) は保持する。
pub fn save_routing_and_agents(
    path: &Path,
    routing: &RoutingConfig,
    agents: &AgentsConfig,
) -> Result<(), ConfigError> {
    let mut doc = super::save::read_document(path)?;
    doc.insert("routing", Item::Table(routing_document_table(routing)));
    doc.insert(
        "agents",
        Item::Table(super::save_agents::agents_document_table(agents)),
    );
    super::save::write_document(path, &doc)
}

pub(crate) fn routing_document_table(routing: &RoutingConfig) -> Table {
    let mut routes = Table::new();
    for (name, candidates) in &routing.routes {
        let mut tables = ArrayOfTables::new();
        for candidate in candidates {
            let mut table = Table::new();
            table.insert("profile", value(candidate.profile.as_str()));
            if let Some(model) = &candidate.model {
                table.insert("model", value(model.as_str()));
            }
            if candidate.auto {
                table.insert("auto", value(true));
            }
            tables.push(table);
        }
        if candidates.is_empty() {
            routes.insert(name, value(toml_edit::Array::new()));
        } else {
            routes.insert(name, Item::ArrayOfTables(tables));
        }
    }
    let mut table = Table::new();
    table.insert("routes", Item::Table(routes));
    if !routing.cooldown.is_default() {
        let mut cooldown = Table::new();
        cooldown.insert(
            "quota_secs",
            value(saturating_i64(routing.cooldown.quota_secs)),
        );
        cooldown.insert(
            "rate_limit_secs",
            value(saturating_i64(routing.cooldown.rate_limit_secs)),
        );
        table.insert("cooldown", Item::Table(cooldown));
    }
    if !routing.fallback.is_default() {
        let mut fallback = Table::new();
        fallback.insert(
            "max_retry_after_wait_secs",
            value(saturating_i64(routing.fallback.max_retry_after_wait_secs)),
        );
        table.insert("fallback", Item::Table(fallback));
    }
    table
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
