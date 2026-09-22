use super::*;

#[test]
fn redacts_multiple_shapes_known_values_and_private_key_bodies() {
    let redactor = SecretRedactor::with_known_values(["configured key with spaces".into()]);
    let source = "before sk-abcdefghijklmnopqrstuvwxyz0123456789 after configured key with spaces\n-----BEGIN OPENSSH PRIVATE KEY-----\nprivate-base64-body\n-----END OPENSSH PRIVATE KEY-----\n終わり";
    let redacted = redactor.redact(source);
    assert_eq!(redacted.count, 3);
    assert_eq!(
        redacted.text,
        "before [REDACTED:openai-style-key] after [REDACTED:known-credential-value]\n[REDACTED:private-key-block]\n終わり"
    );
    assert!(source.contains("private-base64-body"));
    assert_eq!(redactor.redact(&redacted.text).count, 0);
}

#[test]
fn unterminated_private_key_hides_the_remaining_body() {
    let text = "prefix\n-----BEGIN PRIVATE KEY-----\nsecret body";
    assert_eq!(
        SecretRedactor::from_env().redact(text).text,
        "prefix\n[REDACTED:private-key-block]"
    );
}

#[test]
fn markers_cannot_cause_an_infinite_loop_even_when_a_known_value_matches_them() {
    let redactor = SecretRedactor::with_known_values(["REDACTED".into()]);
    assert_eq!(
        redactor.redact("value REDACTED").text,
        "value [REDACTED:known-credential-value]"
    );
}

#[test]
fn configured_kimi_credential_is_redacted_without_a_recognizable_prefix() {
    let key = "kimi-fixture-without-api-key-shape-917362";
    let previous = std::env::var_os("KIMI_API_KEY");
    // SAFETY: This fixture is unique and other tests do not use this credential name.
    unsafe { std::env::set_var("KIMI_API_KEY", key) };
    let redactor = SecretRedactor::from_env();
    unsafe {
        match previous {
            Some(value) => std::env::set_var("KIMI_API_KEY", value),
            None => std::env::remove_var("KIMI_API_KEY"),
        }
    }
    assert_eq!(
        redactor.redact(key).text,
        "[REDACTED:known-credential-value]"
    );
    assert!(!format!("{redactor:?}").contains(key));
}

#[test]
fn private_key_scan_handles_unicode_at_its_window_boundary() {
    let source = format!("-----BEGIN {}日本語", "A".repeat(68));
    assert_eq!(SecretRedactor::from_env().redact(&source).text, source);
}

#[test]
fn repeated_credentials_are_redacted_without_rescanning_the_entire_log() {
    let source = "sk-abcdefghijklmnopqrstuvwxyz0123456789\n".repeat(10_000);
    let redacted = SecretRedactor::with_known_values([]).redact(&source);
    assert_eq!(redacted.count, 10_000);
    assert_eq!(
        redacted.text,
        "[REDACTED:openai-style-key]\n".repeat(10_000)
    );
}

#[test]
fn distinct_secret_json_keys_retain_both_values() {
    let mut value = serde_json::json!({
        "sk-abcdefghijklmnopqrstuvwxyz0123456789": "one",
        "sk-abcdefghijklmnopqrstuvwxyz9876543210": "two"
    });
    let count = SecretRedactor::with_known_values([]).redact_json(&mut value);
    assert_eq!(count, 2);
    assert_eq!(value.as_object().unwrap().len(), 2);
    assert!(!value.to_string().contains("sk-abcdefghijklmnopqrstuvwxyz"));
}
