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

// Given: the canonical requests captured before the Responses wire was shared
// When: serialized with the Codex dialect
// Then: the bytes are identical to the pre-migration Codex wire (cache prefix contract)
#[test]
fn codex_dialect_wire_bytes_match_the_pre_migration_shape() {
    let golden = include_str!("../../../tests/fixtures/codex/wire_request_golden.json");
    let codex = options(ResponsesDialect::Codex, None);

    let rich = serde_json::to_string(&to_wire_request(&rich_request(), &codex)).unwrap();
    let minimal = serde_json::to_string(&to_wire_request(&minimal_request(), &codex)).unwrap();

    assert_eq!(rich, golden.trim_end());
    assert_eq!(
        minimal,
        r#"{"model":"m","instructions":"","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"tools":[],"store":false,"stream":true,"tool_choice":"auto","parallel_tool_calls":true,"reasoning":{"effort":"medium","summary":"auto"},"include":["reasoning.encrypted_content"]}"#
    );
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

    let codex_input: Vec<_> = codex["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] != "compaction")
        .cloned()
        .collect();
    assert_eq!(
        codex_input.len() + 1,
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

// Locate root include's value with serde's byte offset; compare the surrounding
// bytes, not a normalized object that would hide spacing or key/tool reordering.
fn include_span(bytes: &str) -> Option<std::ops::Range<usize>> {
    serde_json::from_str::<Value>(bytes).ok()?.as_object()?;
    let mut depth = 0usize;
    let mut position = 0;
    let mut found = None;
    while position < bytes.len() {
        match bytes.as_bytes()[position] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.checked_sub(1)?,
            b'"' => {
                let mut stream =
                    serde_json::Deserializer::from_str(&bytes[position..]).into_iter::<String>();
                let key = stream.next()?.ok()?;
                position += stream.byte_offset();
                if depth == 1 && key == "include" && bytes[position..].trim_start().starts_with(':')
                {
                    let colon = position + bytes[position..].find(':')?;
                    let start = colon + 1;
                    let mut stream =
                        serde_json::Deserializer::from_str(&bytes[start..]).into_iter::<Value>();
                    let value = stream.next()?.ok()?;
                    if found.is_some() || !value.is_array() {
                        return None;
                    }
                    found = Some(start..start + stream.byte_offset());
                }
                continue;
            }
            _ => {}
        }
        position += 1;
    }
    found
}
fn only_include_changed(old: &str, new: &str) -> bool {
    let Some(a) = include_span(old) else {
        return false;
    };
    let Some(b) = include_span(new) else {
        return false;
    };
    &old[a.clone()] == "[]"
        && &new[b.clone()] == "[\"reasoning.encrypted_content\"]"
        && old[..a.start] == new[..b.start]
        && old[a.end..] == new[b.end..]
}

#[test]
fn b3_golden_allows_only_include_value_bytes_and_rejects_other_changes() {
    use sha2::{Digest, Sha256};
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(include_bytes!(
                "../../../tests/fixtures/codex/wire_request_pre_b3.json"
            ))
        ),
        "71901338bd55459a5385ff4d32b398fd79d269afed7c276b0e392c1a87912ae3"
    );
    let old = include_str!("../../../tests/fixtures/codex/wire_request_pre_b3.json").trim_end();
    let new = serde_json::to_string(&to_wire_request(
        &rich_request(),
        &options(ResponsesDialect::Codex, None),
    ))
    .unwrap();
    assert!(only_include_changed(old, &new));
    for changed in [
        new.replacen("\"model\":", "\"model\": ", 1),
        new.replacen("gpt-5-codex", "other-model", 1),
        new.replacen("alpha", "changed_tool", 1),
        new.replacen("\"store\":false", "\"store\":true", 1),
        new.replace("reasoning.encrypted_content", "wrong.include"),
    ] {
        assert_ne!(new, changed);
        assert!(!only_include_changed(old, &changed));
    }
    let wire = to_wire_request(&rich_request(), &options(ResponsesDialect::Codex, None));
    let first = serde_json::to_string(&wire.tools.as_ref().unwrap()[0]).unwrap();
    let second = serde_json::to_string(&wire.tools.as_ref().unwrap()[1]).unwrap();
    let reordered = new.replace(&format!("{first},{second}"), &format!("{second},{first}"));
    assert_ne!(new, reordered);
    assert!(!only_include_changed(old, &reordered));
    assert_eq!(
        include_span(r#"{"nested":{"include":[]},"include":[],"tail":{"include":[1]}}"#)
            .map(|r| &r#"{"nested":{"include":[]},"include":[],"tail":{"include":[1]}}"#[r]),
        Some("[]")
    );
    let mut changed = rich_request();
    changed.tools[0].description.push_str(" changed");
    assert!(!only_include_changed(
        old,
        &serde_json::to_string(&to_wire_request(
            &changed,
            &options(ResponsesDialect::Codex, None)
        ))
        .unwrap()
    ));
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
