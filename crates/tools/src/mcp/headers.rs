//! Modern HTTP metadata and validated, statically reachable parameter mirrors.
use super::{McpErrorKind, McpToolDefinition};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use std::collections::HashSet;
pub const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
pub const CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
pub const INFO_META: &str = "io.modelcontextprotocol/clientInfo";
const SAFE_INTEGER: i64 = 9_007_199_254_740_991;
#[derive(Clone)]
pub struct ParameterHeader {
    path: Vec<String>,
    name: HeaderName,
    kind: String,
}

pub fn encode_value(value: &str) -> Result<HeaderValue, McpErrorKind> {
    let plain = value
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
        && !value.starts_with([' ', '\t'])
        && !value.ends_with([' ', '\t'])
        && !(value.starts_with("=?base64?") && value.ends_with("?="));
    HeaderValue::from_str(&if plain {
        value.to_owned()
    } else {
        format!("=?base64?{}?=", STANDARD.encode(value))
    })
    .map_err(|_| McpErrorKind::Configuration)
}

pub fn parameter_headers(
    definition: &McpToolDefinition,
) -> Result<Vec<ParameterHeader>, McpErrorKind> {
    fn walk(
        value: &Value,
        path: &mut Vec<String>,
        reachable: bool,
        out: &mut Vec<ParameterHeader>,
        names: &mut HashSet<String>,
    ) -> Result<(), McpErrorKind> {
        match value {
            Value::Object(object) => {
                if let Some(annotation) = object.get("x-mcp-header") {
                    let name = annotation
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or(McpErrorKind::Protocol)?;
                    let kind = object
                        .get("type")
                        .and_then(Value::as_str)
                        .filter(|s| matches!(*s, "string" | "integer" | "boolean"))
                        .ok_or(McpErrorKind::Protocol)?;
                    if !reachable || path.is_empty() || !names.insert(name.to_ascii_lowercase()) {
                        return Err(McpErrorKind::Protocol);
                    }
                    // Validate the suffix itself, not just the prefixed name.
                    HeaderName::from_bytes(name.as_bytes()).map_err(|_| McpErrorKind::Protocol)?;
                    let header = HeaderName::from_bytes(format!("Mcp-Param-{name}").as_bytes())
                        .map_err(|_| McpErrorKind::Protocol)?;
                    out.push(ParameterHeader {
                        path: path.clone(),
                        name: header,
                        kind: kind.into(),
                    });
                }
                for (key, child) in object {
                    if key == "properties" {
                        if let Some(properties) = child.as_object() {
                            for (name, schema) in properties {
                                path.push(name.clone());
                                walk(schema, path, reachable, out, names)?;
                                path.pop();
                            }
                        }
                    } else if key != "x-mcp-header" {
                        walk(child, path, false, out, names)?;
                    }
                }
            }
            Value::Array(array) => {
                for child in array {
                    walk(child, path, false, out, names)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(
        &Value::Object(definition.input_schema.clone()),
        &mut Vec::new(),
        true,
        &mut out,
        &mut HashSet::new(),
    )?;
    Ok(out)
}

pub fn request_headers(
    body: &Value,
    mappings: &[ParameterHeader],
) -> Result<HeaderMap, McpErrorKind> {
    let mut out = HeaderMap::new();
    out.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    out.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(version) = body
        .pointer("/params/_meta")
        .and_then(|m| m.get(VERSION_META))
        .and_then(Value::as_str)
    {
        out.insert(
            "mcp-protocol-version",
            HeaderValue::from_str(version).map_err(|_| McpErrorKind::Protocol)?,
        );
        let method = body
            .get("method")
            .and_then(Value::as_str)
            .ok_or(McpErrorKind::Protocol)?;
        out.insert(
            "mcp-method",
            HeaderValue::from_str(method).map_err(|_| McpErrorKind::Protocol)?,
        );
        if method == "tools/call" {
            let name = body
                .pointer("/params/name")
                .and_then(Value::as_str)
                .ok_or(McpErrorKind::Configuration)?;
            out.insert("mcp-name", encode_value(name)?);
            for mapping in mappings {
                let mut current = body.pointer("/params/arguments");
                for key in &mapping.path {
                    current = current.and_then(|v| v.get(key));
                }
                let Some(value) = current.filter(|v| !v.is_null()) else {
                    continue;
                };
                let text = match mapping.kind.as_str() {
                    "string" => value.as_str().map(str::to_owned),
                    "boolean" => value.as_bool().map(|v| v.to_string()),
                    "integer" => value
                        .as_f64()
                        .filter(|v| {
                            v.is_finite() && v.fract() == 0.0 && v.abs() <= SAFE_INTEGER as f64
                        })
                        .map(|v| (v as i64).to_string()),
                    _ => None,
                }
                .ok_or(McpErrorKind::Configuration)?;
                out.insert(mapping.name.clone(), encode_value(&text)?);
            }
        }
    }
    Ok(out)
}
