use serde_json::{Value, json};

use super::{ResponsesDialect, ResponsesRequestOptions, to_wire_request};
use crate::message::ChatRequest;

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
        r#"{"model":"m","instructions":"","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"tools":[],"store":false,"stream":true,"tool_choice":"auto","parallel_tool_calls":true,"reasoning":{"effort":"medium","summary":"auto"},"include":[]}"#
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
    assert_eq!(value["include"], json!([]));
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
    assert_eq!(value["include"], json!([]));
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
