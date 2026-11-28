//! The Codex Responses Lite backend compiles every tool schema as a strict schema and
//! answers HTTP 400 `invalid_json_schema` ("Unimplemented keys") for the keywords it does
//! not implement. These tests serialize the tools the runtime itself owns the way a Lite
//! request does and fail on any such keyword, so a new tool cannot silently break Codex.

use providers::wire::responses::{ResponsesRequestOptions, to_wire_request};
use providers::{ChatRequest, CodexModelCapabilities, ResponsesDialect, ToolSpec};
use serde_json::{Value, json};

use super::registry::{META_OPS, MetaOp};

/// Keywords the backend reported as unimplemented (`uniqueItems`, `if` / `then` / `else` in
/// #816, `not` for `wait` afterwards). Kept literal so the test does not depend on the list
/// the sanitizer itself uses.
const REJECTED: [&str; 5] = ["not", "uniqueItems", "if", "then", "else"];

/// Schema-valued positions, so a property that is merely *named* `not` is not a keyword.
fn violations(schema: &Value, path: &str, found: &mut Vec<String>) {
    let Value::Object(map) = schema else { return };
    for (key, child) in map {
        let here = format!("{path}.{key}");
        if REJECTED.contains(&key.as_str()) {
            found.push(here.clone());
        }
        match (key.as_str(), child) {
            (
                "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas",
                Value::Object(named),
            ) => {
                for (name, value) in named {
                    violations(value, &format!("{here}.{name}"), found);
                }
            }
            ("dependencies", Value::Object(named)) => {
                for (name, value) in named.iter().filter(|(_, value)| value.is_object()) {
                    violations(value, &format!("{here}.{name}"), found);
                }
            }
            ("allOf" | "anyOf" | "oneOf" | "prefixItems", Value::Array(branches)) => {
                for (index, value) in branches.iter().enumerate() {
                    violations(value, &format!("{here}[{index}]"), found);
                }
            }
            (
                "items"
                | "additionalItems"
                | "additionalProperties"
                | "unevaluatedItems"
                | "unevaluatedProperties"
                | "contains"
                | "propertyNames",
                Value::Array(tuple),
            ) => {
                for (index, value) in tuple.iter().enumerate() {
                    violations(value, &format!("{here}[{index}]"), found);
                }
            }
            (
                "items"
                | "additionalItems"
                | "additionalProperties"
                | "unevaluatedItems"
                | "unevaluatedProperties"
                | "contains"
                | "propertyNames",
                child,
            ) => violations(child, &here, found),
            _ => {}
        }
    }
}

fn lite_wire(tools: Vec<ToolSpec>) -> Value {
    let request: ChatRequest = serde_json::from_value(json!({
        "model": "gpt-6-astra",
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "rules"}]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        ],
        "tools": tools,
    }))
    .unwrap();
    serde_json::to_value(to_wire_request(
        &request,
        &ResponsesRequestOptions {
            dialect: ResponsesDialect::Codex,
            prompt_cache_key: None,
            codex_model: Some(CodexModelCapabilities {
                use_responses_lite: true,
                ..CodexModelCapabilities::default()
            }),
        },
    ))
    .unwrap()
}

// Given: every runtime meta tool, every compiled standard tool and the metadata of the
// optional browser, web and LSP tools
// When: serialized as a Codex Responses Lite request (`additional_tools` namespace)
// Then: no tool's `parameters` carries a keyword the strict schema check rejects
#[test]
fn every_runtime_owned_tool_is_free_of_codex_lite_unsupported_keywords() {
    let mut tools: Vec<ToolSpec> = META_OPS
        .iter()
        .map(|name| MetaOp::from_name(name).unwrap().spec())
        .collect();
    tools.extend(crate::compiled_standard_tool_catalog());
    tools.extend(
        tools::compiled_catalog::optional_tools()
            .into_iter()
            .map(|spec| ToolSpec {
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
            }),
    );
    let expected = tools.len();
    let wire = lite_wire(tools);
    let sent = wire["input"][0]["tools"][0]["tools"].as_array().unwrap();
    assert_eq!(sent.len(), expected, "every tool is on the wire");
    let mut found = Vec::new();
    for tool in sent {
        violations(
            &tool["parameters"],
            tool["name"].as_str().unwrap(),
            &mut found,
        );
    }
    assert!(found.is_empty(), "unsupported keywords sent: {found:?}");
}

// Given: the `wait` tool, whose exclusivity of run_id / run_ids is a `oneOf` of `not` branches
// When: the Lite wire copy drops them
// Then: the canonical definition keeps them (the handler still rejects run_id together with run_ids)
#[test]
fn the_canonical_wait_schema_keeps_its_exclusivity_constraint() {
    let spec = MetaOp::from_name("wait").unwrap().spec();
    let mut found = Vec::new();
    violations(&spec.input_schema, "wait", &mut found);
    assert!(!found.is_empty(), "the canonical schema is not weakened");
}

// Given: a schema with `not` inside tuple-form `items` and `dependencies`, and a property named `not`
// When: the scanner above walks it
// Then: it reports exactly the keyword positions, so a blind spot cannot hide a rejected keyword
#[test]
fn the_scanner_sees_tuple_items_and_dependencies_but_not_property_names() {
    let schema = json!({
        "properties": {"not": {"type": "string"}},
        "items": [{"type": "string"}, {"not": {"type": "null"}}],
        "dependencies": {"a": {"uniqueItems": true}, "b": ["not"]}
    });
    let mut found = Vec::new();
    violations(&schema, "t", &mut found);
    found.sort();
    assert_eq!(
        found,
        ["t.dependencies.a.uniqueItems", "t.items[1].not"].map(String::from)
    );
}
