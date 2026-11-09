//! Worker A: shared non-recursive template grammar for JSON and TOML.
//!
//! | source        | meaning                                            |
//! | ------------- | -------------------------------------------------- |
//! | `${VAR}`      | value of `VAR`; unset is an error                  |
//! | `${VAR:-def}` | value of `VAR`; `def` (literal) when unset or empty |
//! | `$${VAR}`     | the literal text `${VAR}`                          |
//!
//! Anything else (`$HOME`, a lone `$`, `$$` not followed by `{`) is literal.
//! There is no nesting, no recursion, no command substitution: an expanded
//! value or a default is never scanned again. The AST keeps literal parts and
//! references separate because approval fingerprints must not see resolved values.
use super::{MAX_STRING_BYTES, McpConfigError, Template, TemplatePart};

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// A valid environment variable reference name.
pub fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(is_name_start) && chars.all(|c| is_name_start(c) || c.is_ascii_digit())
}

fn push_literal(parts: &mut Vec<TemplatePart>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal { value }) = parts.last_mut() {
        value.push_str(text);
    } else {
        parts.push(TemplatePart::Literal {
            value: text.to_owned(),
        });
    }
}

/// Parse the source syntax into a template AST without consulting any environment.
pub fn parse_template(input: &str) -> Result<Template, McpConfigError> {
    if input.len() > MAX_STRING_BYTES {
        return Err(McpConfigError::LimitExceeded);
    }
    let mut parts: Vec<TemplatePart> = Vec::new();
    let mut rest = input;
    while let Some(index) = rest.find('$') {
        push_literal(&mut parts, &rest[..index]);
        rest = &rest[index..];
        if let Some(after) = rest.strip_prefix("$${") {
            // Escape: the three characters stand for the literal `${`.
            push_literal(&mut parts, "${");
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let end = after.find('}').ok_or(McpConfigError::InvalidTemplate)?;
            let body = &after[..end];
            let (name, default) = match body.split_once(":-") {
                Some((name, default)) => (name, Some(default)),
                None => (body, None),
            };
            if !is_valid_env_name(name) || default.is_some_and(|d| d.contains("${")) {
                return Err(McpConfigError::InvalidTemplate);
            }
            parts.push(TemplatePart::Env {
                name: name.to_owned(),
                default: default.map(str::to_owned),
            });
            rest = &after[end + 1..];
        } else {
            push_literal(&mut parts, "$");
            rest = &rest[1..];
        }
    }
    push_literal(&mut parts, rest);
    Ok(Template(parts))
}

/// Explicit lookup keeps tests/process environment isolated; never calls set_var.
pub fn expand_template(
    template: &Template,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, McpConfigError> {
    let mut out = String::new();
    for part in &template.0 {
        match part {
            TemplatePart::Literal { value } => out.push_str(value),
            TemplatePart::Env { name, default } => match (lookup(name), default) {
                (Some(value), _) if !value.is_empty() => out.push_str(&value),
                (_, Some(default)) => out.push_str(default),
                (Some(value), None) => out.push_str(&value),
                (None, None) => return Err(McpConfigError::MissingEnvironment),
            },
        }
        if out.len() > MAX_STRING_BYTES {
            return Err(McpConfigError::LimitExceeded);
        }
    }
    Ok(out)
}

/// Render the template back to the source syntax accepted by [`parse_template`].
///
/// Fails when the AST has no source spelling (invalid reference name, default
/// containing `}` or `${`, or a literal ending in `$` directly before a
/// reference, which would read as an escape).
pub fn render_template(template: &Template) -> Result<String, McpConfigError> {
    // Merge adjacent literals first so that boundary checks see the real text.
    let mut merged: Vec<TemplatePart> = Vec::new();
    for part in &template.0 {
        match part {
            TemplatePart::Literal { value } => push_literal(&mut merged, value),
            env @ TemplatePart::Env { .. } => merged.push(env.clone()),
        }
    }
    let mut out = String::new();
    let mut previous_dollar = false;
    for part in &merged {
        match part {
            TemplatePart::Literal { value } => {
                out.push_str(&value.replace("${", "$${"));
                previous_dollar = value.ends_with('$');
            }
            TemplatePart::Env { name, default } => {
                if previous_dollar
                    || !is_valid_env_name(name)
                    || default
                        .as_deref()
                        .is_some_and(|d| d.contains('}') || d.contains("${"))
                {
                    return Err(McpConfigError::InvalidTemplate);
                }
                out.push_str("${");
                out.push_str(name);
                if let Some(default) = default {
                    out.push_str(":-");
                    out.push_str(default);
                }
                out.push('}');
                previous_dollar = false;
            }
        }
    }
    Ok(out)
}

/// Total source-visible bytes (literals, reference names and defaults).
pub(crate) fn template_len(template: &Template) -> usize {
    template
        .0
        .iter()
        .map(|part| match part {
            TemplatePart::Literal { value } => value.len(),
            TemplatePart::Env { name, default } => {
                name.len() + default.as_deref().map_or(0, str::len)
            }
        })
        .sum()
}

/// Every literal/default text, for character-class checks.
pub(crate) fn template_texts(template: &Template) -> impl Iterator<Item = &str> {
    template
        .0
        .iter()
        .flat_map(|part| match part {
            TemplatePart::Literal { value } => [Some(value.as_str()), None],
            TemplatePart::Env { default, .. } => [default.as_deref(), None],
        })
        .flatten()
}

/// The template with each reference replaced by `placeholder` (structure probes).
pub(crate) fn with_placeholder(template: &Template, placeholder: &str) -> String {
    template
        .0
        .iter()
        .map(|part| match part {
            TemplatePart::Literal { value } => value.as_str(),
            TemplatePart::Env { .. } => placeholder,
        })
        .collect()
}
