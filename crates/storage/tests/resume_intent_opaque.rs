//! Exact replay and observational snapshots must agree on opaque native fields.
use serde_json::{Value, json};
use storage::{
    Database, RunContextRecord, Storage, StorageConfig, resume_intent::ResumeIntentRecord,
};

fn synthetic_ciphertext() -> String {
    format!("opaque/{}-{}+/tail", "sk", "A".repeat(32))
}

fn message(ciphertext: &str) -> Value {
    // Round-trip the provider's canonical type so field positions cannot drift.
    let message: providers::Message = serde_json::from_value(json!({
        "role": "assistant",
        "content": [
            {"type": "responses_reasoning", "item": {
                "type": "reasoning", "encrypted_content": ciphertext,
                "summary": [{"type": "summary_text", "text": "safe summary"}]
            }, "origin": {
                "dialect": "open_ai", "profile": "test", "route_model": "test",
                "wire_model": "test", "speed": "standard", "service_tier": null,
                "endpoint_fingerprint": "test", "account": {"kind": "unavailable"}
            }},
            {"type": "anthropic", "block": {
                "type": "thinking", "thinking": "safe thinking", "signature": ciphertext
            }},
            {"type": "anthropic", "block": {
                "type": "redacted_thinking", "data": ciphertext
            }},
            {"type": "anthropic", "block": {
                "type": "compaction", "content": "safe summary",
                "signature": ciphertext, "encrypted_content": ciphertext
            }}
        ]
    }))
    .unwrap();
    serde_json::to_value(message).unwrap()
}

fn intent(message: &Value) -> ResumeIntentRecord {
    ResumeIntentRecord {
        run_id: "run-opaque".into(),
        incarnation: "opaque-incarnation".into(),
        payload: json!({
            "active": true,
            "provider_input": {"request": {"messages": [message]}},
            "compactions": [{"summary": message}]
        })
        .to_string(),
    }
}

#[test]
fn native_opaque_fields_survive_snapshot_and_resume_intent_close_reopen() {
    let ciphertext = synthetic_ciphertext();
    assert!(
        secret_guard::SecretRedactor::with_known_values([])
            .detect(&ciphertext)
            .is_some()
    );
    let message = message(&ciphertext);
    let record = intent(&message);
    let context = RunContextRecord {
        run_id: record.run_id.clone(),
        role: "worker".into(),
        name: "opaque-test".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: json!([message]).to_string(),
        checkpoints_json: json!([{"summary": message}]).to_string(),
        terminal_phase: "Checkpoint".into(),
        restorable: true,
        updated_at_ns: 1,
        host_inputs_json: "[]".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("opaque.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    storage.handle().upsert_run_context(&context).unwrap();
    storage
        .handle()
        .save_resume_intent(record.clone(), None)
        .unwrap();
    storage.close();

    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    let saved = db.resume_intent(&record.run_id).unwrap().unwrap();
    // Boolean assertions intentionally never print replay bytes on failure.
    assert!(saved.payload.as_bytes() == record.payload.as_bytes());
    let saved = db.run_context(&record.run_id).unwrap().unwrap();
    let messages: Value = serde_json::from_str(&saved.messages_json).unwrap();
    let checkpoints: Value = serde_json::from_str(&saved.checkpoints_json).unwrap();
    assert!(messages == json!([message]));
    assert!(checkpoints == json!([{"summary": message}]));
    storage.close();
}

#[test]
fn plaintext_and_malformed_opaque_fields_are_rejected_without_overwriting_prior_intent() {
    let secret = synthetic_ciphertext();
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("rejected.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let prior = intent(&message("safe opaque bytes"));
    handle.save_resume_intent(prior.clone(), None).unwrap();
    let payload: Value = serde_json::from_str(&prior.payload).unwrap();
    let base = "/provider_input/request/messages/0/content";
    let cases = [
        (format!("{base}/0/item/encrypted_content"), json!([secret])),
        (
            format!("{base}/0/item/encrypted_content"),
            json!({"text": secret}),
        ),
        (format!("{base}/0/item/future_plaintext"), json!(secret)),
        (
            format!("{base}/0/item/nested"),
            json!({"encrypted_content": secret}),
        ),
        (format!("{base}/0/origin/encrypted_content"), json!(secret)),
        (format!("{base}/0/encrypted_content"), json!(secret)),
        (format!("{base}/1/block/signature"), json!([secret])),
        (format!("{base}/2/block/data"), json!({"text": secret})),
        (format!("{base}/3/block/encrypted_content"), json!([secret])),
        (format!("{base}/1/block/thinking"), json!(secret)),
        (format!("{base}/3/block/future_plaintext"), json!(secret)),
        (format!("{base}/1/prefix_hash"), json!(secret)),
        (
            format!("{base}/1/block"),
            json!({"type": "future_block", "signature": secret}),
        ),
        (
            format!("{base}/0"),
            json!({"type": "text", "item": {"encrypted_content": secret}}),
        ),
        ("/encrypted_content".into(), json!(secret)),
        (
            "/unknown".into(),
            json!({"provider_input": payload["provider_input"], "encrypted_content": secret}),
        ),
        (
            "/tool_intent".into(),
            json!([{"input": {
                "type": "responses_reasoning", "item": {"encrypted_content": secret}
            }}]),
        ),
    ];
    for (index, (path, value)) in cases.into_iter().enumerate() {
        let mut bad = payload.clone();
        if let Some(existing) = bad.pointer_mut(&path) {
            *existing = value;
        } else {
            let (parent, field) = path.rsplit_once('/').unwrap();
            bad.pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(field.into(), value);
        }
        let mut record = prior.clone();
        record.payload = bad.to_string();
        let error = handle
            .save_resume_intent(record.clone(), Some(prior.incarnation.clone()))
            .unwrap_err();
        assert!(
            matches!(error, storage::StorageError::ReplaySecretDetected { .. }),
            "case {index}"
        );
        let diagnostic = format!("{error} {error:?}");
        assert!(diagnostic.contains("openai-style-key"), "case {index}");
        assert!(diagnostic.contains("count=1") || diagnostic.contains("count: 1"));
        assert!(!diagnostic.contains(&secret));
        let db = Database::open(&config).unwrap();
        assert!(db.resume_intent(&prior.run_id).unwrap().unwrap().payload == prior.payload);
        record.run_id = format!("rejected-{index}");
        assert!(handle.save_resume_intent(record.clone(), None).is_err());
        assert!(db.resume_intent(&record.run_id).unwrap().is_none());
    }
    storage.close();
}

#[test]
fn rejection_reports_each_rule_path_and_count_without_values_or_secret_keys() {
    let secret = synthetic_ciphertext();
    let other = format!("{}{}", "ghp_", "B".repeat(36));
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("diagnostics.db"),
        ..Default::default()
    };
    let storage = Storage::open(config).unwrap();
    let mut record = intent(&message("safe opaque bytes"));
    let mut value: Value = serde_json::from_str(&record.payload).unwrap();
    value["unknown"] = json!({secret.clone(): format!("{secret} {secret} {other}")});
    record.payload = value.to_string();
    let error = storage
        .handle()
        .save_resume_intent(record, None)
        .unwrap_err();
    let storage::StorageError::ReplaySecretDetected { ref findings } = error else {
        panic!("expected typed credential rejection");
    };
    assert_eq!(findings.len(), 3);
    assert!(
        findings
            .iter()
            .any(|f| f.rule.to_string() == "openai-style-key" && f.count == 2)
    );
    assert!(
        findings
            .iter()
            .any(|f| f.rule.to_string() == "github-token" && f.count == 1)
    );
    assert!(findings.iter().any(|f| f.path.ends_with(" (key)")));
    assert!(
        findings
            .iter()
            .all(|f| f.path.starts_with("$[\"unknown\"][\"opaque/"))
    );
    let diagnostic = format!("{error} {error:?}");
    assert!(!diagnostic.contains(&secret));
    assert!(!diagnostic.contains(&other));
    assert!(!diagnostic.contains("serialization failed"));
    assert!(error.is_content_rejection());
    storage.close();
}

#[test]
fn known_plaintext_credential_is_still_rejected() {
    // Isolate the environment credential list from other tests and the host.
    let known = format!("{}-{}-{}", "synthetic", "known", "value".repeat(8));
    if std::env::var_os("EVORCH_REPLAY_KNOWN_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "known_plaintext_credential_is_still_rejected"])
            .env("EVORCH_REPLAY_KNOWN_CHILD", "1")
            .env("GH_TOKEN", &known)
            .output()
            .unwrap();
        assert!(output.status.success(), "isolated known-value test failed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(StorageConfig {
        db_path: dir.path().join("known.db"),
        ..Default::default()
    })
    .unwrap();
    let mut record = intent(&message("safe opaque bytes"));
    let mut value: Value = serde_json::from_str(&record.payload).unwrap();
    value["provider_input"]["request"]["messages"][0]["content"][0]["item"]["future_plaintext"] =
        json!(known);
    record.payload = value.to_string();
    let error = storage
        .handle()
        .save_resume_intent(record, None)
        .unwrap_err();
    let storage::StorageError::ReplaySecretDetected { ref findings } = error else {
        panic!("expected typed credential rejection");
    };
    assert_eq!(findings.len(), 1);
    assert_eq!(
        findings[0].rule,
        secret_guard::SecretRule::KnownCredentialValue
    );
    assert_eq!(findings[0].count, 1);
    assert_eq!(
        findings[0].path,
        "$[\"provider_input\"][\"request\"][\"messages\"][0][\"content\"][0][\"item\"][\"future_plaintext\"]"
    );
    assert!(!format!("{error} {error:?}").contains(&known));
    storage.close();
}
