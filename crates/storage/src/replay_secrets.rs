//! Shared native replay field policy. Callers must establish the typed location.
use std::collections::BTreeSet;

use secret_guard::SecretRedactor;
use serde_json::Value;

use crate::{StorageError, error::ReplaySecretFinding};

/// Only direct string fields of these native payloads are opaque. In particular,
/// neither nested lookalikes nor malformed non-string fields are exempt.
pub(crate) fn opaque_fields(kind: &str, payload: &Value) -> Vec<&'static str> {
    let candidates: &[&str] = match (kind, payload.get("type").and_then(Value::as_str)) {
        ("responses_reasoning", _) => &["encrypted_content"],
        ("anthropic", Some("thinking")) => &["signature"],
        ("anthropic", Some("redacted_thinking")) => &["data"],
        ("anthropic", Some("compaction")) => &["signature", "encrypted_content"],
        _ => &[],
    };
    candidates
        .iter()
        .copied()
        .filter(|field| payload.get(field).is_some_and(Value::is_string))
        .collect()
}

pub(crate) fn validate_intent(
    value: &Value,
    redactor: &SecretRedactor,
) -> Result<(), StorageError> {
    let mut opaque = BTreeSet::new();
    if let Some(messages) = value
        .pointer("/provider_input/request/messages")
        .and_then(Value::as_array)
    {
        for (index, message) in messages.iter().enumerate() {
            message_opaque_paths(
                message,
                &format!("/provider_input/request/messages/{index}"),
                &mut opaque,
            );
        }
    }
    if let Some(compactions) = value.get("compactions").and_then(Value::as_array) {
        for (index, checkpoint) in compactions.iter().enumerate() {
            if let Some(summary) = checkpoint.get("summary") {
                message_opaque_paths(
                    summary,
                    &format!("/compactions/{index}/summary"),
                    &mut opaque,
                );
            }
        }
    }
    let mut findings = Vec::new();
    scan(value, "", "$", &opaque, redactor, &mut findings);
    if findings.is_empty() {
        Ok(())
    } else {
        Err(StorageError::ReplaySecretDetected { findings })
    }
}

fn message_opaque_paths(message: &Value, path: &str, opaque: &mut BTreeSet<String>) {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for (index, block) in blocks.iter().enumerate() {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        let field = match kind {
            "responses_reasoning" => "item",
            "anthropic" => "block",
            _ => continue,
        };
        if let Some(payload) = block.get(field) {
            for protected in opaque_fields(kind, payload) {
                opaque.insert(format!("{path}/content/{index}/{field}/{protected}"));
            }
        }
    }
}

fn scan(
    value: &Value,
    pointer: &str,
    path: &str,
    opaque: &BTreeSet<String>,
    redactor: &SecretRedactor,
    findings: &mut Vec<ReplaySecretFinding>,
) {
    match value {
        Value::String(text) if !opaque.contains(pointer) => {
            scan_text(text, path, redactor, findings);
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                scan(
                    value,
                    &format!("{pointer}/{index}"),
                    &format!("{path}[{index}]"),
                    opaque,
                    redactor,
                    findings,
                );
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                // Object keys can themselves contain credentials. Never copy one
                // into diagnostics until it has been redacted and JSON-escaped.
                let safe_key = serde_json::to_string(&redactor.redact(key).text).unwrap();
                let field_path = format!("{path}[{safe_key}]");
                scan_text(key, &format!("{field_path} (key)"), redactor, findings);
                let escaped = key.replace('~', "~0").replace('/', "~1");
                scan(
                    value,
                    &format!("{pointer}/{escaped}"),
                    &field_path,
                    opaque,
                    redactor,
                    findings,
                );
            }
        }
        _ => {}
    }
}

fn scan_text(
    text: &str,
    path: &str,
    redactor: &SecretRedactor,
    findings: &mut Vec<ReplaySecretFinding>,
) {
    for (rule, count) in redactor.redaction_counts(text) {
        findings.push(ReplaySecretFinding {
            rule,
            path: path.into(),
            count,
        });
    }
}
