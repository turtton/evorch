//! ロール構成プロファイルの安全な書き戻し。

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, value};

use crate::save_agents::agents_document_table;
use crate::save_routing::routing_document_table;
use crate::types::role_profile::{DEFAULT_ROLE_PROFILE, is_valid_role_profile_name};
use crate::{AgentsConfig, ConfigError, RoleProfileConfig, RoutingConfig};

/// 指定プロファイルの agents / routing を置き換えて保存する。
///
/// `None` と `"default"` はトップレベルの `[agents]` / `[routing]` を対象にする。
/// `None` を渡した項目は変更しない。対象以外の TOML 表現 (コメントを含む) は保持する。
///
/// # Errors
/// プロファイル名が不正、または名前付きプロファイルが存在しない場合、
/// 保存結果が strict 検証を通らない場合にエラーを返す。
pub fn save_role_profile_bindings(
    path: &Path,
    profile: Option<&str>,
    routing: Option<&RoutingConfig>,
    agents: Option<&AgentsConfig>,
) -> Result<(), ConfigError> {
    let mut doc = crate::save::read_document(path)?;
    let target = target_table(&mut doc, profile, false)?;
    if let Some(routing) = routing {
        target.insert("routing", Item::Table(routing_document_table(routing)));
    }
    if let Some(agents) = agents {
        target.insert("agents", Item::Table(agents_document_table(agents)));
    }
    crate::save::write_document(path, &doc)
}

/// 名前付きプロファイルを作成 (または置換) する。
///
/// # Errors
/// 名前が `default` または `[a-z0-9_-]{1,64}` 外の場合、保存結果が strict 検証を
/// 通らない場合にエラーを返す。
pub fn save_role_profile(
    path: &Path,
    name: &str,
    profile: &RoleProfileConfig,
) -> Result<(), ConfigError> {
    validate_named(name)?;
    let mut doc = crate::save::read_document(path)?;
    let target = target_table(&mut doc, Some(name), true)?;
    target.insert(
        "routing",
        Item::Table(routing_document_table(&profile.routing)),
    );
    target.insert(
        "agents",
        Item::Table(agents_document_table(&profile.agents)),
    );
    crate::save::write_document(path, &doc)
}

/// 名前付きプロファイルを削除する。存在しない場合は何もしない。
///
/// # Errors
/// 名前が `default` の場合、または保存に失敗した場合にエラーを返す。
pub fn delete_role_profile(path: &Path, name: &str) -> Result<(), ConfigError> {
    validate_named(name)?;
    let mut doc = crate::save::read_document(path)?;
    let Some(profiles) = doc
        .get_mut("role_profiles")
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(());
    };
    if profiles.remove(name).is_none() {
        return Ok(());
    }
    if profiles.is_empty() {
        doc.remove("role_profiles");
    }
    crate::save::write_document(path, &doc)
}

/// プロジェクト設定 (`<project>/.evorch/config.toml`) の `role_profile` を設定・削除する。
///
/// `None` と `"default"` はキーを削除する。ディレクトリとファイルは必要なら作成する。
///
/// # Errors
/// 名前が不正な場合、または読み書きに失敗した場合にエラーを返す。
pub fn save_project_role_profile(
    project_dir: &Path,
    profile: Option<&str>,
) -> Result<(), ConfigError> {
    let path = crate::project_main_config_path(project_dir);
    let profile = profile.filter(|name| *name != DEFAULT_ROLE_PROFILE);
    if let Some(name) = profile {
        validate_named(name)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut doc = crate::save::read_document(&path)?;
    match profile {
        Some(name) => {
            doc.insert("role_profile", value(name));
        }
        None => {
            doc.remove("role_profile");
        }
    }
    crate::save::write_document(&path, &doc)
}

fn validate_named(name: &str) -> Result<(), ConfigError> {
    if name == DEFAULT_ROLE_PROFILE || !is_valid_role_profile_name(name) {
        return Err(crate::save::invalid_field(
            &format!("role_profiles.{name}"),
            "role profile names must match [a-z0-9_-]{1,64} and must not be \"default\"",
        ));
    }
    Ok(())
}

/// 保存先テーブル。名前付きプロファイルは `create` が偽なら既存のものに限る。
fn target_table<'a>(
    doc: &'a mut DocumentMut,
    profile: Option<&str>,
    create: bool,
) -> Result<&'a mut Table, ConfigError> {
    let name = match profile {
        None | Some(DEFAULT_ROLE_PROFILE) => return Ok(doc.as_table_mut()),
        Some(name) => name,
    };
    let missing = || {
        crate::save::invalid_field(
            &format!("role_profiles.{name}"),
            "role profile does not exist",
        )
    };
    let profiles = doc
        .entry("role_profiles")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .ok_or_else(|| crate::save::invalid_field("role_profiles", "must be a table"))?;
    if !create && !profiles.contains_key(name) {
        return Err(missing());
    }
    profiles
        .entry(name)
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .ok_or_else(missing)
}
