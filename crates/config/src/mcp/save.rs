//! Worker A: atomic 0600 TOML-only editing with source revision checks.
//!
//! Only `.evorch/mcp.toml` is ever written; `.mcp.json` is a read-only import.
//! An edit rewrites exactly one `[servers.<id>]` table (complete replacement,
//! never a field merge) and keeps the rest of the document, comments included.
use super::expand::render_template;
use super::load::{Sources, build_config, load_project, toml_dir, toml_path};
use super::validate::{validate_keychain_name, validate_server, validate_server_id};
use super::{
    MAX_CONFIG_BYTES, McpConfigError, McpOverrideMutation, McpServerDefinition,
    McpTransportDefinition, ProjectMcpConfig, SourceRevision, Template, ValueSource,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn template_item(template: &Template) -> Result<Item, McpConfigError> {
    Ok(value(render_template(template)?))
}

fn source_item(source: &ValueSource) -> Result<Item, McpConfigError> {
    match source {
        ValueSource::Template { template } => template_item(template),
        ValueSource::Keychain { name } => {
            validate_keychain_name(name)?;
            let mut inline = InlineTable::new();
            inline.insert("keychain", name.as_str().into());
            Ok(Item::Value(inline.into()))
        }
    }
}

fn source_table(map: &BTreeMap<String, ValueSource>) -> Result<Item, McpConfigError> {
    let mut table = Table::new();
    for (key, source) in map {
        table.insert(key, source_item(source)?);
    }
    Ok(Item::Table(table))
}

fn render_definition(definition: &McpServerDefinition) -> Result<Table, McpConfigError> {
    let mut table = Table::new();
    let kind = match &definition.transport {
        McpTransportDefinition::Stdio { .. } => "stdio",
        McpTransportDefinition::Http { .. } => "http",
    };
    table.insert("type", value(kind));
    table.insert("enabled", value(definition.enabled));
    let roles: Array = definition.roles.iter().map(String::as_str).collect();
    table.insert("roles", Item::Value(roles.into()));
    match &definition.transport {
        McpTransportDefinition::Stdio {
            command,
            args,
            cwd,
            env,
        } => {
            table.insert("command", template_item(command)?);
            let mut rendered = Array::new();
            for arg in args {
                rendered.push(render_template(arg)?);
            }
            table.insert("args", Item::Value(rendered.into()));
            if let Some(cwd) = cwd {
                table.insert("cwd", template_item(cwd)?);
            }
            table.insert(
                "startup_timeout_ms",
                timeout_item(definition.startup_timeout_ms)?,
            );
            table.insert(
                "request_timeout_ms",
                timeout_item(definition.request_timeout_ms)?,
            );
            if !env.is_empty() {
                table.insert("env", source_table(env)?);
            }
        }
        McpTransportDefinition::Http {
            url,
            headers,
            token,
        } => {
            table.insert("url", template_item(url)?);
            if let Some(token) = token {
                table.insert("token", source_item(token)?);
            }
            table.insert(
                "startup_timeout_ms",
                timeout_item(definition.startup_timeout_ms)?,
            );
            table.insert(
                "request_timeout_ms",
                timeout_item(definition.request_timeout_ms)?,
            );
            if !headers.is_empty() {
                table.insert("headers", source_table(headers)?);
            }
        }
    }
    Ok(table)
}

fn timeout_item(ms: u64) -> Result<Item, McpConfigError> {
    Ok(value(
        i64::try_from(ms).map_err(|_| McpConfigError::Invalid)?,
    ))
}

fn server_id(mutation: &McpOverrideMutation) -> &str {
    match mutation {
        McpOverrideMutation::Upsert { server_id, .. }
        | McpOverrideMutation::Disable { server_id }
        | McpOverrideMutation::RemoveOverride { server_id } => server_id,
    }
}

/// `Ok(None)` means the mutation changes nothing and no write is needed.
fn apply_mutation(
    existing: Option<&[u8]>,
    mutation: &McpOverrideMutation,
) -> Result<Option<String>, McpConfigError> {
    let id = server_id(mutation);
    let mut document = match existing {
        Some(bytes) => {
            let text = std::str::from_utf8(bytes).map_err(|_| McpConfigError::Syntax)?;
            DocumentMut::from_str(text).map_err(|_| McpConfigError::Syntax)?
        }
        None => DocumentMut::new(),
    };
    if document.as_table().is_empty() {
        document.insert("version", value(1));
    }
    let replacement = match mutation {
        McpOverrideMutation::Upsert { definition, .. } => Some(render_definition(definition)?),
        McpOverrideMutation::Disable { .. } => {
            let mut table = Table::new();
            table.insert("enabled", value(false));
            Some(table)
        }
        McpOverrideMutation::RemoveOverride { .. } => None,
    };
    match replacement {
        Some(table) => {
            let servers = document
                .entry("servers")
                .or_insert_with(|| {
                    let mut servers = Table::new();
                    servers.set_implicit(true);
                    Item::Table(servers)
                })
                .as_table_mut()
                .ok_or(McpConfigError::Invalid)?;
            servers.insert(id, Item::Table(table));
        }
        None => {
            let Some(servers) = document.get_mut("servers") else {
                return Ok(None);
            };
            let servers = servers.as_table_mut().ok_or(McpConfigError::Invalid)?;
            if servers.remove(id).is_none() {
                return Ok(None);
            }
        }
    }
    Ok(Some(document.to_string()))
}

fn refuse_symlinks(root: &Path) -> Result<(), McpConfigError> {
    let dir = toml_dir(root);
    match std::fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => return Err(McpConfigError::Symlink),
        Ok(metadata) if !metadata.is_dir() => return Err(McpConfigError::Io),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(McpConfigError::Io),
    }
    match std::fs::symlink_metadata(toml_path(root)) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(McpConfigError::Symlink),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(McpConfigError::Io),
    }
}

fn create_temp(dir: &Path) -> Result<(std::fs::File, std::path::PathBuf), McpConfigError> {
    for _ in 0..16 {
        let name = format!(
            ".mcp.toml.{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(McpConfigError::Io),
        }
    }
    Err(McpConfigError::Io)
}

/// Write to a sibling temp file, re-verify both sources, then rename over the target.
fn write_atomic(
    root: &Path,
    expected: SourceRevision,
    contents: &str,
) -> Result<(), McpConfigError> {
    let dir = toml_dir(root);
    std::fs::create_dir_all(&dir).map_err(|_| McpConfigError::Io)?;
    let (mut file, temp) = create_temp(&dir)?;
    let result = (|| {
        file.write_all(contents.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|_| McpConfigError::Io)?;
        refuse_symlinks(root)?;
        if Sources::read(root).revision() != expected {
            return Err(McpConfigError::Conflict);
        }
        std::fs::rename(&temp, toml_path(root)).map_err(|_| McpConfigError::Io)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    } else if let Ok(directory) = std::fs::File::open(&dir) {
        let _ = directory.sync_all();
    }
    result
}

pub fn save_override(
    root: &Path,
    expected_revision: SourceRevision,
    mutation: McpOverrideMutation,
) -> Result<ProjectMcpConfig, McpConfigError> {
    let root = std::fs::canonicalize(root).map_err(|_| McpConfigError::Io)?;
    let id = server_id(&mutation);
    validate_server_id(id)?;
    if let McpOverrideMutation::Upsert { definition, .. } = &mutation {
        validate_server(id, definition)?;
    }
    refuse_symlinks(&root)?;
    let sources = Sources::read(&root);
    if sources.revision() != expected_revision {
        return Err(McpConfigError::Conflict);
    }
    let existing = sources.toml.clone()?;
    let Some(text) = apply_mutation(existing.as_deref(), &mutation)? else {
        return load_project(&root);
    };
    if text.len() > MAX_CONFIG_BYTES {
        return Err(McpConfigError::LimitExceeded);
    }

    let projected = build_config(
        root.clone(),
        &Sources {
            json: sources.json.clone(),
            toml: Ok(Some(text.clone().into_bytes())),
        },
    );
    if projected.preparation_blocked {
        let code = projected
            .diagnostics
            .first()
            .map_or(McpConfigError::Invalid, |diagnostic| diagnostic.code);
        return Err(code);
    }
    if !matches!(mutation, McpOverrideMutation::RemoveOverride { .. })
        && projected
            .servers
            .get(id)
            .is_none_or(|entry| !entry.diagnostics.is_empty())
    {
        return Err(McpConfigError::Invalid);
    }

    write_atomic(&root, expected_revision, &text)?;
    load_project(&root)
}
