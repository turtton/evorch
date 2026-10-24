use std::path::{Path, PathBuf};
use std::sync::Arc;

use config::{Config, LoadOptions, RoleProfileBindings};
use serde::Deserialize;
use serde_json::{Value, json};
use tools::{Permissions, Tool, ToolError, ToolExecutionContext, ToolResult};

use super::{Shared, parse};
use crate::metrics;
use crate::runner;

const DIFF_CHARS: usize = 6_000;
const SECTIONS: [&str; 2] = ["agents", "routing"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyArgs {
    overlay: String,
}

/// Where the overlay writes: `None` for the top level or a role profile's name,
/// with the sections it replaces.
type Targets = Vec<(Option<String>, Vec<&'static str>)>;

fn targets(overlay: &toml::Value, current: &Config) -> Result<Targets, String> {
    let refuse = |key: &str| {
        format!(
            "`{key}` cannot be applied: only agents, routing and role_profiles.<name>.agents / routing can, through the settings savers"
        )
    };
    let sections = |table: &toml::Table, prefix: &str| {
        table
            .keys()
            .map(|key| {
                SECTIONS
                    .iter()
                    .copied()
                    .find(|section| section == key)
                    .ok_or_else(|| refuse(&format!("{prefix}{key}")))
            })
            .collect::<Result<Vec<_>, _>>()
    };
    let Some(table) = overlay.as_table() else {
        return Err("the overlay must be a TOML table".into());
    };
    let mut top = toml::Table::new();
    let mut targets = Vec::new();
    for (key, value) in table {
        if key == "role_profiles" {
            let profiles = value.as_table().ok_or_else(|| refuse(key))?;
            for (name, profile) in profiles {
                if !current.role_profiles.contains_key(name) {
                    return Err(format!(
                        "role profile {name:?} does not exist in the user configuration"
                    ));
                }
                let profile = profile
                    .as_table()
                    .ok_or_else(|| refuse(&format!("role_profiles.{name}")))?;
                targets.push((
                    Some(name.clone()),
                    sections(profile, &format!("role_profiles.{name}."))?,
                ));
            }
        } else {
            top.insert(key.clone(), value.clone());
        }
    }
    if !top.is_empty() {
        targets.insert(0, (None, sections(&top, "")?));
    }
    Ok(targets)
}

fn bindings<'a>(config: &'a Config, targets: &'a Targets) -> Vec<RoleProfileBindings<'a>> {
    targets
        .iter()
        .map(|(profile, sections)| {
            let (agents, routing) = match profile {
                None => (&config.agents, &config.routing),
                Some(name) => {
                    let profile = &config.role_profiles[name];
                    (&profile.agents, &profile.routing)
                }
            };
            RoleProfileBindings {
                profile: profile.as_deref(),
                agents: sections.contains(&"agents").then_some(agents),
                routing: sections.contains(&"routing").then_some(routing),
            }
        })
        .collect()
}

fn section_names(targets: &Targets) -> Vec<String> {
    targets
        .iter()
        .flat_map(|(profile, sections)| {
            sections.iter().map(move |section| match profile {
                None => (*section).to_owned(),
                Some(name) => format!("role_profiles.{name}.{section}"),
            })
        })
        .collect()
}

fn options(user: &Path) -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(user.to_path_buf()),
        read_env: false,
        ..LoadOptions::default()
    }
}

/// Sections whose saved value a later file in `config.d` still overrides.
fn overridden(effective: &Config, wanted: &Config, targets: &Targets) -> Vec<String> {
    let section = |config: &Config, profile: &Option<String>, section: &str| {
        let (agents, routing) = match profile {
            None => (Some(&config.agents), Some(&config.routing)),
            Some(name) => config
                .role_profiles
                .get(name)
                .map_or((None, None), |profile| {
                    (Some(&profile.agents), Some(&profile.routing))
                }),
        };
        match section {
            "agents" => serde_json::to_value(agents).unwrap_or_default(),
            _ => serde_json::to_value(routing).unwrap_or_default(),
        }
    };
    targets
        .iter()
        .flat_map(|(profile, sections)| {
            sections
                .iter()
                .filter(|name| section(effective, profile, name) != section(wanted, profile, name))
                .map(move |name| match profile {
                    None => (*name).to_owned(),
                    Some(profile) => format!("role_profiles.{profile}.{name}"),
                })
        })
        .collect()
}

struct Change {
    path: PathBuf,
    before: String,
    after: String,
    sections: Vec<String>,
    overridden: Vec<String>,
}

fn diff(change: &Change) -> String {
    let name = change.path.display().to_string();
    similar::TextDiff::from_lines(&change.before, &change.after)
        .unified_diff()
        .context_radius(3)
        .header(&name, &name)
        .to_string()
}

struct PreparedChange {
    change: Change,
    save: config::save::ValidatedSave,
    wanted: Config,
    targets: Targets,
}

/// What applying `overlay` changes, or `None` when the file already holds it.
fn prepare(user: &Path, overlay: &toml::Value) -> Result<Option<PreparedChange>, String> {
    let current = Config::load_unresolved(&options(user)).map_err(|error| error.to_string())?;
    let targets = targets(overlay, &current)?;
    let wanted = Config::load_unresolved(&LoadOptions {
        cli_overrides: Some(overlay.clone()),
        ..options(user)
    })
    .map_err(|error| error.to_string())?;
    let path = user.join("config.toml");
    let before = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let prepared = config::LayeredSaveContext::new(&options(user))
        .prepare_role_profiles_bindings(&path, &bindings(&wanted, &targets))
        .map_err(|error| error.to_string())?;
    let after = prepared.text().to_owned();
    if after == before {
        return Ok(None);
    }
    let overridden = overridden(prepared.config(), &wanted, &targets);
    let change = Change {
        sections: section_names(&targets),
        path,
        before,
        after,
        overridden,
    };
    Ok(Some(PreparedChange {
        change,
        save: prepared,
        wanted,
        targets,
    }))
}

fn summary(change: &Change, diff: &str) -> String {
    let mut text = format!(
        "Apply the adopted configuration to {} ({}).\n\n{diff}",
        change.path.display(),
        change.sections.join(", ")
    );
    if !change.overridden.is_empty() {
        text.push_str(&format!(
            "\nA file in config.d still overrides {}, so that part will not take effect.\n",
            change.overridden.join(", ")
        ));
    }
    text
}

pub(super) struct ApplyConfig(pub(super) Arc<Shared>);

#[async_trait::async_trait]
impl Tool for ApplyConfig {
    fn name(&self) -> &'static str {
        "eval_apply_config"
    }

    fn description(&self) -> &'static str {
        "Write an adopted overlay into the user's config.toml through the settings savers. The user is shown the change as a diff and must approve; the call waits for that answer. Only agents, routing and role_profiles.<name>.agents / routing can be applied, for profiles that already exist."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "overlay": {"type": "string", "minLength": 1, "description": "The overlay as TOML, typically the evaluated candidate."},
            },
            "required": ["overlay"],
            "additionalProperties": false,
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_write()
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::error("eval_apply_config needs the calling run"))
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolExecutionContext,
        args: Value,
    ) -> Result<ToolResult, ToolError> {
        let ApplyArgs { overlay: text } = parse(args)?;
        let user = self.0.context.user_config_dir.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            let overlay = runner::parse_overlay(&text, Some(&user))
                .map_err(|error| format!("the overlay is unusable: {error}"))?;
            prepare(&user, &overlay)
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|prepared| prepared);
        let PreparedChange {
            mut change,
            save: prepared,
            wanted,
            targets,
        } = match prepared {
            Ok(Some(prepared)) => prepared,
            Ok(None) => {
                return Ok(ToolResult::success(
                    json!({"changed": false, "note": "the user configuration already holds this overlay"})
                        .to_string(),
                ));
            }
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let diff = diff(&change);
        if let Err(declined) =
            super::ask(&self.0, ctx, "eval_apply_config", summary(&change, &diff)).await
        {
            return Ok(ToolResult::error(format!(
                "{declined}; nothing was written"
            )));
        }
        let user = self.0.context.user_config_dir.clone();
        let written = tokio::task::spawn_blocking(move || {
            let now = std::fs::read_to_string(&change.path).unwrap_or_default();
            if now != change.before {
                return Err(format!(
                    "{} changed while waiting for the approval; nothing was written, so propose the change again",
                    change.path.display()
                ));
            }
            // Approval can outlive a drop-in edit. Recheck the approved bytes in
            // today's layers, rather than rebuild or trust the preview's snapshot.
            let prepared = config::LayeredSaveContext::new(&options(&user))
                .revalidate(prepared)
                .map_err(|error| error.to_string())?;
            change.overridden = overridden(prepared.config(), &wanted, &targets);
            prepared.write().map_err(|error| error.to_string())?;
            Ok(change)
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|written| written);
        let change = match written {
            Ok(change) => change,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let host = Arc::clone(&self.0.context.host);
        let reloaded = tokio::task::spawn_blocking(move || host.reload_config())
            .await
            .map_err(|error| error.to_string())
            .and_then(|reloaded| reloaded);
        let note = match &reloaded {
            Ok(()) => {
                "The models were recomposed as after a settings save, so runs use the new bindings from their next model request."
            }
            Err(_) => {
                "The models could not be recomposed; the change applies after the GUI restarts or the settings are saved again."
            }
        };
        Ok(ToolResult::success(
            json!({
                "changed": true,
                "written": change.path,
                "sections": change.sections,
                "diff": metrics::clip(&diff, DIFF_CHARS),
                "overridden_by_config_d": change.overridden,
                "reload_error": reloaded.err(),
                "note": format!("{note} A project's .evorch/config.toml role_profile selection still decides which profile a project uses."),
            })
            .to_string(),
        ))
    }
}
