use serde_json::{Value, json};

use super::{ResponsesDialect, ResponsesRequestOptions, to_wire_request};
use crate::message::ChatRequest;

#[test]
fn apply_patch_definition_is_freeform_only_for_the_exact_contract() {
    use apply_patch::tool_contract::{FREEFORM_DESCRIPTION, LARK_GRAMMAR, NAME, json_schema};
    let mut request = minimal_request();
    request.tools = serde_json::from_value(json!([{
        "name": NAME, "description": "canonical description is not the freeform contract",
        "input_schema": json_schema()
    }]))
    .unwrap();
    for dialect in [ResponsesDialect::Codex, ResponsesDialect::OpenAi] {
        assert_eq!(
            wire(&request, dialect, None)["tools"],
            json!([{
                "type": "custom", "name": NAME, "description": FREEFORM_DESCRIPTION,
                "format": {"type": "grammar", "syntax": "lark", "definition": LARK_GRAMMAR}
            }])
        );
        let mut collision = request.clone();
        collision.tools[0].input_schema = json!({"type": "object"});
        assert_eq!(
            wire(&collision, dialect, None)["tools"][0]["type"],
            "function"
        );
        let mut other_name = request.clone();
        other_name.tools[0].name = "another_tool".into();
        assert_eq!(
            wire(&other_name, dialect, None)["tools"][0]["type"],
            "function"
        );
    }
}

#[test]
fn apply_patch_history_pairs_custom_calls_even_without_current_tools() {
    for tools_present in [false, true] {
        let mut request: ChatRequest = serde_json::from_value(json!({
            "model": "m", "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "valid", "name": "apply_patch", "input": {"input": "patch\n日本語"}},
                    {"type": "tool_use", "id": "invalid", "name": "apply_patch", "input": null}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_call_id": "valid", "is_error": false, "content": [
                        {"type": "text", "text": "ok"},
                        {"type": "image", "media_type": "image/png", "data": "AAAA"}
                    ]},
                    {"type": "tool_result", "tool_call_id": "invalid", "is_error": true, "content": [{"type": "text", "text": "bad"}]}
                ]}
            ]
        })).unwrap();
        if tools_present {
            request.tools = serde_json::from_value(json!([{"name": "apply_patch", "description": "patch", "input_schema": apply_patch::tool_contract::json_schema()}])).unwrap();
        }
        for dialect in [ResponsesDialect::Codex, ResponsesDialect::OpenAi] {
            assert_eq!(
                wire(&request, dialect, None)["input"],
                json!([
                    {"type": "custom_tool_call", "call_id": "valid", "name": "apply_patch", "input": "patch\n日本語"},
                    {"type": "function_call", "call_id": "invalid", "name": "apply_patch", "arguments": "null"},
                    {"type": "custom_tool_call_output", "call_id": "valid", "output": "ok\n[image image/png attached in the next user message]"},
                    {"type": "function_call_output", "call_id": "invalid", "output": "bad"},
                    {"type": "message", "role": "user", "content": [
                        {"type": "input_text", "text": "Images returned by tool call valid:"},
                        {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
                    ]}
                ])
            );
        }
    }
}

#[test]
fn custom_history_uses_only_the_name_and_string_input_even_when_result_precedes_call() {
    for (name, input, call_type, output_type) in [
        (
            "apply_patch",
            json!({"input": ""}),
            "custom_tool_call",
            "custom_tool_call_output",
        ),
        (
            "apply_patch",
            json!({"input": 42}),
            "function_call",
            "function_call_output",
        ),
        (
            "apply_patch",
            json!({}),
            "function_call",
            "function_call_output",
        ),
        (
            "other",
            json!({"input": "patch"}),
            "function_call",
            "function_call_output",
        ),
    ] {
        let request: ChatRequest = serde_json::from_value(json!({
            "model": "m", "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_call_id": "call", "is_error": false, "content": []}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call", "name": name, "input": input}]}
            ]
        })).unwrap();
        for dialect in [ResponsesDialect::Codex, ResponsesDialect::OpenAi] {
            let value = wire(&request, dialect, None);
            assert_eq!(value["input"][0]["type"], output_type);
            assert_eq!(value["input"][0]["output"], "");
            assert_eq!(value["input"][1]["type"], call_type);
        }
    }
}

/// system 連結・画像・tool 往復・tool result 画像・compaction・tool 並べ替え・
/// 生成設定をすべて含む canonical request。
fn rich_request() -> ChatRequest {
    serde_json::from_value(json!({
        "model": "gpt-5-codex",
        "temperature": 0.3,
        "max_tokens": 2048,
        "service_tier": "priority",
        "output_schema": {"name": "verdict", "schema": {
            "type": "object", "properties": {"ok": {"type": "boolean"}},
            "required": ["ok"], "additionalProperties": false
        }},
        "messages": [
            {"role": "system", "content": [
                {"type": "text", "text": "Follow the rules."},
                {"type": "text", "text": "Be brief."}
            ]},
            {"role": "user", "content": [
                {"type": "text", "text": "Look at this"},
                {"type": "image", "media_type": "image/png", "data": "iVBORw0K"}
            ]},
            {"role": "assistant", "content": [
                {"type": "reasoning", "text": "thinking"},
                {"type": "text", "text": "Reading."},
                {"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a.png"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_call_id": "call_1", "is_error": false, "content": [
                    {"type": "text", "text": "Image a.png"},
                    {"type": "image", "media_type": "image/png", "data": "AAAA", "source": "/w/a.png"}
                ]}
            ]},
            {"role": "assistant", "content": [
                {"type": "compaction", "encrypted_content": "opaque=="},
                {"type": "text", "text": "Done."}
            ]}
        ],
        "tools": [
            {"name": "zeta", "description": "Z tool", "input_schema": {"type": "object"}},
            {"name": "alpha", "description": "A tool", "input_schema": {
                "type": "object", "properties": {"q": {"type": "string"}}
            }}
        ]
    }))
    .unwrap()
}

fn minimal_request() -> ChatRequest {
    serde_json::from_value(json!({
        "model": "m",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    }))
    .unwrap()
}

fn options(dialect: ResponsesDialect, prompt_cache_key: Option<&str>) -> ResponsesRequestOptions {
    ResponsesRequestOptions {
        dialect,
        prompt_cache_key: prompt_cache_key.map(str::to_owned),
    }
}

fn wire(request: &ChatRequest, dialect: ResponsesDialect, key: Option<&str>) -> Value {
    serde_json::to_value(to_wire_request(request, &options(dialect, key))).unwrap()
}

// Given: the canonical requests used for the Codex CLI 0.166.0 wire fixture
// When: serialized with the Codex dialect
// Then: the bytes (field order included) equal the checked-in fixture; this is the cache
// prefix contract for the Codex wire
#[test]
fn codex_dialect_wire_bytes_match_the_0166_fixture() {
    let golden = include_str!("../../../tests/fixtures/codex/wire_request_0166.json");
    let codex = options(ResponsesDialect::Codex, None);

    let rich = serde_json::to_string(&to_wire_request(&rich_request(), &codex)).unwrap();
    let minimal = serde_json::to_string(&to_wire_request(&minimal_request(), &codex)).unwrap();

    assert_eq!(rich, golden.trim_end());
    // 空の system からは developer message を作らず、`instructions` も送らない。
    assert_eq!(
        minimal,
        r#"{"model":"m","stream":true,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"tools":[],"tool_choice":"auto","parallel_tool_calls":true,"reasoning":{"effort":"medium","summary":"auto"},"store":false,"include":["reasoning.encrypted_content"]}"#
    );
}

// Given: the pre-0.166 Codex golden (top-level `instructions`) and the 0.166.0 fixture
// When: they are compared as JSON values
// Then: the only differences are `instructions` leaving the top level and a developer
// message (id/role/text) being prepended to `input`; everything else is identical
#[test]
fn codex_0166_differs_from_the_previous_golden_only_by_the_developer_move() {
    let old: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/codex/wire_request_pre_0166.json"
    ))
    .unwrap();
    let new: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/codex/wire_request_0166.json"
    ))
    .unwrap();
    let mut old = old.as_object().unwrap().clone();
    let mut new = new.as_object().unwrap().clone();

    let instructions = old.remove("instructions").unwrap();
    assert!(!new.contains_key("instructions"));
    let old_input = old.remove("input").unwrap();
    let new_input = new.remove("input").unwrap();
    let new_input = new_input.as_array().unwrap();
    let developer = &new_input[0];
    assert_eq!(developer["type"], "message");
    assert_eq!(developer["role"], "developer");
    assert_eq!(developer["content"].as_array().unwrap().len(), 1);
    assert_eq!(developer["content"][0]["type"], "input_text");
    assert_eq!(developer["content"][0]["text"], instructions);
    assert_eq!(
        developer["id"],
        format!("msg_{}", developer_uuid(instructions.as_str().unwrap()))
    );
    assert_eq!(Value::Array(new_input[1..].to_vec()), old_input);
    assert_eq!(old, new, "other keys must be identical");
}

// The expected ID, built independently of `developer_message`.
fn developer_uuid(text: &str) -> uuid::Uuid {
    let namespace = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        b"evorch.codex.developer_instructions",
    );
    uuid::Uuid::new_v5(&namespace, text.as_bytes())
}

// Given: the Codex dialect / When: keys are listed in serialization order
// Then: they follow Codex CLI 0.166.0's `ResponsesApiRequest` field order
#[test]
fn codex_dialect_serializes_fields_in_codex_order() {
    let mut request = rich_request();
    request.reasoning_effort = Some("high".into());
    let json = serde_json::to_string(&to_wire_request(
        &request,
        &options(ResponsesDialect::Codex, Some("run-1")),
    ))
    .unwrap();
    let keys = [
        "model",
        "stream",
        "service_tier",
        "input",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "reasoning",
        "store",
        "include",
        "prompt_cache_key",
        "text",
    ];
    assert_eq!(root_keys(&json), keys, "{json}");
}

/// Top-level object keys in serialization order (`Value` would sort them).
fn root_keys(json: &str) -> Vec<String> {
    use serde::Deserializer as _;
    use serde::de::{IgnoredAny, MapAccess, Visitor};

    struct Keys;
    impl<'de> Visitor<'de> for Keys {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut keys = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                map.next_value::<IgnoredAny>()?;
                keys.push(key);
            }
            Ok(keys)
        }
    }
    serde_json::Deserializer::from_str(json)
        .deserialize_map(Keys)
        .unwrap()
}

// Given: system messages / When: serialized with the Codex dialect
// Then: they are joined by a blank line into one developer message at input[0], with no
// top-level `instructions`; a message without any system text gets no developer message
#[test]
fn codex_places_joined_system_text_in_a_single_developer_message() {
    let mut request = minimal_request();
    request.messages.insert(
        0,
        serde_json::from_value(json!({"role": "system", "content": [
            {"type": "text", "text": "Follow the rules."},
            {"type": "text", "text": "Be brief."}
        ]}))
        .unwrap(),
    );
    request.messages.insert(
        1,
        serde_json::from_value(json!({"role": "system", "content": [
            {"type": "text", "text": "Third."}
        ]}))
        .unwrap(),
    );

    let value = wire(&request, ResponsesDialect::Codex, None);

    let text = "Follow the rules.\n\nBe brief.\n\nThird.";
    assert!(value.get("instructions").is_none());
    assert_eq!(
        value["input"][0],
        json!({
            "type": "message",
            "id": format!("msg_{}", developer_uuid(text)),
            "role": "developer",
            "content": [{"type": "input_text", "text": text}]
        })
    );
    assert_eq!(value["input"].as_array().unwrap().len(), 2);
    assert_eq!(value["input"][1]["role"], "user");

    let mut empty = minimal_request();
    empty.messages.insert(
        0,
        serde_json::from_value(json!({"role": "system", "content": []})).unwrap(),
    );
    let value = wire(&empty, ResponsesDialect::Codex, None);
    assert!(value.get("instructions").is_none());
    assert_eq!(value["input"].as_array().unwrap().len(), 1);
    assert_eq!(value["input"][0]["role"], "user");
}

// Given: the same instructions in different requests / When: serialized
// Then: the developer ID is a pure function of the text (stable across requests, history
// growth and compaction), and different text gives a different ID
#[test]
fn developer_message_id_depends_only_on_the_instruction_text() {
    let first = wire(&rich_request(), ResponsesDialect::Codex, None);
    let mut grown = rich_request();
    grown.messages.push(minimal_request().messages.remove(0));
    let second = wire(&grown, ResponsesDialect::Codex, None);
    assert_eq!(first["input"][0], second["input"][0]);

    let mut changed = rich_request();
    changed.messages[0].content = serde_json::from_value(json!([
        {"type": "text", "text": "Different rules."}
    ]))
    .unwrap();
    let third = wire(&changed, ResponsesDialect::Codex, None);
    assert_ne!(first["input"][0]["id"], third["input"][0]["id"]);
    assert_eq!(third["input"][0]["content"][0]["text"], "Different rules.");
}

// Given: system messages / When: serialized with the OpenAI dialect
// Then: `instructions` stays at the top level and no developer message is added
#[test]
fn openai_dialect_keeps_top_level_instructions_without_a_developer_message() {
    let value = wire(&rich_request(), ResponsesDialect::OpenAi, None);

    assert_eq!(value["instructions"], "Follow the rules.\n\nBe brief.");
    assert!(
        value["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["role"] != "developer" && item.get("id").is_none())
    );
}

// Given: the OpenAI-dialect wire captured before the Codex field-order change
// When: the same canonical requests are serialized now
// Then: the bytes differ only in the order of the root fields: parsed JSON is equal, and the
// large `input` and `tools` values are byte-identical substrings
#[test]
fn openai_dialect_differs_from_the_previous_wire_only_in_root_field_order() {
    let mut rich = rich_request();
    rich.reasoning_effort = Some("high".into());
    let cases = [
        (
            include_str!("../../../tests/fixtures/openai/responses_request_pre_0166_rich.json"),
            to_wire_request(&rich, &options(ResponsesDialect::OpenAi, Some("run-1"))),
        ),
        (
            include_str!("../../../tests/fixtures/openai/responses_request_pre_0166_minimal.json"),
            to_wire_request(&minimal_request(), &options(ResponsesDialect::OpenAi, None)),
        ),
    ];
    for (old, new) in cases {
        let old = old.trim_end();
        let new = serde_json::to_string(&new).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(old).unwrap(),
            serde_json::from_str::<Value>(&new).unwrap()
        );
        // The old order was `... "input", "tools", "store" ...` (tools absent when empty).
        let input_end = old
            .find(",\"tools\":")
            .or_else(|| old.find(",\"store\":"))
            .unwrap();
        let input = &old[old.find("\"input\":").unwrap()..input_end];
        assert!(new.contains(input), "input bytes changed: {new}");
        if let Some(start) = old.find("\"tools\":") {
            let tools = &old[start..old.find(",\"store\":").unwrap()];
            assert!(new.contains(tools), "tools bytes changed: {new}");
        }
        assert_eq!(new.len(), old.len(), "no byte may be added or removed");
    }
}

// Given: a request with every generation setting and a run cache key
// When: serialized with the OpenAI dialect
// Then: the official Responses settings carry them and tools are explicitly non-strict
#[test]
fn openai_dialect_maps_generation_settings() {
    let mut request = rich_request();
    request.reasoning_effort = Some("high".into());

    let value = wire(&request, ResponsesDialect::OpenAi, Some("run-1"));

    assert_eq!(value["instructions"], "Follow the rules.\n\nBe brief.");
    assert_eq!(
        value["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(value["max_output_tokens"], 2048);
    assert_eq!(value["temperature"], 0.3);
    assert_eq!(value["service_tier"], "priority");
    assert_eq!(value["prompt_cache_key"], "run-1");
    assert_eq!(value["tool_choice"], "auto");
    assert_eq!(value["parallel_tool_calls"], true);
    assert_eq!(value["store"], false);
    assert_eq!(value["stream"], true);
    assert_eq!(value["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(
        value["text"]["format"],
        json!({
            "type": "json_schema", "name": "verdict", "strict": true,
            "schema": request.output_schema.as_ref().unwrap().schema,
        })
    );
    assert_eq!(
        value["tools"],
        json!([
            {"type": "function", "name": "alpha", "description": "A tool", "strict": false,
                "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}},
            {"type": "function", "name": "zeta", "description": "Z tool", "strict": false,
                "parameters": {"type": "object"}}
        ])
    );
}

// Given: no system prompt, tools, reasoning effort, sampling settings, or run
// When: serialized with the OpenAI dialect
// Then: unset settings, including the empty tools key, are omitted instead of sending Codex
// defaults, while Codex keeps its byte-compatible empty tools array
#[test]
fn openai_dialect_omits_unset_optional_settings() {
    let value = wire(&minimal_request(), ResponsesDialect::OpenAi, None);
    let object = value.as_object().unwrap();

    for absent in [
        "instructions",
        "tools",
        "reasoning",
        "max_output_tokens",
        "temperature",
        "tool_choice",
        "parallel_tool_calls",
        "service_tier",
        "prompt_cache_key",
        "text",
    ] {
        assert!(!object.contains_key(absent), "{absent}: {value}");
    }
    assert_eq!(
        wire(&minimal_request(), ResponsesDialect::Codex, None)["tools"],
        json!([])
    );
    assert_eq!(value["store"], false);
    assert_eq!(value["stream"], true);
    assert_eq!(value["include"], json!(["reasoning.encrypted_content"]));
}

// Given: reasoning effort set or unset / When: both dialects are serialized
// Then: Codex always requests a summary (defaulting to medium), OpenAI only when configured
#[test]
fn reasoning_is_requested_per_dialect() {
    for (effort, codex, openai) in [
        (
            Some("low"),
            json!({"effort": "low", "summary": "auto"}),
            json!({"effort": "low", "summary": "auto"}),
        ),
        (
            None,
            json!({"effort": "medium", "summary": "auto"}),
            Value::Null,
        ),
    ] {
        let mut request = minimal_request();
        request.reasoning_effort = effort.map(str::to_owned);
        let codex_wire = to_wire_request(&request, &options(ResponsesDialect::Codex, None));
        let openai_wire = to_wire_request(&request, &options(ResponsesDialect::OpenAi, None));

        assert_eq!(
            serde_json::to_value(&codex_wire).unwrap()["reasoning"],
            codex
        );
        assert_eq!(
            serde_json::to_value(&openai_wire).unwrap()["reasoning"],
            openai
        );
        assert_eq!(
            codex_wire.reasoning_effort(),
            Some(effort.unwrap_or("medium"))
        );
        assert_eq!(openai_wire.reasoning_effort(), effort);
    }
}

// Given: a history containing a Codex compaction blob
// When: serialized with the OpenAI dialect
// Then: the opaque blob is not replayed, and every other input item matches Codex
#[test]
fn openai_dialect_skips_compaction_but_shares_the_input_shape() {
    let request = rich_request();

    let codex = wire(&request, ResponsesDialect::Codex, None);
    let openai = wire(&request, ResponsesDialect::OpenAi, None);

    // Codex は指示文を developer message として input 先頭に置く。OpenAI は top-level。
    let codex_input: Vec<_> = codex["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] != "compaction" && item["role"] != "developer")
        .cloned()
        .collect();
    assert_eq!(
        codex_input.len() + 2,
        codex["input"].as_array().unwrap().len()
    );
    assert_eq!(openai["input"], Value::Array(codex_input));
    assert!(!openai.to_string().contains("opaque=="));
    assert!(!openai.to_string().contains("/w/a.png"));
}

#[test]
fn b3_include_is_stable_for_both_dialects() {
    for dialect in [ResponsesDialect::Codex, ResponsesDialect::OpenAi] {
        for effort in [None, Some("high".to_owned())] {
            let mut request = minimal_request();
            request.reasoning_effort = effort;
            let value = wire(&request, dialect, None);
            assert_eq!(value["include"], json!(["reasoning.encrypted_content"]));
            assert_eq!(value["store"], false);
        }
    }
}

fn replay_scope(dialect: ResponsesDialect) -> crate::ResolvedResponsesScope {
    crate::ResolvedResponsesScope::resolve(
        &minimal_request(),
        dialect,
        Some("work"),
        "https://example.test/v1/responses",
        crate::ResponsesAccount::Unavailable,
    )
}

#[test]
fn b3_native_item_round_trip_and_replay_keeps_boundaries_and_fields() {
    use crate::{ContentBlock, Message, ResponsesReplayContext, ResponsesSpeed, Role};
    for dialect in [ResponsesDialect::OpenAi, ResponsesDialect::Codex] {
        let scope = replay_scope(dialect);
        let item = json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque\n日本語", "content":[{"type":"reasoning_text","text":"visible"}],"status":"completed","future":{"a":true}});
        let opaque = ContentBlock::ResponsesReasoning {
            item: item.clone(),
            origin: scope.clone(),
        };
        assert_eq!(opaque.text(), None);
        assert_eq!(opaque.reasoning_text(), None);
        assert!(!opaque.is_reasoning());
        assert_eq!(
            serde_json::from_value::<ContentBlock>(serde_json::to_value(&opaque).unwrap()).unwrap(),
            opaque
        );
        let mut request = minimal_request();
        request.responses_replay = Some(ResponsesReplayContext {
            route_model: scope.route_model.clone(),
            speed: ResponsesSpeed::Standard,
            expected_scope: Some(scope.clone()),
        });
        request.messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "before".into(),
                },
                opaque.clone(),
                ContentBlock::Text {
                    text: "after".into(),
                },
            ],
        }];
        let value = serde_json::to_value(
            super::to_wire_request_with_scope(&request, &options(dialect, None), &scope).unwrap(),
        )
        .unwrap();
        assert_eq!(value["input"][1], item);
        assert_eq!(value["input"].as_array().unwrap().len(), 3);
        assert!(!value.to_string().contains("origin"));
        assert!(
            wire(&request, dialect, None)["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["type"] != "reasoning")
        );
        for role in [Role::User, Role::System] {
            request.messages[0].role = role;
            let value = serde_json::to_value(
                super::to_wire_request_with_scope(&request, &options(dialect, None), &scope)
                    .unwrap(),
            )
            .unwrap();
            assert!(!value.to_string().contains("opaque"));
        }
        request.messages[0].role = Role::Assistant;
        request
            .responses_replay
            .as_mut()
            .unwrap()
            .expected_scope
            .as_mut()
            .unwrap()
            .profile = "other".into();
        assert!(matches!(
            super::to_wire_request_with_scope(&request, &options(dialect, None), &scope),
            Err(crate::ProviderError::ResponsesScopeChanged { .. })
        ));
    }
}

#[test]
fn b3_review_m1_canonical_debug_redacts_reasoning_without_changing_serde() {
    use crate::{ChatResponse, ContentBlock, Message, Role, StreamEvent};
    let sentinel = "m1-canonical-ciphertext-sentinel-🔒";
    let item = json!({"type":"reasoning","id":"rs_debug","summary":[],"encrypted_content":sentinel,"future":{"field":"preserved"}});
    let origin = replay_scope(ResponsesDialect::OpenAi);
    let block = ContentBlock::ResponsesReasoning {
        item: item.clone(),
        origin: origin.clone(),
    };
    let encoded = serde_json::to_value(&block).unwrap();
    assert_eq!(
        encoded,
        json!({"type":"responses_reasoning","item":item,"origin":origin})
    );
    for debug in [format!("{block:?}"), format!("{block:#?}")] {
        assert!(
            !debug.contains(sentinel),
            "canonical Debug exposed ciphertext"
        );
        assert!(debug.contains("redacted"));
    }
    let response = ChatResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![block],
        },
        usage: Default::default(),
        finish_reason: crate::FinishReason::Stop,
    };
    assert!(!format!("{:?}", StreamEvent::Completed { response }).contains(sentinel));
}

#[test]
fn b3_review_m1_wire_debug_redacts_reasoning_without_changing_serialization() {
    use crate::{ContentBlock, Message, ResponsesReplayContext, Role};
    let sentinel = "m1-wire-ciphertext-sentinel-🔒";
    for dialect in [ResponsesDialect::OpenAi, ResponsesDialect::Codex] {
        let scope = replay_scope(dialect);
        let item = json!({"type":"reasoning","id":"rs_debug","summary":[],"encrypted_content":sentinel,"future":true});
        let mut request = minimal_request();
        request.responses_replay = Some(ResponsesReplayContext {
            route_model: scope.route_model.clone(),
            speed: scope.speed,
            expected_scope: Some(scope.clone()),
        });
        request.messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ResponsesReasoning {
                item: item.clone(),
                origin: scope.clone(),
            }],
        }];
        let wire =
            super::to_wire_request_with_scope(&request, &options(dialect, None), &scope).unwrap();
        assert_eq!(serde_json::to_value(&wire).unwrap()["input"][0], item);
        let serialized: Value =
            serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
        assert_eq!(serialized["input"][0], item);
        assert!(
            !format!("{wire:?}").contains(sentinel),
            "wire Debug exposed ciphertext"
        );
        assert!(
            !format!("{:#?}", wire.input).contains(sentinel),
            "input Debug exposed ciphertext"
        );
    }
}

#[test]
fn b3_review_m1_existing_text_debug_shape_is_unchanged() {
    assert_eq!(
        format!(
            "{:?}",
            crate::ContentBlock::Text {
                text: "visible".into()
            }
        ),
        r#"Text { text: "visible" }"#
    );
    assert_eq!(
        format!(
            "{:?}",
            crate::ContentBlock::Reasoning {
                text: "plain thinking".into()
            }
        ),
        r#"Reasoning { text: "plain thinking" }"#
    );
}
