//! Editable form of one MCP server and its input-time validation.
//!
//! The draft keeps the source syntax (`${VAR}`, `${VAR:-default}`, `$${`) as
//! typed text and converts it to the shared template AST only when checked, so
//! validation never expands environment variables, reads the keychain or
//! touches the filesystem. It is cheap enough to run every frame.

use std::collections::{BTreeMap, BTreeSet};

use config::mcp::expand::{is_valid_env_name, parse_template, render_template};
use config::mcp::validate::{
    validate_header_name, validate_keychain_name, validate_server, validate_server_id, validate_url,
};
use config::mcp::{
    DEFAULT_REQUEST_TIMEOUT_MS, DEFAULT_STARTUP_TIMEOUT_MS, MAX_TIMEOUT_MS, MCP_ROLE_KEYS,
    MIN_TIMEOUT_MS, McpConfigError, McpServerDefinition, McpServerEntry, McpTransportDefinition,
    Template, TemplatePart, ValueSource,
};

/// Roles a new server is published to.
pub const DEFAULT_ROLES: &[&str] = &["worker"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftTransport {
    Stdio,
    Http,
}

/// Where a value comes from: typed text (literal and `${VAR}` references) or
/// a logical OS keychain reference. Only the reference name is ever saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftValueKind {
    Text,
    Keychain,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DraftValue {
    pub kind: DraftValueKind,
    /// Template source text, or the keychain reference name.
    pub text: String,
}

// Values may hold plain-text credentials: keep them out of logs.
impl std::fmt::Debug for DraftValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DraftValue")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl DraftValue {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: DraftValueKind::Text,
            text: text.into(),
        }
    }

    pub fn keychain(name: impl Into<String>) -> Self {
        Self {
            kind: DraftValueKind::Keychain,
            text: name.into(),
        }
    }

    fn from_source(source: &ValueSource) -> Self {
        match source {
            ValueSource::Template { template } => Self::text(render(template)),
            ValueSource::Keychain { name } => Self::keychain(name.clone()),
        }
    }
}

/// One `NAME = value` row of `env` or `headers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftPair {
    pub name: String,
    pub value: DraftValue,
}

impl DraftPair {
    pub fn empty() -> Self {
        Self {
            name: String::new(),
            value: DraftValue::text(""),
        }
    }

    fn is_blank(&self) -> bool {
        self.name.trim().is_empty() && self.value.text.is_empty()
    }
}

/// A form field that can carry an input-time error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DraftField {
    Id,
    Command,
    Arg(usize),
    Cwd,
    EnvName(usize),
    EnvValue(usize),
    Url,
    Token,
    HeaderName(usize),
    HeaderValue(usize),
    StartupTimeout,
    RequestTimeout,
    /// Anything the field checks did not attribute, from the shared validator.
    Server,
}

/// A potentially risky but valid setting the user should see before saving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftWarning {
    /// A credential-looking value is typed in plain text and would be saved in mcp.toml.
    PlainTextSecret(String),
    /// No role selected: the server is configured but no agent sees it.
    Unpublished,
    /// `http://` sends requests and headers unencrypted.
    PlainHttp,
}

impl DraftWarning {
    pub fn message(&self) -> String {
        match self {
            Self::PlainTextSecret(name) => format!(
                "{name} is saved in plain text in .evorch/mcp.toml. Use ${{VAR}} or a keychain reference for secrets."
            ),
            Self::Unpublished => "No role is selected, so no agent can use this server.".to_owned(),
            Self::PlainHttp => {
                "http:// is unencrypted; use it only for servers on this machine or a trusted network."
                    .to_owned()
            }
        }
    }
}

/// Result of checking a draft: the definition to save, or field errors.
#[derive(Debug, Clone, Default)]
pub struct DraftCheck {
    pub errors: BTreeMap<DraftField, String>,
    pub warnings: Vec<DraftWarning>,
    definition: Option<(String, McpServerDefinition)>,
}

impl DraftCheck {
    pub fn is_valid(&self) -> bool {
        self.definition().is_some()
    }

    pub fn error(&self, field: DraftField) -> Option<&str> {
        self.errors.get(&field).map(String::as_str)
    }

    /// The server ID and complete definition, when every check passed.
    pub fn definition(&self) -> Option<&(String, McpServerDefinition)> {
        if self.errors.is_empty() {
            self.definition.as_ref()
        } else {
            None
        }
    }

    pub fn into_definition(self) -> Option<(String, McpServerDefinition)> {
        if self.errors.is_empty() {
            self.definition
        } else {
            None
        }
    }
}

/// The editor form for one server. Saving always writes the whole definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerDraft {
    /// `Some` when editing an existing server ID; IDs are not renamed in place.
    pub existing_id: Option<String>,
    pub id: String,
    /// A `.mcp.json` server of the same ID exists: saving writes a full override.
    pub imported: bool,
    pub transport: DraftTransport,
    pub enabled: bool,
    pub roles: BTreeSet<String>,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<DraftPair>,
    pub url: String,
    /// `None` sends no bearer token.
    pub token: Option<DraftValue>,
    pub headers: Vec<DraftPair>,
    pub startup_timeout_ms: String,
    pub request_timeout_ms: String,
}

impl McpServerDraft {
    /// A new stdio server published to the default roles.
    pub fn new_server() -> Self {
        Self {
            existing_id: None,
            id: String::new(),
            imported: false,
            transport: DraftTransport::Stdio,
            enabled: true,
            roles: DEFAULT_ROLES
                .iter()
                .map(|role| (*role).to_owned())
                .collect(),
            command: String::new(),
            args: Vec::new(),
            cwd: String::new(),
            env: Vec::new(),
            url: String::new(),
            token: None,
            headers: Vec::new(),
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS.to_string(),
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS.to_string(),
        }
    }

    /// Edits `entry`; an entry without a usable definition (invalid or a
    /// tombstone) starts from an empty server with the same ID.
    pub fn from_entry(id: &str, entry: &McpServerEntry) -> Self {
        let mut draft = Self::new_server();
        draft.existing_id = Some(id.to_owned());
        id.clone_into(&mut draft.id);
        draft.imported = entry.imported_exists;
        let Some(definition) = &entry.definition else {
            return draft;
        };
        draft.enabled = definition.enabled;
        draft.roles = definition.roles.iter().cloned().collect();
        draft.startup_timeout_ms = definition.startup_timeout_ms.to_string();
        draft.request_timeout_ms = definition.request_timeout_ms.to_string();
        match &definition.transport {
            McpTransportDefinition::Stdio {
                command,
                args,
                cwd,
                env,
            } => {
                draft.transport = DraftTransport::Stdio;
                draft.command = render(command);
                draft.args = args.iter().map(render).collect();
                draft.cwd = cwd.as_ref().map(render).unwrap_or_default();
                draft.env = pairs(env);
            }
            McpTransportDefinition::Http {
                url,
                headers,
                token,
            } => {
                draft.transport = DraftTransport::Http;
                draft.url = render(url);
                draft.token = token.as_ref().map(DraftValue::from_source);
                draft.headers = pairs(headers);
            }
        }
        draft
    }

    pub fn is_new(&self) -> bool {
        self.existing_id.is_none()
    }

    /// Checks every field without I/O. `taken_ids` are IDs a new server must not reuse.
    pub fn check(&self, taken_ids: &BTreeSet<String>) -> DraftCheck {
        let mut check = DraftCheck::default();
        let id = self.id.trim().to_owned();
        if id.is_empty() {
            check
                .errors
                .insert(DraftField::Id, "Enter a server name.".into());
        } else if validate_server_id(&id).is_err() {
            check.errors.insert(
                DraftField::Id,
                "Use at most 128 bytes without control characters.".into(),
            );
        } else if self.is_new() && taken_ids.contains(&id) {
            check.errors.insert(
                DraftField::Id,
                "A server with this name already exists.".into(),
            );
        }
        let startup = timeout(
            &mut check,
            DraftField::StartupTimeout,
            &self.startup_timeout_ms,
        );
        let request = timeout(
            &mut check,
            DraftField::RequestTimeout,
            &self.request_timeout_ms,
        );
        let roles: Vec<String> = MCP_ROLE_KEYS
            .iter()
            .filter(|role| self.roles.contains(**role))
            .map(|role| (*role).to_owned())
            .collect();
        if roles.is_empty() {
            check.warnings.push(DraftWarning::Unpublished);
        }
        let transport = match self.transport {
            DraftTransport::Stdio => self.check_stdio(&mut check),
            DraftTransport::Http => self.check_http(&mut check),
        };
        let (Some(transport), Some(startup), Some(request)) = (transport, startup, request) else {
            return check;
        };
        if !check.errors.is_empty() {
            return check;
        }
        let definition = McpServerDefinition {
            enabled: self.enabled,
            roles,
            transport,
            startup_timeout_ms: startup,
            request_timeout_ms: request,
        };
        // The shared validator is the authority; field checks only localize errors.
        if let Err(code) = validate_server(&id, &definition) {
            check
                .errors
                .insert(DraftField::Server, config_error_text(code));
            return check;
        }
        check.definition = Some((id, definition));
        check
    }

    fn check_stdio(&self, check: &mut DraftCheck) -> Option<McpTransportDefinition> {
        let command = if self.command.trim().is_empty() {
            check
                .errors
                .insert(DraftField::Command, "Enter the command to run.".into());
            None
        } else {
            template(check, DraftField::Command, &self.command)
        };
        let args: Vec<Option<Template>> = self
            .args
            .iter()
            .enumerate()
            .map(|(index, arg)| template(check, DraftField::Arg(index), arg))
            .collect();
        let cwd = if self.cwd.trim().is_empty() {
            Some(None)
        } else {
            template(check, DraftField::Cwd, &self.cwd).map(Some)
        };
        let env = check_pairs(check, &self.env, PairKind::Env, false);
        Some(McpTransportDefinition::Stdio {
            command: command?,
            args: args.into_iter().collect::<Option<_>>()?,
            cwd: cwd?,
            env: env?,
        })
    }

    fn check_http(&self, check: &mut DraftCheck) -> Option<McpTransportDefinition> {
        let url = if self.url.trim().is_empty() {
            check
                .errors
                .insert(DraftField::Url, "Enter the server URL.".into());
            None
        } else {
            check_url(check, &self.url)
        };
        let token = match &self.token {
            None => Some(None),
            Some(value) => {
                let source = value_source(check, DraftField::Token, value, true);
                if let Some(ValueSource::Template { template }) = &source
                    && has_literal(template)
                {
                    check
                        .warnings
                        .push(DraftWarning::PlainTextSecret("The bearer token".into()));
                }
                source.map(Some)
            }
        };
        let headers = check_pairs(check, &self.headers, PairKind::Header, self.token.is_some());
        Some(McpTransportDefinition::Http {
            url: url?,
            headers: headers?,
            token: token?,
        })
    }
}

fn check_url(check: &mut DraftCheck, text: &str) -> Option<Template> {
    let parsed = template(check, DraftField::Url, text)?;
    // A leading reference may supply the scheme; it is re-checked after expansion.
    if matches!(parsed.0.first(), Some(TemplatePart::Env { .. })) {
        return Some(parsed);
    }
    let sample: String = parsed
        .0
        .iter()
        .map(|part| match part {
            TemplatePart::Literal { value } => value.as_str(),
            TemplatePart::Env { .. } => "1",
        })
        .collect();
    if validate_url(&sample).is_err() {
        check.errors.insert(
            DraftField::Url,
            "Use an http:// or https:// URL with a host, without user info or #fragment.".into(),
        );
        return None;
    }
    if sample
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"))
    {
        check.warnings.push(DraftWarning::PlainHttp);
    }
    Some(parsed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairKind {
    Env,
    Header,
}

impl PairKind {
    fn name_field(self, index: usize) -> DraftField {
        match self {
            Self::Env => DraftField::EnvName(index),
            Self::Header => DraftField::HeaderName(index),
        }
    }

    fn value_field(self, index: usize) -> DraftField {
        match self {
            Self::Env => DraftField::EnvValue(index),
            Self::Header => DraftField::HeaderValue(index),
        }
    }

    fn dedup_key(self, name: &str) -> String {
        match self {
            Self::Env => name.to_owned(),
            Self::Header => name.to_ascii_lowercase(),
        }
    }

    fn name_error(self, name: &str, has_token: bool) -> Option<&'static str> {
        match self {
            Self::Env => (!is_valid_env_name(name))
                .then_some("Use letters, digits and _, not starting with a digit."),
            Self::Header if name.eq_ignore_ascii_case("authorization") => {
                has_token.then_some("Use either the bearer token or an Authorization header.")
            }
            Self::Header => validate_header_name(name).err().map(|_| {
                "Use a valid header name; Host, Content-*, Accept and Mcp-* are managed by evorch."
            }),
        }
    }
}

fn check_pairs(
    check: &mut DraftCheck,
    pairs: &[DraftPair],
    kind: PairKind,
    has_token: bool,
) -> Option<BTreeMap<String, ValueSource>> {
    let mut out = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut valid = true;
    for (index, pair) in pairs.iter().enumerate() {
        if pair.is_blank() {
            continue;
        }
        let name = pair.name.trim();
        if let Some(error) = kind.name_error(name, has_token) {
            check.errors.insert(kind.name_field(index), error.into());
            valid = false;
        } else if !seen.insert(kind.dedup_key(name)) {
            check
                .errors
                .insert(kind.name_field(index), "Duplicate name.".into());
            valid = false;
        }
        let header = kind == PairKind::Header;
        match value_source(check, kind.value_field(index), &pair.value, header) {
            Some(source) => {
                if let ValueSource::Template { template } = &source
                    && has_literal(template)
                    && looks_secret(name)
                {
                    check
                        .warnings
                        .push(DraftWarning::PlainTextSecret(name.to_owned()));
                }
                out.insert(name.to_owned(), source);
            }
            None => valid = false,
        }
    }
    valid.then_some(out)
}

fn render(template: &Template) -> String {
    // Loaded definitions always have a source spelling; fall back to the plain parts.
    render_template(template).unwrap_or_else(|_| {
        template
            .0
            .iter()
            .map(|part| match part {
                TemplatePart::Literal { value } => value.clone(),
                TemplatePart::Env { name, default } => match default {
                    Some(default) => format!("${{{name}:-{default}}}"),
                    None => format!("${{{name}}}"),
                },
            })
            .collect()
    })
}

fn pairs(map: &BTreeMap<String, ValueSource>) -> Vec<DraftPair> {
    map.iter()
        .map(|(name, source)| DraftPair {
            name: name.clone(),
            value: DraftValue::from_source(source),
        })
        .collect()
}

fn template(check: &mut DraftCheck, field: DraftField, text: &str) -> Option<Template> {
    if text.contains('\0') {
        check
            .errors
            .insert(field, "Remove the NUL character.".into());
        return None;
    }
    match parse_template(text) {
        Ok(template) => Some(template),
        Err(code) => {
            check.errors.insert(field, config_error_text(code));
            None
        }
    }
}

fn value_source(
    check: &mut DraftCheck,
    field: DraftField,
    value: &DraftValue,
    header: bool,
) -> Option<ValueSource> {
    match value.kind {
        DraftValueKind::Keychain => {
            let name = value.text.trim();
            if validate_keychain_name(name).is_err() {
                check.errors.insert(
                    field,
                    "Keychain references use letters, digits, '.', '_' and '-'.".into(),
                );
                return None;
            }
            Some(ValueSource::Keychain {
                name: name.to_owned(),
            })
        }
        DraftValueKind::Text => {
            if header && value.text.chars().any(|c| c != '\t' && c.is_control()) {
                check
                    .errors
                    .insert(field, "Header values cannot contain line breaks.".into());
                return None;
            }
            template(check, field, &value.text).map(|template| ValueSource::Template { template })
        }
    }
}

fn timeout(check: &mut DraftCheck, field: DraftField, text: &str) -> Option<u64> {
    match text.trim().parse::<u64>() {
        Ok(value) if (MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&value) => Some(value),
        _ => {
            check.errors.insert(
                field,
                format!("Enter milliseconds from {MIN_TIMEOUT_MS} to {MAX_TIMEOUT_MS}."),
            );
            None
        }
    }
}

fn has_literal(template: &Template) -> bool {
    template
        .0
        .iter()
        .any(|part| matches!(part, TemplatePart::Literal { value } if !value.trim().is_empty()))
}

/// Names that usually carry credentials; only drives a warning, never a block.
fn looks_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "key",
        "auth",
        "credential",
        "cookie",
        "session",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// User-facing text for a classified configuration error.
pub fn config_error_text(code: McpConfigError) -> String {
    match code {
        McpConfigError::Unavailable => "MCP configuration is unavailable.",
        McpConfigError::Io => "The MCP configuration file could not be read or written.",
        McpConfigError::Syntax => "The MCP configuration file has a syntax error.",
        McpConfigError::Invalid => "This server definition is invalid.",
        McpConfigError::InvalidTemplate => "Invalid ${VAR} reference. Write $${ for a literal ${.",
        McpConfigError::MissingEnvironment => "A referenced environment variable is not set.",
        McpConfigError::LimitExceeded => "A size or count limit is exceeded.",
        McpConfigError::Conflict => "The configuration changed on disk.",
        McpConfigError::Symlink => "The configuration file is a symlink, which is refused.",
    }
    .to_owned()
}
