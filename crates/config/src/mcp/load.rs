//! Worker A: bounded parsing and full-definition precedence; no MCP I/O.
//!
//! Precedence: `.mcp.json` (read-only import) first, then `.evorch/mcp.toml`.
//! A server ID present in the TOML replaces the imported server as a whole; an
//! invalid or disabled TOML entry never reveals the imported definition. A TOML
//! document that cannot be parsed blocks new preparation for the project.
use super::expand::parse_template;
use super::validate::validate_server;
use super::{
    DEFAULT_REQUEST_TIMEOUT_MS, DEFAULT_STARTUP_TIMEOUT_MS, MAX_CONFIG_BYTES, MAX_SERVERS,
    McpConfigDiagnostic, McpConfigError, McpServerDefinition, McpServerEntry, McpSource,
    McpTransportDefinition, ProjectMcpConfig, SourceRevision, Template, ValueSource,
};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

pub(super) const JSON_FILE: &str = ".mcp.json";
pub(super) const TOML_FILE: &str = "mcp.toml";

pub(super) fn json_path(root: &Path) -> PathBuf {
    root.join(JSON_FILE)
}

pub(super) fn toml_dir(root: &Path) -> PathBuf {
    root.join(crate::PROJECT_CONFIG_DIR)
}

pub(super) fn toml_path(root: &Path) -> PathBuf {
    toml_dir(root).join(TOML_FILE)
}

/// Raw bytes of both source files, or the classified reason one could not be read.
pub(super) struct Sources {
    pub json: Result<Option<Vec<u8>>, McpConfigError>,
    pub toml: Result<Option<Vec<u8>>, McpConfigError>,
}

impl Sources {
    pub(super) fn read(root: &Path) -> Self {
        Self {
            json: read_bounded(&json_path(root)),
            toml: read_bounded(&toml_path(root)),
        }
    }

    pub(super) fn revision(&self) -> SourceRevision {
        let bytes = |source: &Result<Option<Vec<u8>>, McpConfigError>| {
            source.as_ref().ok().and_then(|bytes| bytes.clone())
        };
        let (json, toml) = (bytes(&self.json), bytes(&self.toml));
        SourceRevision::from_sources(json.as_deref(), toml.as_deref())
    }
}

/// Regular files only (a FIFO or device would block or never end), size-bounded.
fn read_bounded(path: &Path) -> Result<Option<Vec<u8>>, McpConfigError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(McpConfigError::Io),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(McpConfigError::Io),
    }
    let file = std::fs::File::open(path).map_err(|_| McpConfigError::Io)?;
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| McpConfigError::Io)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(McpConfigError::LimitExceeded);
    }
    Ok(Some(bytes))
}

/// Fail closed on I/O errors; a missing source is simply absent.
pub fn load_project(root: &Path) -> Result<ProjectMcpConfig, McpConfigError> {
    let canonical_root = std::fs::canonicalize(root).map_err(|_| McpConfigError::Io)?;
    let sources = Sources::read(&canonical_root);
    Ok(build_config(canonical_root, &sources))
}

pub(super) enum ParsedServer {
    Tombstone,
    Definition(McpServerDefinition),
}

type ParsedServers = BTreeMap<String, Result<ParsedServer, McpConfigError>>;

pub(super) fn build_config(canonical_root: PathBuf, sources: &Sources) -> ProjectMcpConfig {
    let mut diagnostics = Vec::new();
    let mut preparation_blocked = false;
    let mut global = |source: McpSource, code: McpConfigError| {
        diagnostics.push(McpConfigDiagnostic {
            server_id: None,
            source,
            code,
        });
    };

    let imported = match &sources.json {
        Ok(None) => ParsedServers::new(),
        Ok(Some(bytes)) => parse_json_document(bytes).unwrap_or_else(|code| {
            global(McpSource::ImportedJson, code);
            ParsedServers::new()
        }),
        Err(code) => {
            global(McpSource::ImportedJson, *code);
            ParsedServers::new()
        }
    };
    let overrides = match &sources.toml {
        Ok(None) => ParsedServers::new(),
        Ok(Some(bytes)) => parse_toml_document(bytes).unwrap_or_else(|code| {
            global(McpSource::ProjectToml, code);
            preparation_blocked = true;
            ParsedServers::new()
        }),
        Err(code) => {
            global(McpSource::ProjectToml, *code);
            preparation_blocked = true;
            ParsedServers::new()
        }
    };

    let mut servers = BTreeMap::new();
    if !preparation_blocked {
        let mut ids: Vec<&String> = imported.keys().chain(overrides.keys()).collect();
        ids.sort();
        ids.dedup();
        if ids.len() > MAX_SERVERS {
            global(McpSource::ProjectToml, McpConfigError::LimitExceeded);
            preparation_blocked = true;
        } else {
            for id in ids {
                let imported_exists = imported.contains_key(id);
                let entry = match (overrides.get(id), imported.get(id)) {
                    (Some(parsed), _) => {
                        entry_from(McpSource::ProjectToml, imported_exists, parsed)
                    }
                    (None, Some(parsed)) => entry_from(McpSource::ImportedJson, true, parsed),
                    (None, None) => continue,
                };
                servers.insert(id.clone(), with_server_id(entry, id));
            }
        }
    }
    drop(global);
    ProjectMcpConfig {
        canonical_root,
        revision: sources.revision(),
        servers,
        preparation_blocked,
        diagnostics,
    }
}

fn entry_from(
    source: McpSource,
    imported_exists: bool,
    parsed: &Result<ParsedServer, McpConfigError>,
) -> McpServerEntry {
    let (definition, tombstone, code) = match parsed {
        Ok(ParsedServer::Definition(definition)) => (Some(definition.clone()), false, None),
        Ok(ParsedServer::Tombstone) => (None, true, None),
        Err(code) => (None, false, Some(*code)),
    };
    McpServerEntry {
        source,
        imported_exists,
        definition,
        tombstone,
        diagnostics: code
            .map(|code| McpConfigDiagnostic {
                server_id: None,
                source,
                code,
            })
            .into_iter()
            .collect(),
    }
}

fn with_server_id(mut entry: McpServerEntry, id: &str) -> McpServerEntry {
    for diagnostic in &mut entry.diagnostics {
        diagnostic.server_id = Some(id.to_owned());
    }
    entry
}

/// `serde_json::Value` that refuses duplicate object keys instead of last-wins.
struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(v)))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::from(v)))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::from(v)))
            }

            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::from(v)))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v.to_owned())))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut items = Vec::new();
                while let Some(StrictValue(item)) = seq.next_element()? {
                    items.push(item);
                }
                Ok(StrictValue(Value::Array(items)))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut object = Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    let StrictValue(value) = map.next_value()?;
                    if object.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate key"));
                    }
                }
                Ok(StrictValue(Value::Object(object)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

fn parse_json_document(bytes: &[u8]) -> Result<ParsedServers, McpConfigError> {
    let StrictValue(document) =
        serde_json::from_slice(bytes).map_err(|_| McpConfigError::Syntax)?;
    let Value::Object(top) = document else {
        return Err(McpConfigError::Invalid);
    };
    // Unknown top-level keys are tolerated: other tools share this file.
    let servers = match top.get("mcpServers") {
        None => return Ok(ParsedServers::new()),
        Some(Value::Object(servers)) => servers,
        Some(_) => return Err(McpConfigError::Invalid),
    };
    if servers.len() > MAX_SERVERS {
        return Err(McpConfigError::LimitExceeded);
    }
    Ok(servers
        .iter()
        .map(|(id, value)| {
            let parsed = match value {
                Value::Object(map) => parse_server(id, map, Format::Json),
                _ => Err(McpConfigError::Invalid),
            };
            (id.clone(), parsed)
        })
        .collect())
}

fn toml_to_json(value: &toml::Value) -> Result<Value, McpConfigError> {
    Ok(match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(_) => return Err(McpConfigError::Invalid),
        toml::Value::Array(items) => {
            Value::Array(items.iter().map(toml_to_json).collect::<Result<_, _>>()?)
        }
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(key, value)| Ok((key.clone(), toml_to_json(value)?)))
                .collect::<Result<Map<_, _>, McpConfigError>>()?,
        ),
    })
}

/// Document-level failures are returned as `Err`; per-server failures stay entries.
pub(super) fn parse_toml_document(bytes: &[u8]) -> Result<ParsedServers, McpConfigError> {
    let text = std::str::from_utf8(bytes).map_err(|_| McpConfigError::Syntax)?;
    let table: toml::Table = toml::from_str(text).map_err(|_| McpConfigError::Syntax)?;
    if table.keys().any(|key| key != "version" && key != "servers") {
        return Err(McpConfigError::Invalid);
    }
    if table.get("version").and_then(toml::Value::as_integer) != Some(1) {
        return Err(McpConfigError::Invalid);
    }
    let servers = match table.get("servers") {
        None => return Ok(ParsedServers::new()),
        Some(toml::Value::Table(servers)) => servers,
        Some(_) => return Err(McpConfigError::Invalid),
    };
    if servers.len() > MAX_SERVERS {
        return Err(McpConfigError::LimitExceeded);
    }
    Ok(servers
        .iter()
        .map(|(id, value)| {
            let parsed = match toml_to_json(value) {
                Ok(Value::Object(map)) => parse_server(id, &map, Format::Toml),
                _ => Err(McpConfigError::Invalid),
            };
            (id.clone(), parsed)
        })
        .collect())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    /// Read-only import: command/args/env/cwd/url/headers with string values only.
    Json,
    Toml,
}

const COMMON_FIELDS: &[&str] = &["type", "command", "args", "env", "cwd", "url", "headers"];
const TOML_FIELDS: &[&str] = &[
    "enabled",
    "roles",
    "token",
    "startup_timeout_ms",
    "request_timeout_ms",
];

fn parse_server(
    id: &str,
    map: &Map<String, Value>,
    format: Format,
) -> Result<ParsedServer, McpConfigError> {
    let invalid = McpConfigError::Invalid;
    let allowed = |key: &str| {
        COMMON_FIELDS.contains(&key) || (format == Format::Toml && TOML_FIELDS.contains(&key))
    };
    if map.keys().any(|key| !allowed(key)) {
        return Err(invalid);
    }
    if format == Format::Toml && map.len() == 1 && map.get("enabled") == Some(&Value::Bool(false)) {
        return Ok(ParsedServer::Tombstone);
    }
    let stdio_fields = ["command", "args", "cwd", "env"];
    let http_fields = ["url", "headers", "token"];
    let has = |fields: &[&str]| fields.iter().any(|field| map.contains_key(*field));
    let kind = match (map.get("type"), format) {
        (Some(Value::String(kind)), _) => kind.as_str(),
        (Some(_), _) => return Err(invalid),
        (None, Format::Json) if map.contains_key("command") => "stdio",
        (None, _) => return Err(invalid),
    };
    let allow_keychain = format == Format::Toml;
    let transport = match kind {
        "stdio" => {
            if has(&http_fields) {
                return Err(invalid);
            }
            McpTransportDefinition::Stdio {
                command: template_field(map.get("command").ok_or(invalid)?)?,
                args: match map.get("args") {
                    None => Vec::new(),
                    Some(Value::Array(items)) => {
                        items.iter().map(template_field).collect::<Result<_, _>>()?
                    }
                    Some(_) => return Err(invalid),
                },
                cwd: map.get("cwd").map(template_field).transpose()?,
                env: source_map(map.get("env"), allow_keychain)?,
            }
        }
        "http" => {
            if has(&stdio_fields) {
                return Err(invalid);
            }
            McpTransportDefinition::Http {
                url: template_field(map.get("url").ok_or(invalid)?)?,
                headers: source_map(map.get("headers"), allow_keychain)?,
                token: map
                    .get("token")
                    .map(|value| value_source(value, allow_keychain))
                    .transpose()?,
            }
        }
        _ => return Err(invalid),
    };
    let enabled = match map.get("enabled") {
        None => true,
        Some(Value::Bool(enabled)) => *enabled,
        Some(_) => return Err(invalid),
    };
    let roles = match map.get("roles") {
        None => vec!["worker".to_owned()],
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or(invalid))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(invalid),
    };
    let timeout = |key: &str, default: u64| match map.get(key) {
        None => Ok(default),
        Some(value) => value.as_u64().ok_or(invalid),
    };
    let definition = McpServerDefinition {
        enabled,
        roles,
        transport,
        startup_timeout_ms: timeout("startup_timeout_ms", DEFAULT_STARTUP_TIMEOUT_MS)?,
        request_timeout_ms: timeout("request_timeout_ms", DEFAULT_REQUEST_TIMEOUT_MS)?,
    };
    validate_server(id, &definition)?;
    Ok(ParsedServer::Definition(definition))
}

fn template_field(value: &Value) -> Result<Template, McpConfigError> {
    parse_template(value.as_str().ok_or(McpConfigError::Invalid)?)
}

fn value_source(value: &Value, allow_keychain: bool) -> Result<ValueSource, McpConfigError> {
    match value {
        Value::String(text) => Ok(ValueSource::Template {
            template: parse_template(text)?,
        }),
        Value::Object(map) if allow_keychain && map.len() == 1 => match map.get("keychain") {
            Some(Value::String(name)) => Ok(ValueSource::Keychain { name: name.clone() }),
            _ => Err(McpConfigError::Invalid),
        },
        _ => Err(McpConfigError::Invalid),
    }
}

fn source_map(
    value: Option<&Value>,
    allow_keychain: bool,
) -> Result<BTreeMap<String, ValueSource>, McpConfigError> {
    match value {
        None => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| Ok((key.clone(), value_source(value, allow_keychain)?)))
            .collect(),
        Some(_) => Err(McpConfigError::Invalid),
    }
}
