//! Catalog capabilities decide the Codex request shape (Responses Lite and friends).

use serde_json::{Value, json};

use super::{ResponsesDialect, ResponsesRequestOptions, to_wire_request};
use crate::message::ChatRequest;
use crate::models::CodexModelCapabilities;

/// The capabilities `gpt-6-astra` advertises in Codex CLI 0.166.0's `models.json`.
fn astra() -> CodexModelCapabilities {
    CodexModelCapabilities {
        use_responses_lite: true,
        default_reasoning_level: Some("low".into()),
        supported_reasoning_levels: ["low", "medium", "high", "xhigh", "max", "ultra"]
            .map(String::from)
            .into(),
        support_verbosity: true,
        default_verbosity: Some("low".into()),
        priority_tier: Some(true),
        ..CodexModelCapabilities::default()
    }
}

fn request(tools: &[&str]) -> ChatRequest {
    let tools: Vec<_> = tools
        .iter()
        .map(|name| {
            json!({"name": name, "description": format!("{name} tool"),
            "input_schema": {"type": "object"}})
        })
        .collect();
    serde_json::from_value(json!({
        "model": "gpt-6-astra",
        "service_tier": "priority",
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "Follow the rules."}]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        ],
        "tools": tools
    }))
    .unwrap()
}

fn wire_with(request: &ChatRequest, model: Option<CodexModelCapabilities>) -> Value {
    serde_json::to_value(to_wire_request(
        request,
        &ResponsesRequestOptions {
            dialect: ResponsesDialect::Codex,
            prompt_cache_key: None,
            codex_model: model,
        },
    ))
    .unwrap()
}

// Given: models the catalog does not describe (no catalog, or default capabilities)
// When: serialized with the Codex dialect
// Then: the bytes are identical, so a failed catalog keeps the pre-catalog wire shape
#[test]
fn unknown_capabilities_keep_the_legacy_shape_byte_for_byte() {
    for tools in [&[][..], &["alpha", "zeta"][..]] {
        let request = request(tools);
        let bytes = |model| {
            serde_json::to_string(&to_wire_request(
                &request,
                &ResponsesRequestOptions {
                    dialect: ResponsesDialect::Codex,
                    prompt_cache_key: Some("run-1".into()),
                    codex_model: model,
                },
            ))
            .unwrap()
        };
        assert_eq!(bytes(None), bytes(Some(CodexModelCapabilities::default())));
    }
}

// Given: a Lite model with two tools
// When: serialized
// Then: tools move into an additional_tools item ahead of the developer message, the
// top-level tools disappear, parallel calls are off, and reasoning carries all_turns
#[test]
fn lite_model_moves_tools_into_additional_tools_before_the_developer_message() {
    let value = wire_with(&request(&["zeta", "alpha"]), Some(astra()));

    assert!(value.get("tools").is_none(), "{value}");
    assert_eq!(value["tool_choice"], "auto");
    assert_eq!(value["parallel_tool_calls"], false);
    assert_eq!(
        value["reasoning"],
        json!({"effort": "low", "summary": "auto", "context": "all_turns"})
    );
    let input = value["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "{value}");
    let tools = &input[0];
    assert_eq!(tools["type"], "additional_tools");
    assert_eq!(tools["role"], "developer");
    assert!(tools["id"].as_str().unwrap().starts_with("at_"));
    assert_eq!(
        tools["tools"],
        json!([{"type": "namespace", "name": "functions", "description": "", "tools": [
            {"type": "function", "name": "alpha", "description": "alpha tool",
                "parameters": {"type": "object"}},
            {"type": "function", "name": "zeta", "description": "zeta tool",
                "parameters": {"type": "object"}}
        ]}])
    );
    assert_eq!(input[1]["role"], "developer");
    assert_eq!(input[2]["role"], "user");
}

// Given: a Lite model / When: the request is built
// Then: the wire request reports it, so the transport adds the Lite header
#[test]
fn only_lite_requests_report_responses_lite() {
    let lite = to_wire_request(
        &request(&["alpha"]),
        &ResponsesRequestOptions {
            dialect: ResponsesDialect::Codex,
            prompt_cache_key: None,
            codex_model: Some(astra()),
        },
    );
    let plain = to_wire_request(
        &request(&["alpha"]),
        &ResponsesRequestOptions {
            dialect: ResponsesDialect::Codex,
            prompt_cache_key: None,
            codex_model: Some(CodexModelCapabilities::default()),
        },
    );
    assert!(lite.is_responses_lite());
    assert!(!plain.is_responses_lite());
}

// Given: Lite requests built from the same tools
// When: the history, instructions or tool order differ
// Then: the additional_tools item (id included) is identical; other tools change the id
#[test]
fn additional_tools_id_depends_only_on_the_serialized_tools() {
    let id_and_item = |request: &ChatRequest| {
        let value = wire_with(request, Some(astra()));
        value["input"][0].clone()
    };
    let base = id_and_item(&request(&["alpha", "zeta"]));
    let mut longer = request(&["zeta", "alpha"]);
    longer.messages.pop();
    let mut changed_instructions = request(&["alpha", "zeta"]);
    changed_instructions.messages.remove(0);

    assert_eq!(id_and_item(&longer), base);
    assert_eq!(id_and_item(&changed_instructions), base);
    assert_ne!(id_and_item(&request(&["alpha"])), base);
}

// Given: a Lite model with no tools
// When: serialized
// Then: no additional_tools item is emitted and tools stay absent
#[test]
fn lite_model_without_tools_has_no_additional_tools_item() {
    let value = wire_with(&request(&[]), Some(astra()));

    assert!(value.get("tools").is_none());
    assert_eq!(value["parallel_tool_calls"], false);
    let input = value["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["role"], "developer");
    assert_ne!(input[0]["type"], "additional_tools");
}

// Given: a model that is not Lite
// When: its capabilities are described by the catalog
// Then: tools stay top-level, calls stay parallel, and reasoning has no context
#[test]
fn non_lite_catalog_model_keeps_top_level_tools_and_parallel_calls() {
    let model = CodexModelCapabilities {
        default_reasoning_level: Some("high".into()),
        ..CodexModelCapabilities::default()
    };
    let value = wire_with(&request(&["alpha"]), Some(model));

    assert_eq!(value["tools"].as_array().unwrap().len(), 1);
    assert_eq!(value["parallel_tool_calls"], true);
    assert_eq!(
        value["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(value["input"][0]["role"], "developer");
}

// Given: explicit, model-default and absent reasoning effort
// When: serialized
// Then: explicit wins, then the model default, then medium
#[test]
fn reasoning_effort_resolves_explicit_then_model_default_then_medium() {
    let mut explicit = request(&[]);
    explicit.reasoning_effort = Some("xhigh".into());

    assert_eq!(
        wire_with(&explicit, Some(astra()))["reasoning"]["effort"],
        "xhigh"
    );
    assert_eq!(
        wire_with(&request(&[]), Some(astra()))["reasoning"]["effort"],
        "low"
    );
    assert_eq!(
        wire_with(&request(&[]), Some(CodexModelCapabilities::default()))["reasoning"]["effort"],
        "medium"
    );
}

// Given: a model that does not accept reasoning.summary (the catalog's default summary is
// ignored in favour of this flag)
// When: serialized, Lite or not
// Then: summary is omitted; Lite still carries its context
#[test]
fn reasoning_summary_follows_the_support_flag_only() {
    for lite in [false, true] {
        let model = CodexModelCapabilities {
            use_responses_lite: lite,
            supports_reasoning_summary_parameter: false,
            ..CodexModelCapabilities::default()
        };
        let reasoning = wire_with(&request(&[]), Some(model))["reasoning"].clone();
        let expected = if lite {
            json!({"effort": "medium", "context": "all_turns"})
        } else {
            json!({"effort": "medium"})
        };
        assert_eq!(reasoning, expected);
    }
}

// Given: models with and without verbosity support / When: serialized, with and without
// an output schema
// Then: supported models send their default verbosity before the schema format
#[test]
fn verbosity_follows_the_model_default_only_when_supported() {
    let value = wire_with(&request(&[]), Some(astra()));
    assert_eq!(value["text"], json!({"verbosity": "low"}));

    let mut structured = request(&[]);
    structured.output_schema = Some(
        serde_json::from_value(json!({"name": "verdict", "schema": {"type": "object"}})).unwrap(),
    );
    let json = serde_json::to_string(&to_wire_request(
        &structured,
        &ResponsesRequestOptions {
            dialect: ResponsesDialect::Codex,
            prompt_cache_key: None,
            codex_model: Some(astra()),
        },
    ))
    .unwrap();
    let verbosity = json.find("\"verbosity\"").expect("verbosity");
    let format = json.find("\"format\"").expect("format");
    assert!(verbosity < format, "{json}");

    let unsupported = CodexModelCapabilities {
        support_verbosity: false,
        default_verbosity: Some("high".into()),
        ..CodexModelCapabilities::default()
    };
    assert!(
        wire_with(&request(&[]), Some(unsupported))
            .get("text")
            .is_none()
    );
}

// Given: a priority request / When: the model's catalog entry was fetched and advertises
// priority, does not, or no entry was fetched (`None`: no catalog, failure, unlisted)
// Then: it is omitted exactly for a fetched entry that does not advertise priority
#[test]
fn service_tier_is_sent_only_for_an_advertised_priority_or_an_unfetched_entry() {
    for (advertised, expected) in [
        (Some(true), Some("priority")),
        (None, Some("priority")),
        (Some(false), None),
    ] {
        let model = CodexModelCapabilities {
            priority_tier: advertised,
            ..CodexModelCapabilities::default()
        };
        let value = wire_with(&request(&[]), Some(model));
        assert_eq!(value["service_tier"].as_str(), expected, "{advertised:?}");
    }
}

// Given: capabilities that would change a Codex request / When: the OpenAI dialect builds
// Then: they have no effect
#[test]
fn openai_dialect_ignores_codex_capabilities() {
    let build = |model| {
        serde_json::to_value(to_wire_request(
            &request(&["alpha"]),
            &ResponsesRequestOptions {
                dialect: ResponsesDialect::OpenAi,
                prompt_cache_key: None,
                codex_model: model,
            },
        ))
        .unwrap()
    };
    assert_eq!(build(None), build(Some(astra())));
}
fn schema_request(schema: Value) -> ChatRequest {
    serde_json::from_value(json!({
        "model": "gpt-6.2-astra",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        "tools": [{"name": "present", "description": "d", "input_schema": schema}]
    }))
    .unwrap()
}

fn has_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| has_key(v, key)),
        Value::Array(items) => items.iter().any(|v| has_key(v, key)),
        _ => false,
    }
}

/// The first function tool of a serialized request, wherever the shape puts it.
fn first_tool_parameters(request: &Value) -> Value {
    let tool = if request.get("tools").is_some() {
        &request["tools"][0]
    } else {
        &request["input"][0]["tools"][0]["tools"][0]
    };
    tool["parameters"].clone()
}

fn mixed_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ids": {"type": "array", "uniqueItems": true, "minItems": 1, "maxItems": 8,
                "items": {"type": "string", "minLength": 1, "pattern": "^a"}},
            "nested": {"type": "object", "minProperties": 1,
                "properties": {"tags": {"type": "array", "uniqueItems": true,
                    "items": {"anyOf": [{"type": "array", "uniqueItems": true}]}}}},
            "uniqueItems": {"type": "boolean", "default": {"uniqueItems": true}}
        },
        "required": ["ids"],
        "additionalProperties": false,
        "allOf": [{
            "if": {"properties": {"ids": {"minItems": 2}}},
            "then": {"required": ["nested"], "not": {"required": ["uniqueItems"]}},
            "else": {"required": ["uniqueItems"]}
        }],
        "oneOf": [{"required": ["ids"]}, {"not": {"required": ["ids"]}}]
    })
}

// Given: a schema with the keywords the backend reported as unimplemented (uniqueItems,
// if/then/else, not), nested in properties, items, anyOf and allOf, next to keywords it accepted
// (allOf, anyOf, oneOf, min/max items and length, pattern) and one never observed
// (minProperties), plus a property that is merely named `uniqueItems`
// When: serialized for a Lite Codex model
// Then: the rejected keywords are gone everywhere (the oneOf whose branch lost its `not` goes
// with it), the accepted ones and the property survive, and the canonical tool definition
// is untouched
#[test]
fn lite_tool_schemas_drop_only_the_keywords_the_backend_rejects() {
    let request = schema_request(mixed_schema());
    let parameters = first_tool_parameters(&wire_with(&request, Some(astra())));

    for key in ["if", "then", "else"] {
        assert!(!has_key(&parameters["allOf"], key), "{key}");
    }
    assert!(!has_key(&parameters["properties"]["ids"], "uniqueItems"));
    assert!(!has_key(&parameters["properties"]["nested"], "uniqueItems"));
    assert_eq!(parameters["properties"]["nested"]["minProperties"], 1);
    let ids = &parameters["properties"]["ids"];
    assert_eq!(ids["minItems"], 1);
    assert_eq!(ids["maxItems"], 8);
    assert_eq!(
        ids["items"],
        json!({"type": "string", "minLength": 1, "pattern": "^a"})
    );
    assert_eq!(
        parameters["properties"]["uniqueItems"],
        json!({"type": "boolean", "default": {"uniqueItems": true}})
    );
    assert!(parameters["properties"]["nested"]["properties"]["tags"]["items"]["anyOf"].is_array());
    assert!(parameters.get("oneOf").is_none());
    assert!(parameters["allOf"].is_array());
    assert_eq!(parameters["required"], json!(["ids"]));
    assert_eq!(request.tools[0].input_schema, mixed_schema());
}

// Given: the same schema / When: serialized without Lite (a non-Lite model or no catalog)
// or with the official OpenAI dialect
// Then: it is sent as is, so established conversations keep their cached tool prefix
#[test]
fn non_lite_and_openai_tool_schemas_are_untouched() {
    let request = schema_request(mixed_schema());
    for model in [Some(CodexModelCapabilities::default()), None] {
        let parameters = first_tool_parameters(&wire_with(&request, model));
        assert_eq!(parameters, mixed_schema());
    }
    let openai = serde_json::to_value(to_wire_request(
        &request,
        &ResponsesRequestOptions {
            dialect: ResponsesDialect::OpenAi,
            prompt_cache_key: None,
            codex_model: Some(astra()),
        },
    ))
    .unwrap();
    assert_eq!(first_tool_parameters(&openai), mixed_schema());
}

// Given: a Lite tool schema with none of the observed-rejected keywords, but with keywords
// that were never observed being rejected (minProperties, contains, dependentRequired, ...)
// When: serialized for a Lite Codex model
// Then: the schema is sent byte for byte as it is
#[test]
fn lite_schemas_without_rejected_keywords_are_unchanged() {
    let schema = json!({
        "type": "object",
        "properties": {
            "tags": {"type": "array", "minItems": 1, "contains": {"type": "string"},
                "minContains": 1, "maxContains": 2, "unevaluatedItems": false,
                "items": {"type": "string"}},
            "meta": {"type": "object", "minProperties": 1, "maxProperties": 4,
                "propertyNames": {"pattern": "^a"}, "patternProperties": {"^x": {"type": "string"}},
                "unevaluatedProperties": false}
        },
        "dependentRequired": {"tags": ["meta"]},
        "dependentSchemas": {"meta": {"required": ["tags"]}},
        "allOf": [{"required": ["meta"]}],
        "oneOf": [{"required": ["tags"]}, {"required": ["meta"]}]
    });
    let parameters =
        first_tool_parameters(&wire_with(&schema_request(schema.clone()), Some(astra())));
    assert_eq!(
        serde_json::to_string(&parameters).unwrap(),
        serde_json::to_string(&schema).unwrap()
    );
}

// Given: a Lite tool schema that uses the rejected keywords' names as data (property names,
// patternProperties and dependentRequired/dependentSchemas keys, required entries, enum, const
// and default values) but has none of them as keywords
// When: serialized for a Lite Codex model
// Then: the schema is sent byte for byte as it is
#[test]
fn lite_schemas_keep_rejected_keyword_names_used_as_data() {
    let schema = json!({
        "type": "object",
        "properties": {
            "if": {"type": "string", "enum": ["then", "else"], "default": "uniqueItems"},
            "then": {"type": "object", "properties": {"else": {"const": "if"}}},
            "uniqueItems": {"type": "array", "items": {"type": "string"}}
        },
        "patternProperties": {"^if$": {"type": "string"}, "else": {"type": "integer"}},
        "dependentRequired": {"if": ["then", "uniqueItems"]},
        "dependentSchemas": {"else": {"properties": {"then": {"type": "string"}}}},
        "required": ["if", "then"],
        "$defs": {"uniqueItems": {"type": "string"}},
        "examples": [{"if": 1, "uniqueItems": true}]
    });
    let parameters =
        first_tool_parameters(&wire_with(&schema_request(schema.clone()), Some(astra())));
    assert_eq!(parameters, schema);
}

// Given: rejected keywords at every schema position (items, additionalProperties, the
// branches of anyOf, dependentSchemas and patternProperties values, $defs)
// When: serialized for a Lite Codex model
// Then: they are all removed, and the names around them are kept
#[test]
fn lite_schemas_lose_rejected_keywords_at_every_schema_position() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "array", "items": {"type": "array", "uniqueItems": true}}},
        "additionalProperties": {"type": "array", "uniqueItems": true},
        "anyOf": [{"type": "array", "uniqueItems": true}],
        "dependentSchemas": {"a": {"if": {}, "else": {}}},
        "patternProperties": {"^x": {"uniqueItems": true}},
        "$defs": {"d": {"uniqueItems": true}},
        "dependentRequired": {"a": ["b"]}
    });
    let parameters = first_tool_parameters(&wire_with(&schema_request(schema), Some(astra())));
    assert_eq!(
        parameters,
        json!({
            "type": "object",
            "properties": {"a": {"type": "array", "items": {"type": "array"}}},
            "additionalProperties": {"type": "array"},
            "anyOf": [{"type": "array"}],
            "dependentSchemas": {"a": {}},
            "patternProperties": {"^x": {}},
            "$defs": {"d": {}},
            "dependentRequired": {"a": ["b"]}
        })
    );
}

// Given: `wait`-style exclusivity (oneOf branches each carrying `not`), an anyOf with `not`
// nested inside a property of a branch, and an allOf with `not`
// When: serialized for a Lite Codex model
// Then: no `not` is sent; a oneOf / anyOf whose branch lost one is dropped whole (a branch
// without its `not` would match more than before and break the selection), while allOf is kept
#[test]
fn lite_schemas_drop_not_and_the_selections_it_would_distort() {
    let schema = json!({
        "type": "object",
        "properties": {"run_id": {"type": "string"}, "run_ids": {"type": "array"}},
        "oneOf": [
            {"required": ["run_id"], "not": {"required": ["run_ids"]}},
            {"required": ["run_ids"], "not": {"required": ["run_id"]}},
            {"not": {"anyOf": [{"required": ["run_id"]}, {"required": ["run_ids"]}]}}
        ],
        "anyOf": [{"properties": {"x": {"not": {"const": 1}}}}, {"required": ["y"]}],
        "allOf": [{"not": {"required": ["z"]}, "required": ["w"]}, {"required": ["v"]}]
    });
    let request = schema_request(schema.clone());
    let parameters = first_tool_parameters(&wire_with(&request, Some(astra())));
    assert_eq!(
        parameters,
        json!({
            "type": "object",
            "properties": {"run_id": {"type": "string"}, "run_ids": {"type": "array"}},
            "allOf": [{"required": ["w"]}, {"required": ["v"]}]
        })
    );
    assert_eq!(
        request.tools[0].input_schema, schema,
        "canonical schema is untouched"
    );
}

// Given: a `not` that only exists inside a `then` (stripped with its `if`) and a property
// named `not`, both next to a oneOf
// When: serialized for a Lite Codex model
// Then: the oneOf and the property named `not` survive, because no `not` keyword was removed
// from the oneOf's branches
#[test]
fn lite_schemas_keep_a_property_named_not_and_selections_it_does_not_touch() {
    let schema = json!({
        "type": "object",
        "properties": {"not": {"type": "string"}},
        "oneOf": [{"required": ["not"]}, {"required": ["other"]}],
        "allOf": [{"if": {"required": ["a"]}, "then": {"not": {"required": ["b"]}}}]
    });
    let parameters = first_tool_parameters(&wire_with(&schema_request(schema), Some(astra())));
    assert_eq!(
        parameters,
        json!({
            "type": "object",
            "properties": {"not": {"type": "string"}},
            "oneOf": [{"required": ["not"]}, {"required": ["other"]}],
            "allOf": [{}]
        })
    );
}
