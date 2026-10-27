use super::*;
use serde_json::json;

fn redactor() -> SecretRedactor {
    SecretRedactor::with_known_values([])
}

/// Only for synthetic fixtures whose complete origin history is known.
fn native_provenance(messages: &Value) -> SnapshotProvenance {
    let mut mapped = Vec::new();
    for (raw_index, message) in messages.as_array().unwrap().iter().enumerate() {
        if message["role"] == "system" {
            continue;
        }
        mapped.push(MessageProvenance {
            raw_index,
            ordinal: mapped.len(),
            blocks: message["content"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(block_index, _)| BlockProvenance {
                    block_index,
                    origin: BlockOrigin::Native,
                    excluded_reason: None,
                })
                .collect(),
        });
    }
    SnapshotProvenance {
        run_id: "run-1".into(),
        snapshot_revision: 7,
        snapshot_sha256: "fixture-identity".into(),
        format_version: PROVENANCE_FORMAT_VERSION,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
        map: ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: messages.as_array().unwrap().len(),
            checkpoint_count: 1,
            messages: mapped,
        },
    }
}

fn input<'a>(messages: &'a Value, provenance: &'a SnapshotProvenance) -> IndexInput<'a> {
    IndexInput {
        run_id: "run-1",
        snapshot_revision: 7,
        source_revision: 11,
        snapshot_sha256: "fixture-identity",
        messages,
        checkpoint_count: 1,
        provenance: Some(provenance),
        latest_note: None,
    }
}

fn extract_one(block: Value, redactor: &SecretRedactor) -> IndexBlock {
    let messages = json!([{"role":"assistant", "content":[block]}]);
    extract_index_input(input(&messages, &native_provenance(&messages)), redactor)
        .unwrap()
        .blocks
        .remove(0)
}

fn tool_use(input: Value) -> Value {
    json!({"type":"tool_use", "id":"call-1", "name":"shell", "input":input})
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({"type":"tool_result", "tool_call_id":"call-1", "is_error":is_error,
        "content":[{"type":"text", "text":text}]})
}

#[test]
fn tool_use_4096_byte_neighbors_and_hash_are_exact() {
    for bytes in [4095, 4096, 4097] {
        // A JSON string adds exactly two quote bytes.
        let raw = json!("x".repeat(bytes - 2));
        assert_eq!(raw.to_string().len(), bytes);
        let block = extract_one(tool_use(raw.clone()), &redactor());
        assert_eq!(block.text, raw.to_string()[..bytes.min(4096)]);
        assert_eq!(block.truncated, bytes > 4096);
        assert_eq!(block.kind, IndexBlockKind::ToolUse);
        assert_eq!(
            block.hash,
            format!("{:x}", Sha256::digest(block.text.as_bytes()))
        );
    }
}

#[test]
fn tool_result_2048_byte_neighbors_and_large_errors() {
    for bytes in [2047, 2048, 2049, 9000] {
        for is_error in [false, true] {
            let raw = "x".repeat(bytes);
            let block = extract_one(tool_result(&raw, is_error), &redactor());
            let expected = if is_error { bytes } else { bytes.min(2048) };
            assert_eq!(block.text, raw[..expected]);
            assert_eq!(block.truncated, !is_error && bytes > 2048);
            assert_eq!(
                block.kind,
                if is_error {
                    IndexBlockKind::ToolError
                } else {
                    IndexBlockKind::ToolResult
                }
            );
        }
    }
}

#[test]
fn utf8_limits_round_down_without_marking_exact_length_truncated() {
    for (limit, is_use) in [(4096, true), (2048, false)] {
        for extra in [0, 1, 2] {
            let text = format!("{}界tail", "x".repeat(limit - 2 + extra));
            let block = if is_use {
                extract_one(tool_use(json!(text)), &redactor())
            } else {
                extract_one(tool_result(&text, false), &redactor())
            };
            assert!(block.truncated);
            assert!(block.text.len() <= limit);
            let original = if is_use {
                json!(text).to_string()
            } else {
                text
            };
            assert!(original.starts_with(&block.text));
            assert!(original.is_char_boundary(block.text.len()));
        }
    }
    let exact = format!("{}界", "x".repeat(2045));
    let block = extract_one(tool_result(&exact, false), &redactor());
    assert_eq!(block.text.len(), 2048);
    assert!(!block.truncated);
}

#[test]
fn redact_full_input_before_cutting_secrets_crossing_both_limits() {
    let secret = "credential-spanning-the-limit";
    let redactor = SecretRedactor::with_known_values([secret.into()]);
    for (limit, is_use) in [(4096, true), (2048, false)] {
        let raw = format!("{}{}", "x".repeat(limit - 10), secret);
        let expected = redactor.redact(&raw).text;
        let block = if is_use {
            extract_one(tool_use(json!(raw)), &redactor)
        } else {
            extract_one(tool_result(&raw, false), &redactor)
        };
        assert!(!block.text.contains("credential"));
        let redacted_full = if is_use {
            redactor.redact(&json!(expected).to_string()).text
        } else {
            expected
        };
        assert_eq!(block.text, redacted_full[..redacted_full.len().min(limit)]);
        assert_eq!(block.truncated, redacted_full.len() > limit);
    }
}

#[test]
fn structured_keys_and_join_created_secrets_are_redacted_again() {
    let secret = "credential-one\ncredential-two";
    let redactor = SecretRedactor::with_known_values([secret.into()]);
    let block = extract_one(
        json!({"type":"tool_result", "tool_call_id":"c", "is_error":false,
            "content":[{"type":"text","text":"credential-one"},
                       {"type":"text","text":"credential-two"}]}),
        &redactor,
    );
    assert_eq!(block.text, redactor.redact(secret).text);
    let secret = "legacy-credential";
    let redactor = SecretRedactor::with_known_values([secret.into()]);
    let block = extract_one(tool_use(json!({secret: secret})), &redactor);
    assert!(!block.text.contains(secret));
    assert_eq!(
        block.text,
        redactor
            .redact(
                &json!({
                    redactor.redact(secret).text: redactor.redact(secret).text
                })
                .to_string()
            )
            .text
    );
}

#[test]
fn excludes_hidden_unknown_and_images_without_closing_numbering_gaps() {
    let messages = json!([
        {"role":"system","content":[{"type":"text","text":"system"}]},
        {"role":"assistant","content":[
            {"type":"reasoning","text":"reasoning"},
            {"type":"thinking","thinking":"thinking"},
            {"type":"anthropic","block":{"type":"text","text":"raw"}},
            {"type":"compaction","encrypted_content":"opaque"},
            {"type":"unknown","text":"unknown"}]},
        {"role":"system","content":[]},
        {"role":"user","content":[
            {"type":"image","media_type":"image/png","data":"secret-image"},
            {"type":"text","text":"same native text"}]},
        {"role":"assistant","content":[
            {"type":"text","text":"same native text"}]}
    ]);
    let result =
        extract_index_input(input(&messages, &native_provenance(&messages)), &redactor()).unwrap();
    assert_eq!(result.blocks.len(), 2);
    assert_eq!(
        result.blocks[0].locator,
        BlockLocator::Message {
            ordinal: 1,
            block_index: 1
        }
    );
    assert_eq!(
        result.blocks[1].locator,
        BlockLocator::Message {
            ordinal: 2,
            block_index: 0
        }
    );
    assert_eq!(result.blocks[0].text, result.blocks[1].text);
    assert_eq!(result.text_bytes, 32);
}

#[test]
fn image_tool_result_only_keeps_structural_text_even_if_it_looks_like_image_note() {
    let block = extract_one(
        json!({"type":"tool_result","tool_call_id":"c","is_error":false,"content":[
            {"type":"image","media_type":"image/png","data":"private-image"},
            {"type":"text","text":"[image image/png]"},
            {"type":"mystery","text":"unknown"},
            {"type":"text","text":"actual text"}]}),
        &redactor(),
    );
    assert_eq!(block.text, "[image image/png]\nactual text");
    let messages = json!([{"role":"user","content":[
        {"type":"tool_result","tool_call_id":"c","is_error":false,
         "content":[{"type":"image","data":"hidden"}]}]}]);
    assert!(
        extract_index_input(input(&messages, &native_provenance(&messages)), &redactor())
            .unwrap()
            .blocks
            .is_empty()
    );
}

#[test]
fn malformed_json_or_known_shapes_holds_whole_run_without_payload_diagnostics() {
    let secret = "invalid-secret-payload";
    let error = parse_index_snapshot(&format!("[{secret}")).unwrap_err();
    assert_eq!(error, ExtractionError::InvalidJson);
    assert!(!format!("{error:?} {error}").contains(secret));
    for messages in [
        Value::Null,
        json!({}),
        json!([{"role":"unknown","content":[]}]),
        json!([{"role":"user","content":"not-array"}]),
        json!([{"role":"user","content":[{"type":"text","text":4}]}]),
        json!([{"role":"user","content":[{"type":"tool_result","content":[]}]}]),
    ] {
        let provenance = native_provenance(&json!([]));
        assert_eq!(
            extract_index_input(input(&messages, &provenance), &redactor()),
            Err(ExtractionError::InvalidJson)
        );
    }
}

#[test]
fn unproven_mismatched_or_incomplete_provenance_is_never_native() {
    let messages = json!([{"role":"user","content":[{"type":"text","text":"visible"}]}]);
    let good = native_provenance(&messages);
    let mut missing = input(&messages, &good);
    missing.provenance = None;
    assert_eq!(
        extract_index_input(missing, &redactor()),
        Err(unknown(ProvenanceUnknownReason::MissingSidecar))
    );
    for reason in [
        ProvenanceUnknownReason::LegacyUnproven,
        ProvenanceUnknownReason::IncompleteMap,
        ProvenanceUnknownReason::SnapshotMismatch,
        ProvenanceUnknownReason::UnsupportedVersion,
    ] {
        let mut bad = good.clone();
        match reason {
            ProvenanceUnknownReason::LegacyUnproven => {
                bad.status = ProvenanceStatus::Unknown { reason }
            }
            ProvenanceUnknownReason::IncompleteMap => bad.map.messages[0].blocks.clear(),
            ProvenanceUnknownReason::SnapshotMismatch => bad.snapshot_revision += 1,
            ProvenanceUnknownReason::UnsupportedVersion => bad.map.version += 1,
            _ => unreachable!(),
        }
        assert_eq!(
            extract_index_input(input(&messages, &bad), &redactor()),
            Err(unknown(reason))
        );
    }
}

#[test]
fn latest_note_is_full_and_empty_clear_has_no_block_or_snapshot_alias() {
    let messages = json!([]);
    let provenance = native_provenance(&messages);
    let mut note = crate::RunLedgerEntry {
        seq: 99,
        run_id: "run-1".into(),
        body: "working note ".repeat(1000),
        created_at_ns: 0,
    };
    let extract = |note: &crate::RunLedgerEntry| {
        let mut input = input(&messages, &provenance);
        input.latest_note = Some(note);
        extract_index_input(input, &redactor())
    };
    let result = extract(&note).unwrap();
    assert_eq!(
        result.blocks[0].locator,
        BlockLocator::WorkingNote { note_seq: 99 }
    );
    assert_eq!(result.blocks[0].text, note.body);
    assert!(!result.blocks[0].truncated);
    note.seq += 1;
    note.body.clear();
    assert!(extract(&note).unwrap().blocks.is_empty());
    note.run_id = "run-2".into();
    assert_eq!(extract(&note), Err(ExtractionError::NoteRunMismatch));
}

#[test]
fn fork_compaction_restart_fixture_keeps_native_duplicate_and_original_positions() {
    let messages = json!([
        {"role":"system","content":[]},
        {"role":"user","content":[{"type":"text","text":"same"}]},
        {"role":"assistant","content":[
            {"type":"text","text":"generated boundary"},
            {"type":"text","text":"same"}]}]);
    let mut provenance = native_provenance(&messages);
    provenance.map.messages[0].blocks[0].origin = BlockOrigin::ForkCopy {
        source: CopySource::Message {
            run_id: "run-0".into(),
            snapshot_revision: 3,
            ordinal: 0,
            block_index: 0,
        },
        original_origin: Box::new(BlockOrigin::Native),
        boundary: ForkBoundary {
            run_id: "run-0".into(),
            snapshot_revision: 3,
            context_len: 1,
        },
    };
    provenance.map.messages[1].blocks[0].excluded_reason = Some(ExcludedReason::GeneratedBoundary);
    // Compaction adds a separate checkpoint, leaving the raw prefix and map intact.
    let restarted: SnapshotProvenance =
        serde_json::from_str(&serde_json::to_string(&provenance).unwrap()).unwrap();
    let result = extract_index_input(input(&messages, &restarted), &redactor()).unwrap();
    assert_eq!(result.blocks.len(), 1);
    assert_eq!(result.blocks[0].text, "same");
    assert_eq!(
        result.blocks[0].locator,
        BlockLocator::Message {
            ordinal: 1,
            block_index: 1
        }
    );
}

#[test]
fn hash_uses_length_delimited_saved_bytes_and_binds_checkpoints() {
    assert_ne!(snapshot_sha256("a", "bc"), snapshot_sha256("ab", "c"));
    assert_ne!(snapshot_sha256("[]", "[]"), snapshot_sha256("[] ", "[]"));
    assert_ne!(snapshot_sha256("[]", "[]"), snapshot_sha256("[]", "[{}]"));
    let mut expected = Sha256::new();
    expected.update(2_u64.to_be_bytes());
    expected.update(b"[]");
    expected.update(2_u64.to_be_bytes());
    expected.update(b"[]");
    assert_eq!(
        snapshot_sha256("[]", "[]"),
        format!("{:x}", expected.finalize())
    );
}

#[test]
fn snapshot_save_fault_fixture_accepts_only_complete_old_or_new_bundle() {
    // Contract oracle for later SQL fault injection: no persistence is exercised
    // here. Each listed boundary must roll back to old, never expose a hybrid.
    let boundaries = [
        "after_redaction",
        "after_snapshot",
        "after_thread_binding",
        "after_sidecar",
        "after_source_revision",
        "after_state_transition",
        "before_commit",
    ];
    let before = json!([{"role":"user","content":[{"type":"text","text":"old"}]}]);
    let after = json!([{"role":"user","content":[{"type":"text","text":"new"}]}]);
    let bundle = |messages: &Value, revision, source_revision| {
        let mut provenance = native_provenance(messages);
        provenance.snapshot_revision = revision;
        provenance.map.checkpoint_count = 0;
        provenance.snapshot_sha256 = snapshot_sha256(&messages.to_string(), "[]");
        SnapshotBundle {
            record: crate::RunContextRecord {
                run_id: "run-1".into(),
                role: "Worker".into(),
                name: "fault fixture".into(),
                parent_run_id: None,
                config_json: "{}".into(),
                messages_json: messages.to_string(),
                checkpoints_json: "[]".into(),
                terminal_phase: "Done".into(),
                restorable: true,
                updated_at_ns: 0,
                host_inputs_json: "[]".into(),
            },
            snapshot_revision: revision,
            source_revision,
            provenance: Some(provenance),
            latest_note: None,
        }
    };
    let old = bundle(&before, 7, 11);
    let new = bundle(&after, 8, 12);
    for boundary in boundaries {
        for complete in [&old, &new] {
            assert!(
                extract_snapshot_bundle(complete, &redactor()).is_ok(),
                "{boundary}"
            );
        }
        for (snapshot, sidecar) in [(&old, &new), (&new, &old)] {
            let mut hybrid = snapshot.clone();
            hybrid.provenance = sidecar.provenance.clone();
            assert_eq!(
                extract_snapshot_bundle(&hybrid, &redactor()),
                Err(unknown(ProvenanceUnknownReason::SnapshotMismatch)),
                "{boundary}",
            );
        }
    }
}

#[test]
fn legacy_heuristic_secret_and_continuity_copies_are_filtered_without_deduplication() {
    let credential = format!("ghp_{}", "z".repeat(40));
    let messages = json!([{"role":"user","content":[
        {"type":"text","text":credential},
        {"type":"text","text":credential},
        {"type":"text","text":credential}]}]);
    let mut provenance = native_provenance(&messages);
    provenance.map.messages[0].blocks[0].origin = BlockOrigin::ContinuityCopy {
        source: CopySource::WorkingNote {
            run_id: "run-0".into(),
            note_seq: 4,
        },
    };
    let result = extract_index_input(input(&messages, &provenance), &redactor()).unwrap();
    assert_eq!(result.blocks.len(), 2);
    assert!(!result.blocks[0].text.contains(&credential));
    assert_eq!(result.blocks[0].text, result.blocks[1].text);
    assert_eq!(
        result.blocks[0].locator,
        BlockLocator::Message {
            ordinal: 0,
            block_index: 1
        }
    );
    assert_eq!(
        result.blocks[1].locator,
        BlockLocator::Message {
            ordinal: 0,
            block_index: 2
        }
    );
}
