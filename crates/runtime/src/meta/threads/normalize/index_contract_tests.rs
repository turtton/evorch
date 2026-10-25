//! Independent typed-provider projection vs storage's saved-Value extractor.

use super::*;
use providers::ToolResultContent;
use serde_json::json;
use storage::thread_index::*;

type Comparison = (usize, usize, IndexBlockKind, String, bool);

fn fixture(messages: &[Message]) -> (RunContextRecord, SnapshotProvenance) {
    let raw = serde_json::to_value(messages).unwrap();
    let mut positions = Vec::new();
    for (raw_index, message) in messages.iter().enumerate() {
        if message.role == Role::System {
            continue;
        }
        positions.push(MessageProvenance {
            raw_index,
            ordinal: positions.len(),
            blocks: message
                .content
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
    let record = RunContextRecord {
        run_id: "run-1".into(),
        role: "Worker".into(),
        name: "fixture".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: raw.to_string(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: true,
        updated_at_ns: 0,
    };
    let provenance = SnapshotProvenance {
        run_id: record.run_id.clone(),
        snapshot_revision: 2,
        snapshot_sha256: snapshot_sha256(&record.messages_json, &record.checkpoints_json),
        format_version: PROVENANCE_FORMAT_VERSION,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
        map: ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: messages.len(),
            checkpoint_count: 0,
            messages: positions,
        },
    };
    (record, provenance)
}

fn comparable_normalized(messages: &[Message], redactor: &SecretRedactor) -> Vec<Comparison> {
    // Remove only structural images inside tool results BEFORE normalization.
    // String replacement would also erase a real text "[image image/png]".
    // No message or top-level content index changes in this comparison adapter.
    let mut text_only = messages.to_vec();
    for message in &mut text_only {
        for block in &mut message.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                content.retain(|part| matches!(part, ToolResultContent::Text { .. }));
            }
        }
    }
    let (record, _) = fixture(&text_only);
    normalize(&record, redactor)
        .unwrap()
        .messages
        .into_iter()
        .flat_map(|message| {
            let ordinal = message.ordinal;
            message.blocks.into_iter().filter_map(move |block| {
                let kind = match block.kind {
                    BlockKind::Text => IndexBlockKind::Text,
                    BlockKind::ToolUse => IndexBlockKind::ToolUse,
                    BlockKind::ToolResult => {
                        let ContentBlock::ToolResult {
                            content, is_error, ..
                        } = &text_only_message(messages, ordinal).content[block.index]
                        else {
                            panic!("normalizer changed the block position");
                        };
                        if !content
                            .iter()
                            .any(|part| matches!(part, ToolResultContent::Text { .. }))
                        {
                            return None;
                        }
                        if *is_error {
                            IndexBlockKind::ToolError
                        } else {
                            IndexBlockKind::ToolResult
                        }
                    }
                    BlockKind::ImageNote => return None,
                };
                let mut text = redactor.redact(&block.text).text;
                let limit = match kind {
                    IndexBlockKind::ToolUse => 4096,
                    IndexBlockKind::ToolResult => 2048,
                    _ => usize::MAX,
                };
                let truncated = text.len() > limit;
                if truncated {
                    let end = text
                        .char_indices()
                        .map(|(i, _)| i)
                        .take_while(|i| *i <= limit)
                        .last()
                        .unwrap_or(0);
                    text.truncate(end);
                }
                Some((ordinal, block.index, kind, text, truncated))
            })
        })
        .collect()
}

fn text_only_message(messages: &[Message], ordinal: usize) -> &Message {
    messages
        .iter()
        .filter(|message| message.role != Role::System)
        .nth(ordinal)
        .unwrap()
}

fn compare(messages: Vec<Message>, redactor: &SecretRedactor) -> Vec<Comparison> {
    let expected = comparable_normalized(&messages, redactor);
    let (record, provenance) = fixture(&messages);
    let result = extract_snapshot_bundle(
        &SnapshotBundle {
            record,
            snapshot_revision: 2,
            source_revision: 5,
            provenance: Some(provenance),
            latest_note: None,
        },
        redactor,
    )
    .unwrap();
    let actual: Vec<_> = result
        .blocks
        .into_iter()
        .map(|block| {
            let BlockLocator::Message {
                ordinal,
                block_index,
            } = block.locator
            else {
                panic!("message fixture produced a note");
            };
            (
                ordinal,
                block_index,
                block.kind,
                block.text,
                block.truncated,
            )
        })
        .collect();
    // Full tuples, not hash-only, prefix-only or renumbered comparisons.
    assert_eq!(actual, expected);
    actual
}

#[test]
fn storage_extractor_matches_normalize_common_representation_and_keeps_positions() {
    let secret = "known-legacy-credential";
    let redactor = SecretRedactor::with_known_values([secret.into()]);
    let tuples = compare(
        vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "hidden system".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Reasoning {
                        text: "hidden reasoning".into(),
                    },
                    ContentBlock::Anthropic {
                        block: json!({"type":"text","text":"hidden raw"}),
                        prefix_hash: None,
                    },
                    ContentBlock::Compaction {
                        encrypted_content: "hidden compaction".into(),
                    },
                ],
            },
            Message {
                role: Role::System,
                content: vec![],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Image {
                        media_type: "image/png".into(),
                        data: "hidden data".into(),
                    },
                    ContentBlock::Text {
                        text: format!("user {secret} {}", "full text ".repeat(1000)),
                    },
                ],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "c".into(),
                        name: "shell".into(),
                        input: json!({secret: format!("{}界{secret}", "x".repeat(4090))}),
                    },
                    ContentBlock::Text {
                        text: "visible assistant".into(),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_call_id: "c".into(),
                        is_error: false,
                        content: vec![
                            ToolResultContent::Image {
                                media_type: "image/png".into(),
                                data: "hidden tool image".into(),
                                source: None,
                            },
                            ToolResultContent::Text {
                                text: "[image image/png]".into(),
                            },
                            ToolResultContent::Text {
                                text: format!("{}界{secret}", "s".repeat(2040)),
                            },
                        ],
                    },
                    ContentBlock::ToolResult {
                        tool_call_id: "c".into(),
                        is_error: true,
                        content: vec![ToolResultContent::Text {
                            text: format!("error {secret} {}", "e".repeat(8000)),
                        }],
                    },
                ],
            },
        ],
        &redactor,
    );
    assert_eq!((tuples[0].0, tuples[0].1), (1, 1));
    assert!(tuples[0].3.len() > 4096);
    assert_eq!(tuples[1].2, IndexBlockKind::ToolUse);
    assert!(tuples[1].3.starts_with("{\"[REDACTED"));
    assert_eq!(tuples[3].2, IndexBlockKind::ToolResult);
    assert!(tuples[3].3.starts_with("[image image/png]\n"));
    assert!(tuples[3].4);
    assert_eq!(tuples[4].2, IndexBlockKind::ToolError);
    assert!(tuples[4].3.len() > 2048);
    assert!(!tuples[4].4);
    assert!(tuples.iter().all(|tuple| !tuple.3.contains(secret)));
}

#[test]
fn normalize_contract_covers_both_limits_neighbors_and_utf8() {
    let redactor = SecretRedactor::with_known_values([]);
    for bytes in [2047, 2048, 2049, 4095, 4096, 4097] {
        for suffix in ["", "界tail"] {
            compare(
                vec![Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::ToolUse {
                            id: "c".into(),
                            name: "shell".into(),
                            input: json!(format!("{}{suffix}", "a".repeat(bytes - 2))),
                        },
                        ContentBlock::ToolResult {
                            tool_call_id: "c".into(),
                            is_error: false,
                            content: vec![ToolResultContent::Text {
                                text: format!("{}{suffix}", "b".repeat(bytes)),
                            }],
                        },
                    ],
                }],
                &redactor,
            );
        }
    }
}
