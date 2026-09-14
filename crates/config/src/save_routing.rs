//! `routing` セクションの安全な書き戻し。

use std::path::Path;

use toml_edit::{ArrayOfTables, Item, Table, value};

use crate::{ConfigError, RoutingConfig};

/// 他セクションのコメントを保持し、候補をフォールバック順で保存する。
pub fn save_routing(path: &Path, routing: &RoutingConfig) -> Result<(), ConfigError> {
    let mut doc = super::save::read_document(path)?;
    let mut routes = Table::new();
    for (name, candidates) in &routing.routes {
        let mut tables = ArrayOfTables::new();
        for candidate in candidates {
            let mut table = Table::new();
            table.insert("profile", value(candidate.profile.as_str()));
            if let Some(model) = &candidate.model {
                table.insert("model", value(model.as_str()));
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
    doc.insert("routing", Item::Table(table));
    super::save::write_document(path, &doc)
}
