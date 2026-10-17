use super::*;
use crate::cache_contract::{CacheProtocol, assert_append_only};

// Independent Bun 1.4.2 xxHash64 golden, using the native request exclusions.
const GOLDEN: &str = r#"{"model":"claude-test","messages":[{"role":"user","content":[{"type":"text","text":"Reply LOCAL_CAPTURE_OK.","cache_control":{"type":"ephemeral","ttl":"1h"}}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=1fed0;"},{"type":"text","text":"Keep instructions."}],"tools":[{"name":"_read","input_schema":{"type":"object"}}],"max_tokens":1024,"stream":true}"#;

// Bun's hash of a separately constructed normalized body is 2b794. Nested
// routing fields are excluded too; similarly named schema properties remain.
const NESTED_GOLDEN: &str = r#"{"model":"claude-test","max_tokens":1024,"messages":[{"role":"user","content":"Reply LOCAL_CAPTURE_OK."},{"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"_read","input":{"model":"nested-model","max_tokens":7,"fallbacks":[["primary",{"nested":["]","\""]}]],"fallback_credit_token":"nested-token","keep":"literal cch=00000;"}}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=2b794;"},{"type":"text","text":"Keep instructions."}],"tools":[{"name":"_read","input_schema":{"type":"object","properties":{"model":{"type":"string"},"max_tokens":{"type":"integer"},"fallbacks":{"type":"array"},"fallback_credit_token":{"type":"string"}},"examples":[{"model":"example","max_tokens":9},{"max_tokens":11},{"fallbacks":[]},{"fallback_credit_token":"secret"}]}}],"fallbacks":["root-fallback"],"fallback_credit_token":"root-token","stream":true}"#;

fn signed(request: Value) -> Vec<u8> {
    signed_with_context(request, "")
}

fn signed_with_context(mut request: Value, context: &str) -> Vec<u8> {
    let suffix = version_suffix(first_user_text(&request));
    request["system"][0]["text"] = json!(format!(
        "{BILLING_START}{suffix}{BILLING_MIDDLE}00000;{context}"
    ));
    let raw = serde_json::to_vec(&request).unwrap();
    let checksum = format!(
        "{:05x}",
        xxh64(&checksum_input(&raw).unwrap(), 0x4d65_9218_e32a_3268) & 0xfffff
    );
    request["system"][0]["text"] = json!(format!(
        "{BILLING_START}{suffix}{BILLING_MIDDLE}{checksum};{context}"
    ));
    serde_json::to_vec(&request).unwrap()
}

#[test]
fn native_context_golden_is_verified_before_only_transport_fields_are_removed() {
    // Independently hashed with Bun 1.4.2; the newline belongs to the fixture file.
    let raw = include_str!("context-golden.json").trim_end().as_bytes();
    let model = claude_model_input(raw).unwrap();
    assert_eq!(model, claude_model_input(GOLDEN.as_bytes()).unwrap());
    let mut request: Value = serde_json::from_slice(raw).unwrap();
    request["tools"][0]["input_schema"]["diagnostics"] =
        json!({"previous_message_id":"nested-user-value"});
    request["future_root_setting"] = json!("do not ignore");
    let preserved = claude_model_input(&signed_with_context(
        request.clone(),
        " cc_prev_req=req_first;",
    ))
    .unwrap();
    assert_eq!(preserved["tools"], request["tools"]);
    assert_eq!(preserved["future_root_setting"], "do not ignore");
    assert!(assert_append_only(CacheProtocol::Anthropic, &model, &preserved).is_err());
}

#[test]
fn invalid_dynamic_controls_are_rejected_even_with_a_valid_checksum() {
    let request: Value = serde_json::from_str(GOLDEN).unwrap();
    for fields in [
        " cc_prev_req=msg_wrong;",
        " cc_prev_req=req_first; cc_prev_req=req_second;",
        " cc_prompt_id=not-a-uuid;",
        " cc_turn_origin=Human;",
        " cc_turn_index=0;",
        " cc_prompt_index=1;",
        " cc_prompt_index=01; cc_turn_index=1;",
        " cc_prompt_index=1; cc_turn_index=10000001;",
        " cc_is_subagent=false;",
        " cc_unrecognized=value;",
        " cc_turn_origin=human; cc_prev_req=req_wrong_order;",
    ] {
        assert!(
            claude_model_input(&signed_with_context(request.clone(), fields)).is_err(),
            "{fields}"
        );
    }
    for diagnostics in [
        json!({}),
        json!({"previous_message_id":"req_wrong"}),
        json!({"previous_message_id":null,"unknown":true}),
        json!({"previous_message_id":42}),
    ] {
        let mut invalid = request.clone();
        invalid["diagnostics"] = diagnostics;
        assert!(claude_model_input(&signed(invalid)).is_err());
    }
}

#[test]
fn native_checksum_golden_accepts_middle_and_final_max_tokens() {
    let model = claude_model_input(GOLDEN.as_bytes()).unwrap();
    assert_eq!(
        model["system"],
        json!([{"type": "text", "text": "Keep instructions."}])
    );
    assert_eq!(
        model["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    let last = GOLDEN.replace(
        ",\"max_tokens\":1024,\"stream\":true",
        ",\"stream\":true,\"max_tokens\":1024",
    );
    assert_eq!(claude_model_input(last.as_bytes()).unwrap(), model);
    // These fields are excluded from the checksum but stay in model settings.
    let changed = GOLDEN
        .replace("claude-test", "claude-other")
        .replace("1024", "4096");
    let changed = claude_model_input(changed.as_bytes()).unwrap();
    assert_eq!(changed["model"], "claude-other");
    assert_eq!(changed["max_tokens"], 4096);
    assert!(assert_append_only(CacheProtocol::Anthropic, &model, &changed).is_err());
}

#[test]
fn tampering_or_unexpected_control_format_is_rejected() {
    for raw in [
        GOLDEN.replace("cch=1fed0", "cch=00000"),
        GOLDEN.replace("Keep instructions.", "Changed instructions."),
        GOLDEN.replace("2.1.295.bef", "2.1.295.000"),
        GOLDEN.replace("cch=1fed0", "cch=1FED0"),
        GOLDEN.replace("cc_entrypoint=cli", "cc_entrypoint=sdk-cli"),
        GOLDEN.replace("2.1.295", "2.1.280"),
    ] {
        assert!(claude_model_input(raw.as_bytes()).is_err(), "{raw}");
    }
    let mut request: Value = serde_json::from_str(GOLDEN).unwrap();
    request["system"][0]["cache_control"] = json!({"type": "ephemeral"});
    assert!(claude_model_input(&signed(request)).is_err());
}

#[test]
fn nested_routing_exclusions_match_bun_without_mutating_any_payload() {
    let raw = NESTED_GOLDEN.as_bytes().to_vec();
    let unchanged = raw.clone();
    let model_input = claude_model_input(&raw).unwrap();
    assert_eq!(raw, unchanged);
    let mut expected: Value = serde_json::from_slice(&raw).unwrap();
    expected["system"].as_array_mut().unwrap().remove(0);
    assert_eq!(model_input, expected);
    let placeholder = NESTED_GOLDEN.replacen("cch=2b794;", "cch=00000;", 1);
    assert_eq!(raw.len(), placeholder.len());
    assert_eq!(
        raw.iter()
            .zip(placeholder.bytes())
            .filter(|(left, right)| **left != *right)
            .count(),
        5
    );
    assert_eq!(
        xxh64(
            &checksum_input(placeholder.as_bytes()).unwrap(),
            0x4d65_9218_e32a_3268
        ) & 0xfffff,
        0x2b794
    );
    for (original, changed) in [
        ("nested-model", "different-model"),
        ("\"max_tokens\":7", "\"max_tokens\":987"),
        ("\"max_tokens\":11", "\"max_tokens\":555"),
        ("nested-token", "different-token"),
        ("root-fallback", "different-fallback"),
    ] {
        let altered = NESTED_GOLDEN.replace(original, changed);
        let altered_model = claude_model_input(altered.as_bytes()).unwrap();
        assert!(
            assert_append_only(CacheProtocol::Anthropic, &model_input, &altered_model).is_err(),
            "{original}"
        );
    }
}

#[test]
fn fingerprint_matches_javascript_utf16_and_missing_text_vectors() {
    assert_eq!(version_suffix("abcd😀xxz0000000000😀"), "9fa");
    assert_eq!(version_suffix(""), "8a8");
    assert_eq!(version_suffix("012345678901234567890"), "4eb");
    let mut request: Value = serde_json::from_str(GOLDEN).unwrap();
    request["messages"][0]["content"] = json!("abcd😀xxz0000000000😀");
    claude_model_input(&signed(request)).unwrap();
}

#[test]
fn only_verified_first_control_block_and_direct_ttl_are_removed() {
    let mut request: Value = serde_json::from_str(GOLDEN).unwrap();
    let sentinel =
        "x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=00000;";
    request["system"][1]["text"] = json!(sentinel);
    request["system"][1]["cache_control"] = json!({"type": "ephemeral", "ttl": "1h"});
    request["tools"][0]["input_schema"]["properties"] = json!({"text": {"const": sentinel}, "cache_control": {"const": {"type": "ephemeral", "ttl": "1h"}}});
    request["messages"].as_array_mut().unwrap().push(json!({"role": "assistant", "content": [{"type": "tool_use", "id": "call-1", "name": "_read", "input": {"sentinel": sentinel, "cache_control": {"type": "ephemeral", "ttl": "1h"}}}]}));
    request["messages"].as_array_mut().unwrap().push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call-1", "content": sentinel, "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}));
    let model = claude_model_input(&signed(request.clone())).unwrap();
    assert_eq!(model["system"][0], request["system"][1]);
    assert_eq!(model["tools"], request["tools"]);
    assert_eq!(model["messages"][1], request["messages"][1]);
    assert_eq!(model["messages"][2]["content"][0]["content"], sentinel);
    assert_eq!(
        model["messages"][2]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

#[test]
fn valid_checksums_do_not_mask_model_prefix_regressions() {
    let previous: Value = serde_json::from_str(GOLDEN).unwrap();
    let baseline = claude_model_input(&signed(previous.clone())).unwrap();
    let mut appended = previous.clone();
    appended["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role": "assistant", "content": [{"type": "text", "text": "old result"}]}));
    let next = claude_model_input(&signed(appended.clone())).unwrap();
    assert_append_only(CacheProtocol::Anthropic, &baseline, &next).unwrap();
    for target in ["instructions", "tool_schema", "history"] {
        let mut changed = appended.clone();
        match target {
            "instructions" => changed["system"][1]["text"] = json!("changed instructions"),
            "tool_schema" => {
                changed["tools"][0]["input_schema"]["additionalProperties"] = json!(false)
            }
            "history" => {
                changed["messages"][0]["content"][0]["text"] = json!("changed previous user text")
            }
            _ => unreachable!(),
        }
        let model = claude_model_input(&signed(changed)).unwrap();
        assert!(
            assert_append_only(CacheProtocol::Anthropic, &baseline, &model).is_err(),
            "{target}"
        );
    }
}
