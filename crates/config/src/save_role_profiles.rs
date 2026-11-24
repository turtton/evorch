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
    save_role_profiles_bindings(
        path,
        &[RoleProfileBindings {
            profile,
            routing,
            agents,
        }],
    )
}

/// Save final agents / routing edits after checking all layers and role profiles.
/// No file is opened for writing until model-kind and provider constraints pass.
///
/// # Errors
/// Fails like [`save_role_profile_bindings`], or on incompatible effective chains.
pub fn save_role_profile_bindings_with_options(
    path: &Path,
    profile: Option<&str>,
    routing: Option<&RoutingConfig>,
    agents: Option<&AgentsConfig>,
    options: &crate::LoadOptions,
) -> Result<(), ConfigError> {
    let doc = bindings_document(
        path,
        &[RoleProfileBindings {
            profile,
            routing,
            agents,
        }],
    )?;
    crate::save::write_document_in_scope(
        path,
        &doc,
        crate::save_context::SaveScope::Layered(options),
    )
}

/// One profile's sections for [`save_role_profiles_bindings`].
#[derive(Debug, Clone, Copy)]
pub struct RoleProfileBindings<'a> {
    /// `None` and `"default"` are the top-level sections.
    pub profile: Option<&'a str>,
    pub routing: Option<&'a RoutingConfig>,
    pub agents: Option<&'a AgentsConfig>,
}

/// Replace several profiles' agents / routing in one atomic write, as
/// [`save_role_profile_bindings`] does for one.
///
/// # Errors
/// Fails like [`save_role_profile_bindings`]; nothing is written then.
pub fn save_role_profiles_bindings(
    path: &Path,
    changes: &[RoleProfileBindings<'_>],
) -> Result<(), ConfigError> {
    crate::save::write_document(path, &bindings_document(path, changes)?)
}

/// The file's text after [`save_role_profiles_bindings`], validated as saving
/// would but not written.
///
/// # Errors
/// Fails like [`save_role_profiles_bindings`].
pub fn preview_role_profiles_bindings(
    path: &Path,
    changes: &[RoleProfileBindings<'_>],
) -> Result<String, ConfigError> {
    crate::StandaloneSaveContext.preview_role_profiles_bindings(path, changes)
}

pub(crate) fn bindings_document(
    path: &Path,
    changes: &[RoleProfileBindings<'_>],
) -> Result<DocumentMut, ConfigError> {
    let mut doc = crate::save::read_document(path)?;
    apply_bindings(&mut doc, changes)?;
    Ok(doc)
}

fn apply_bindings(
    doc: &mut DocumentMut,
    changes: &[RoleProfileBindings<'_>],
) -> Result<(), ConfigError> {
    for change in changes {
        let target = target_table(doc, change.profile, false)?;
        if let Some(routing) = change.routing {
            target.insert("routing", Item::Table(routing_document_table(routing)));
        }
        if let Some(agents) = change.agents {
            target.insert("agents", Item::Table(agents_document_table(agents)));
        }
    }
    Ok(())
}

fn apply_role_profile(
    doc: &mut DocumentMut,
    name: &str,
    profile: &RoleProfileConfig,
) -> Result<(), ConfigError> {
    validate_named(name)?;
    let target = target_table(doc, Some(name), true)?;
    target.insert(
        "routing",
        Item::Table(routing_document_table(&profile.routing)),
    );
    target.insert(
        "agents",
        Item::Table(agents_document_table(&profile.agents)),
    );
    Ok(())
}

fn apply_delete_role_profile(doc: &mut DocumentMut, name: &str) -> Result<(), ConfigError> {
    validate_named(name)?;
    if let Some(profiles) = doc
        .get_mut("role_profiles")
        .and_then(Item::as_table_like_mut)
    {
        profiles.remove(name);
        if profiles.is_empty() {
            doc.remove("role_profiles");
        }
    }
    Ok(())
}

fn edited_text(
    previous: &str,
    edit: impl FnOnce(&mut DocumentMut) -> Result<(), ConfigError>,
) -> Result<String, ConfigError> {
    let mut doc = crate::save::parse_document(previous, "config.toml")?;
    edit(&mut doc)?;
    crate::save::checked_standalone_text(previous, &doc)
}

/// [`save_role_profiles_bindings`] applied to the file text `previous` instead of a path.
///
/// # Errors
/// Fails like [`save_role_profiles_bindings`] with a standalone check.
pub fn role_profiles_bindings_text(
    previous: &str,
    changes: &[RoleProfileBindings<'_>],
) -> Result<String, ConfigError> {
    edited_text(previous, |doc| apply_bindings(doc, changes))
}

/// [`save_role_profile`] applied to the file text `previous` instead of a path.
///
/// # Errors
/// Fails like [`save_role_profile`].
pub fn role_profile_text(
    previous: &str,
    name: &str,
    profile: &RoleProfileConfig,
) -> Result<String, ConfigError> {
    edited_text(previous, |doc| apply_role_profile(doc, name, profile))
}

/// [`delete_role_profile`] applied to the file text `previous` instead of a path.
///
/// # Errors
/// Fails like [`delete_role_profile`].
pub fn without_role_profile_text(previous: &str, name: &str) -> Result<String, ConfigError> {
    edited_text(previous, |doc| apply_delete_role_profile(doc, name))
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
    save_role_profile_in_scope(
        path,
        name,
        profile,
        crate::save_context::SaveScope::Standalone,
    )
}

pub(crate) fn save_role_profile_in_scope(
    path: &Path,
    name: &str,
    profile: &RoleProfileConfig,
    scope: crate::save_context::SaveScope<'_>,
) -> Result<(), ConfigError> {
    validate_named(name)?;
    let mut doc = crate::save::read_document(path)?;
    apply_role_profile(&mut doc, name, profile)?;
    crate::save::write_document_in_scope(path, &doc, scope)
}

/// 名前付きプロファイルを削除する。存在しない場合は何もしない。
///
/// # Errors
/// 名前が `default` の場合、または保存に失敗した場合にエラーを返す。
pub fn delete_role_profile(path: &Path, name: &str) -> Result<(), ConfigError> {
    delete_role_profile_in_scope(path, name, crate::save_context::SaveScope::Standalone)
}

pub(crate) fn delete_role_profile_in_scope(
    path: &Path,
    name: &str,
    scope: crate::save_context::SaveScope<'_>,
) -> Result<(), ConfigError> {
    validate_named(name)?;
    let mut doc = crate::save::read_document(path)?;
    apply_delete_role_profile(&mut doc, name)?;
    crate::save::write_document_in_scope(path, &doc, scope)
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
    save_project_role_profile_in_scope(
        project_dir,
        profile,
        crate::save_context::SaveScope::Standalone,
    )
}

pub(crate) fn save_project_role_profile_in_scope(
    project_dir: &Path,
    profile: Option<&str>,
    scope: crate::save_context::SaveScope<'_>,
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
    crate::save::write_document_in_scope(&path, &doc, scope)
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

#[cfg(test)]
mod text_tests {
    use super::*;
    use crate::RouteCandidateConfig;

    const PREVIOUS: &str = r#"version = 3
# kept as written
[core]
idle_grace_secs = 123

[core.restart.targets.next]
path = "/local/core"

[providers.local]
type = "openai-compatible"
base_url = "http://localhost:1/v1"
api_key_env = "KEY"
models = ["m"]
default_model = "m"
"#;

    fn routing() -> RoutingConfig {
        let mut routing = RoutingConfig::default();
        routing.routes.insert(
            "fast".into(),
            vec![RouteCandidateConfig {
                profile: "local".into(),
                model: None,
                auto: false,
            }],
        );
        routing
    }

    // Given: a file with a comment, an unrelated section and restart targets
    // When: its routing is replaced through the text function and through the path function
    // Then: both give the same bytes, and the comment, the section and the targets survive
    #[test]
    fn the_text_and_the_path_save_give_the_same_bytes_and_keep_the_rest() {
        let routing = routing();
        let changes = [RoleProfileBindings {
            profile: None,
            routing: Some(&routing),
            agents: None,
        }];
        let text = role_profiles_bindings_text(PREVIOUS, &changes).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, PREVIOUS).unwrap();
        save_role_profiles_bindings(&path, &changes).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        for kept in ["# kept as written", "idle_grace_secs = 123", "/local/core"] {
            assert!(text.contains(kept), "{kept} in {text}");
        }
        assert!(text.contains("fast"), "{text}");
    }

    // Given: a file with restart targets
    // When: a profile is created and deleted by text, and a document without the targets is checked
    // Then: the profile edits keep the targets and pass; the document that drops them is refused
    #[test]
    fn profile_edits_keep_the_restart_targets_and_dropping_them_is_refused() {
        let profile = RoleProfileConfig {
            routing: routing(),
            ..RoleProfileConfig::default()
        };
        let created = role_profile_text(PREVIOUS, "cheap", &profile).unwrap();
        assert!(created.contains("[role_profiles.cheap"), "{created}");
        let deleted = without_role_profile_text(&created, "cheap").unwrap();
        assert!(!deleted.contains("role_profiles"), "{deleted}");
        assert!(deleted.contains("/local/core"));
        assert!(role_profile_text(PREVIOUS, "default", &profile).is_err());

        let without_targets = crate::save::parse_document("version = 3\n", "config.toml").unwrap();
        let refused = crate::save::checked_standalone_text(PREVIOUS, &without_targets).unwrap_err();
        assert!(
            refused.to_string().contains("core.restart.targets"),
            "{refused}"
        );
    }
}
