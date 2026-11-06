//! Worker A: validate without executing, resolving DNS or reading keychains.
use super::expand::{is_valid_env_name, template_len, template_texts, with_placeholder};
use super::{
    MAX_STRING_BYTES, MAX_TIMEOUT_MS, MCP_ROLE_KEYS, MIN_TIMEOUT_MS, McpConfigError,
    McpServerDefinition, McpTransportDefinition, Template, TemplatePart, ValueSource,
};
use std::collections::BTreeSet;

/// Bounds on collection sizes inside one server (config size is bounded separately).
pub const MAX_SERVER_ID_BYTES: usize = 128;
pub const MAX_ARGS: usize = 1024;
pub const MAX_MAP_ENTRIES: usize = 256;
const MAX_NAME_BYTES: usize = 256;

fn invalid() -> McpConfigError {
    McpConfigError::Invalid
}

/// Server IDs are free-form (public tool names are hashed when unsafe) but bounded.
pub fn validate_server_id(id: &str) -> Result<(), McpConfigError> {
    if id.is_empty() || id.len() > MAX_SERVER_ID_BYTES || id.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(())
}

/// Logical keychain reference name.
pub fn validate_keychain_name(name: &str) -> Result<(), McpConfigError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_SERVER_ID_BYTES
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    ok.then_some(()).ok_or_else(invalid)
}

fn validate_env_key(key: &str) -> Result<(), McpConfigError> {
    (key.len() <= MAX_NAME_BYTES && is_valid_env_name(key))
        .then_some(())
        .ok_or_else(invalid)
}

fn is_header_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '!' | '#'
                | '$'
                | '%'
                | '&'
                | '\''
                | '*'
                | '+'
                | '-'
                | '.'
                | '^'
                | '_'
                | '`'
                | '|'
                | '~'
        )
}

/// Header names the client itself owns; the user must not override them.
fn is_managed_header(lower: &str) -> bool {
    lower.starts_with("mcp-")
        || lower.starts_with("proxy-")
        || matches!(
            lower,
            "host"
                | "content-length"
                | "content-type"
                | "content-encoding"
                | "accept"
                | "transfer-encoding"
                | "connection"
                | "keep-alive"
                | "te"
                | "trailer"
                | "upgrade"
                | "expect"
                | "last-event-id"
        )
}

pub fn validate_header_name(name: &str) -> Result<(), McpConfigError> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES || !name.chars().all(is_header_token_char) {
        return Err(invalid());
    }
    if is_managed_header(&name.to_ascii_lowercase()) {
        return Err(invalid());
    }
    Ok(())
}

/// Applies to literal text and to values after environment expansion.
pub fn validate_header_value(value: &str) -> Result<(), McpConfigError> {
    if value.len() > MAX_STRING_BYTES {
        return Err(McpConfigError::LimitExceeded);
    }
    let ok = value.chars().all(|c| c == '\t' || !(c.is_control()));
    ok.then_some(()).ok_or_else(invalid)
}

fn validate_authority(authority: &str) -> Result<(), McpConfigError> {
    if authority.contains('@') {
        return Err(invalid());
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (host, after) = inner.split_once(']').ok_or_else(invalid)?;
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.'))
        {
            return Err(invalid());
        }
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':').ok_or_else(invalid)?),
        };
        (host, port)
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        let host_ok = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !host_ok {
            return Err(invalid());
        }
        (host, port)
    };
    let _ = host;
    if let Some(port) = port {
        let digits = !port.is_empty() && port.chars().all(|c| c.is_ascii_digit());
        match port.parse::<u16>() {
            Ok(value) if digits && value >= 1 => {}
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

/// Structural URL check: `http`/`https`, host, optional port 1..=65535; no
/// userinfo, no fragment, no whitespace/control/non-ASCII. Never resolves DNS.
/// Also used by the resolver on the expanded URL.
pub fn validate_url(url: &str) -> Result<(), McpConfigError> {
    if url.len() > MAX_STRING_BYTES {
        return Err(McpConfigError::LimitExceeded);
    }
    if !url.is_ascii() || url.chars().any(|c| c.is_control() || c == ' ' || c == '\\') {
        return Err(invalid());
    }
    let (scheme, rest) = url.split_once("://").ok_or_else(invalid)?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(invalid());
    }
    if rest.contains('#') {
        return Err(invalid());
    }
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    validate_authority(&rest[..end])
}

fn validate_template(template: &Template) -> Result<(), McpConfigError> {
    if template_len(template) > MAX_STRING_BYTES {
        return Err(McpConfigError::LimitExceeded);
    }
    if template_texts(template).any(|text| text.contains('\0')) {
        return Err(invalid());
    }
    for part in &template.0 {
        if let TemplatePart::Env { name, .. } = part
            && !is_valid_env_name(name)
        {
            return Err(McpConfigError::InvalidTemplate);
        }
    }
    Ok(())
}

fn validate_value_source(source: &ValueSource, header_value: bool) -> Result<(), McpConfigError> {
    match source {
        ValueSource::Keychain { name } => validate_keychain_name(name),
        ValueSource::Template { template } => {
            validate_template(template)?;
            if header_value {
                template_texts(template).try_for_each(validate_header_value)?;
            }
            Ok(())
        }
    }
}

fn validate_url_template(template: &Template) -> Result<(), McpConfigError> {
    validate_template(template)?;
    // A leading reference may supply the scheme; the expanded URL is re-checked.
    if matches!(template.0.first(), Some(TemplatePart::Env { .. })) {
        return Ok(());
    }
    validate_url(&with_placeholder(template, "1"))
}

fn validate_roles(roles: &[String]) -> Result<(), McpConfigError> {
    let mut seen = BTreeSet::new();
    for role in roles {
        if !MCP_ROLE_KEYS.contains(&role.as_str()) || !seen.insert(role.as_str()) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn validate_timeout(value: u64) -> Result<(), McpConfigError> {
    (MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS)
        .contains(&value)
        .then_some(())
        .ok_or_else(invalid)
}

pub fn validate_server(
    server_id: &str,
    definition: &McpServerDefinition,
) -> Result<(), McpConfigError> {
    validate_server_id(server_id)?;
    validate_roles(&definition.roles)?;
    validate_timeout(definition.startup_timeout_ms)?;
    validate_timeout(definition.request_timeout_ms)?;
    match &definition.transport {
        McpTransportDefinition::Stdio {
            command,
            args,
            cwd,
            env,
        } => {
            validate_template(command)?;
            if command.0.is_empty()
                || command
                    .0
                    .iter()
                    .all(|part| matches!(part, TemplatePart::Literal { value } if value.is_empty()))
            {
                return Err(invalid());
            }
            if args.len() > MAX_ARGS || env.len() > MAX_MAP_ENTRIES {
                return Err(McpConfigError::LimitExceeded);
            }
            args.iter().try_for_each(validate_template)?;
            if let Some(cwd) = cwd {
                validate_template(cwd)?;
            }
            for (key, value) in env {
                validate_env_key(key)?;
                validate_value_source(value, false)?;
            }
        }
        McpTransportDefinition::Http {
            url,
            headers,
            token,
        } => {
            validate_url_template(url)?;
            if headers.len() > MAX_MAP_ENTRIES {
                return Err(McpConfigError::LimitExceeded);
            }
            let mut seen = BTreeSet::new();
            for (name, value) in headers {
                let lower = name.to_ascii_lowercase();
                if lower == "authorization" {
                    if name.is_empty() || token.is_some() {
                        return Err(invalid());
                    }
                } else {
                    validate_header_name(name)?;
                }
                if !seen.insert(lower) {
                    return Err(invalid());
                }
                validate_value_source(value, true)?;
            }
            if let Some(token) = token {
                validate_value_source(token, true)?;
            }
        }
    }
    Ok(())
}
