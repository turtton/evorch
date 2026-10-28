//! TypeScript declarations of the `tools` API, generated from the tools' JSON
//! schemas (ADR 0039: the description and the implementation must not drift).
//!
//! Output is deterministic: tools are sorted by name and object properties keep
//! the schema's (sorted) key order, so the description is byte-identical across
//! the turns of a run.

use serde_json::Value;

const RESULT_TYPE: &str = "type Result = { ok: boolean; text: string; truncated: boolean; next: { tool: \"read\"; path: string; offset?: number; byte_offset?: number } | null; detail: any };";

/// Arguments that only matter for calls outside `run_js` (access review).
/// What a program may pass to `shell`.
const SHELL_PROPERTIES: [&str; 3] = ["command", "cwd", "timeout_ms"];

const HIDDEN_PROPERTIES: [&str; 2] = ["sandbox_access", "justification"];

/// Declarations for `tools` (sorted by name) given each callable tool's schema.
pub fn declarations(tools: &[(String, Value)]) -> String {
    let mut sorted: Vec<&(String, Value)> = tools.iter().collect();
    sorted.sort_by(|left, right| left.0.cmp(&right.0));
    let mut out = String::from(RESULT_TYPE);
    out.push_str("\ndeclare const tools: {\n");
    for (name, schema) in &sorted {
        let schema = &callable_schema(name, schema);
        let arguments = object_type(schema);
        let optional = if has_required(schema) { "" } else { "?" };
        out.push_str(&format!(
            "  {name}(args{optional}: {arguments}): Promise<Result>;\n"
        ));
    }
    out.push_str("};");
    if sorted.iter().any(|(name, _)| name == "read") {
        out.push_str("\ndeclare function readText(path: string): Promise<string>;");
    }
    out
}

/// The part of a tool's schema a program may use. `read` enforces `path` at run
/// time but not in its schema; `shell` shrinks to starting a command, because job
/// control, interactive use and non-isolated access are not available here.
fn callable_schema(name: &str, schema: &Value) -> Value {
    let mut schema = schema.clone();
    match name {
        "read" if schema.get("required").is_none() => {
            schema["required"] = serde_json::json!(["path"]);
        }
        "shell" => {
            if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                properties.retain(|key, _| SHELL_PROPERTIES.contains(&key.as_str()));
            }
            schema["required"] = serde_json::json!(["command"]);
        }
        _ => {}
    }
    schema
}

fn has_required(schema: &Value) -> bool {
    schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| !required.is_empty())
}

fn object_type(schema: &Value) -> String {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return "Record<string, unknown>".to_owned();
    };
    let fields: Vec<String> = properties
        .iter()
        .filter(|(name, _)| !HIDDEN_PROPERTIES.contains(&name.as_str()))
        .map(|(name, property)| {
            let optional = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            format!("{name}{optional}: {}", type_of(property))
        })
        .collect();
    if fields.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{ {} }}", fields.join("; "))
    }
}

fn type_of(schema: &Value) -> String {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return union(values.iter().map(Value::to_string).collect());
    }
    if let Some(value) = schema.get("const") {
        return value.to_string();
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(variants) = schema.get(key).and_then(Value::as_array) {
            return union(variants.iter().map(type_of).collect());
        }
    }
    match schema.get("type") {
        Some(Value::String(kind)) => named_type(kind, schema),
        Some(Value::Array(kinds)) => union(
            kinds
                .iter()
                .filter_map(Value::as_str)
                .map(|kind| named_type(kind, schema))
                .collect(),
        ),
        _ => "unknown".to_owned(),
    }
}

fn named_type(kind: &str, schema: &Value) -> String {
    match kind {
        "string" => "string".to_owned(),
        "integer" | "number" => "number".to_owned(),
        "boolean" => "boolean".to_owned(),
        "null" => "null".to_owned(),
        "array" => {
            let item = schema.get("items").map_or("unknown".to_owned(), type_of);
            if item.contains(" | ") {
                format!("({item})[]")
            } else {
                format!("{item}[]")
            }
        }
        "object" => object_type(schema),
        _ => "unknown".to_owned(),
    }
}

fn union(mut variants: Vec<String>) -> String {
    variants.dedup();
    if variants.is_empty() {
        "never".to_owned()
    } else {
        variants.join(" | ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use serde_json::json;

    fn schemas() -> Vec<(String, Value)> {
        vec![
            ("write".to_owned(), crate::tools::Write.schema()),
            ("read".to_owned(), crate::tools::Read.schema()),
            ("grep".to_owned(), crate::tools::Grep.schema()),
            ("edit".to_owned(), crate::tools::Edit.schema()),
            (
                "git_diff".to_owned(),
                crate::tools::GitDiff::new(std::sync::Arc::new(
                    sandbox::DirectSandbox::new_unchecked(),
                ))
                .schema(),
            ),
            (
                "shell".to_owned(),
                crate::tools::Shell::new(std::sync::Arc::new(
                    sandbox::DirectSandbox::new_unchecked(),
                ))
                .schema(),
            ),
        ]
    }

    #[test]
    fn declarations_for_the_standard_tools_are_pinned() {
        let expected = "\
type Result = { ok: boolean; text: string; truncated: boolean; next: { tool: \"read\"; path: string; offset?: number; byte_offset?: number } | null; detail: any };
declare const tools: {
  edit(args: { edits?: { new_string: string; old_string: string; replace_all?: boolean }[]; new_string?: string; old_string?: string; path: string; replace_all?: boolean }): Promise<Result>;
  git_diff(args?: { cwd?: string; path?: string }): Promise<Result>;
  grep(args: { path: string; pattern: string }): Promise<Result>;
  read(args: { byte_offset?: number; limit?: number; offset?: number; path: string }): Promise<Result>;
  shell(args: { command: string; cwd?: string; timeout_ms?: number }): Promise<Result>;
  write(args: { content: string; path: string }): Promise<Result>;
};
declare function readText(path: string): Promise<string>;";
        assert_eq!(declarations(&schemas()), expected);
    }

    #[test]
    fn declarations_do_not_depend_on_input_order() {
        let mut reversed = schemas();
        reversed.reverse();
        assert_eq!(declarations(&reversed), declarations(&schemas()));
    }

    #[test]
    fn shell_is_projected_from_its_schema_to_starting_a_command() {
        let mut schema = schemas()
            .into_iter()
            .find(|(name, _)| name == "shell")
            .unwrap()
            .1;
        schema["properties"]["timeout_ms"] = json!({"type": "string"});
        schema["properties"]["job_id"] = json!({"type": "string"});
        let declared = declarations(&[("shell".to_owned(), schema)]);
        assert!(
            declared
                .contains("shell(args: { command: string; cwd?: string; timeout_ms?: string })"),
            "{declared}"
        );
        assert!(!declared.contains("job_id"));
    }

    #[test]
    fn read_text_is_declared_only_with_read() {
        let without_read: Vec<_> = schemas()
            .into_iter()
            .filter(|(name, _)| name != "read")
            .collect();
        assert!(!declarations(&without_read).contains("readText"));
    }

    #[test]
    fn schema_constructs_map_to_typescript() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mode": {"enum": ["a", "b"]},
                "n": {"type": ["integer", "null"]},
                "list": {"type": "array", "items": {"oneOf": [{"type": "string"}, {"type": "number"}]}},
                "free": {}
            },
            "required": ["mode"]
        });
        assert_eq!(
            object_type(&schema),
            "{ free?: unknown; list?: (string | number)[]; mode: \"a\" | \"b\"; n?: number | null }"
        );
        assert_eq!(
            object_type(&json!({"type": "object"})),
            "Record<string, unknown>"
        );
    }
}
