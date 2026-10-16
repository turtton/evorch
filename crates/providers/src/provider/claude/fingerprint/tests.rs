use super::{billing_header, finalize_billing_checksum, version_suffix};

// Generated independently with Bun 1.4.2 (node:crypto SHA-256 and Bun.hash.xxHash64).
// The ASCII suffix also matches a captured Claude Code 2.1.295 request.
#[test]
fn suffix_matches_javascript_utf16_and_utf8_golden_vectors() {
    for (text, version, expected) in [
        ("Reply LOCAL_CAPTURE_OK.", "2.1.295", "bef"),
        ("Reply LOCAL_CAPTURE_OK.", "2.1.280", "976"),
        ("", "2.1.295", "8a8"),
        ("abc", "2.1.295", "8a8"),
        ("0123😀67😀0123456789😀end", "2.1.295", "be6"),
        (
            "日本語の入力です。あいうえおかきくけこさしすせそ",
            "2.1.295",
            "388",
        ),
        ("0123😀😀0123456789012X", "2.1.295", "62f"),
    ] {
        assert_eq!(version_suffix(text, version), expected, "{text:?}");
    }
    assert_eq!(
        billing_header("2.1.295", "Reply LOCAL_CAPTURE_OK."),
        "x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=00000;"
    );
}

#[test]
fn checksum_matches_native_295_routing_exclusions_and_exact_remaining_bytes() {
    let original = br#"{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":[{"type":"text","text":"Reply LOCAL_CAPTURE_OK."}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=00000;"}],"stream":true}"#;
    for (trailing_spaces, expected_checksum) in [(0, "adc2c"), (17, "018c4")] {
        let mut body = original.to_vec();
        body.extend(std::iter::repeat_n(b' ', trailing_spaces));
        let expected = String::from_utf8(body.clone()).unwrap().replacen(
            "cch=00000;",
            &format!("cch={expected_checksum};"),
            1,
        );
        finalize_billing_checksum(&mut body).unwrap();
        assert_eq!(body, expected.as_bytes());
    }
}

#[test]
fn checksum_omits_routing_values_without_mutating_the_sent_fields() {
    let fixtures = [
        (
            r#"{"model":"claude-sonnet-4-6","max_tokens":8192,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=00000;"}],"tools":[{"input_schema":{"model":"nested-model","max_tokens":42}}],"stream":true}"#,
            "b77b3",
        ),
        (
            r#"{"model":"different-model","max_tokens":16,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=00000;"}],"tools":[{"input_schema":{"model":"different-nested","max_tokens":1}}],"stream":true}"#,
            "b77b3",
        ),
        (
            r#"{"model":"claude-sonnet-4-6","system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=00000;"}],"max_tokens":8192}"#,
            "40803",
        ),
        (
            r#"{"model":"claude-sonnet-4-6","fallbacks":[["primary",{"nested":["]","\""]}]],"fallback_credit_token":"token","max_tokens":8192,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=00000;"}],"stream":true}"#,
            "8ee18",
        ),
    ];
    for (original, checksum) in fixtures {
        let mut body = original.as_bytes().to_vec();
        finalize_billing_checksum(&mut body).unwrap();
        assert_eq!(
            body,
            original
                .replacen("cch=00000;", &format!("cch={checksum};"), 1)
                .as_bytes()
        );
    }
}

#[test]
fn checksum_changes_only_root_system_first_text_and_preserves_encoding() {
    let original = r#"{
 "tools": [{"name":"sentinel","input_schema":{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=00000;"}]}}],
 "messages": [{"role":"user","content":"😀 same billing text: x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=00000;"},{"role":"tool","content":"cch=00000;"}],
 "system": [{"text":"x-anthropic-billing-header: cc_version=2.1.295.bef; cc_entrypoint=cli; cch=00000;","type":"text"},{"type":"text","text":"\nkeep this escaped"}],
 "stream": true
}"#;
    let mut body = original.as_bytes().to_vec();
    finalize_billing_checksum(&mut body).unwrap();
    let expected = original.replacen(
        "cch=00000;\",\"type\":\"text\"}",
        "cch=56aba;\",\"type\":\"text\"}",
        1,
    );
    assert_ne!(expected, original);
    assert_eq!(body, expected.as_bytes());
    assert_eq!(original.matches("cch=00000;").count(), 4);
    assert_eq!(expected.matches("cch=00000;").count(), 3);
}

#[test]
fn invalid_or_ambiguous_billing_envelopes_fail_without_mutation() {
    let header = billing_header("2.1.295", "Reply LOCAL_CAPTURE_OK.");
    let valid = serde_json::json!({"system":[{"type":"text","text":header}]}).to_string();
    let encoded_header = serde_json::to_string(&header).unwrap();
    let malformed = [
        "{}".to_owned(),
        "{\"system\":[]}".to_owned(),
        valid.replace("\"type\":\"text\"", "\"type\":\"image\""),
        valid.replace("x-anthropic-billing-header", "other-header"),
        valid.replace("cch=00000;", "cch=12345;"),
        valid.replace("cch=00000;", "cch=00000; cch=00000;"),
        valid.replace("x-anthropic", "x\\u002danthropic"),
        format!(
            r#"{{"system":[{{"type":"text","text":{encoded_header},"text":{encoded_header}}}]}}"#
        ),
        format!(r#"{{"system":[],"system":[{{"type":"text","text":{encoded_header}}}]}}"#),
        format!("{valid} trailing bytes"),
    ];
    for original in malformed {
        let mut body = original.as_bytes().to_vec();
        assert!(finalize_billing_checksum(&mut body).is_err(), "{original}");
        assert_eq!(body, original.as_bytes());
    }
}
