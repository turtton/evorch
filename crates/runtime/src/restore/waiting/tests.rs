use super::*;
use crate::quiesce::{ObservedGeneration, WaitContinuation, WaitPayload};
use providers::{Role, ToolResultContent};
use serde_json::json;

fn fixture() -> (storage::RunContextRecord, Vec<DurableWait>, Vec<Message>) {
    let run = crate::RunId::new(1);
    let input = json!({"run_ids":["run-2","run-3"],"mode":"all"});
    let legacy = json!({"run_id":"run-2","timeout_ms":99});
    let messages = vec![
        Message {
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: "stable instructions".into(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "work".into(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "read-0".into(),
                    name: "read".into(),
                    input: json!({"path":"a"}),
                },
                ContentBlock::ToolUse {
                    id: "wait-1".into(),
                    name: "wait".into(),
                    input: input.clone(),
                },
                ContentBlock::ToolUse {
                    id: "wait-2".into(),
                    name: "wait".into(),
                    input: legacy.clone(),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "read-0".into(),
                content: vec![ToolResultContent::Text {
                    text: "done".into(),
                }],
                is_error: false,
            }],
        },
    ];
    let waits: Vec<_> = [("wait-1", input, 1), ("wait-2", legacy, 2)]
        .into_iter()
        .map(|(id, input, position)| {
            let (targets, mode, legacy_result) = wait_targets(&input).unwrap();
            DurableWait {
                requester: run,
                targets,
                mode,
                tool_call_id: id.into(),
                completions: vec![ObservedGeneration {
                    id: "run-2".into(),
                    generation: 7,
                }],
                questions: vec![ObservedGeneration {
                    id: "q".into(),
                    generation: 11,
                }],
                answers: vec![ObservedGeneration {
                    id: "a".into(),
                    generation: 12,
                }],
                continuation: WaitContinuation {
                    tool_name: "wait".into(),
                    legacy_result,
                    consume_mailbox: true,
                    payload: Some(WaitPayload {
                        position,
                        input,
                        kind: WaitKind::Runs,
                    }),
                },
            }
        })
        .collect();
    let descriptor: RunRestoreDescriptor = serde_json::from_value(json!({
        "role":"worker","name":null,"parent_run_id":null,"interactive":false,"keep_alive":false,
        "category":null,"load_skills":[],"model_preference":null,"restorable":true,
        "non_restorable_reason":null,"recovery_boundary":"tools_waiting","recovery_waits":waits,
    }))
    .unwrap();
    let record = storage::RunContextRecord {
        run_id: run.to_string(),
        role: "worker".into(),
        name: "worker".into(),
        parent_run_id: None,
        config_json: serde_json::to_string(&descriptor).unwrap(),
        messages_json: serde_json::to_string(&messages).unwrap(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Checkpoint".into(),
        restorable: true,
        updated_at_ns: 1,
        host_inputs_json: "[\"prompt-1\"]".into(),
    };
    (record, waits, messages)
}

#[test]
fn tools_waiting_loader_retains_exact_batch_and_completed_results() {
    let (record, waits, messages) = fixture();
    let before = record.clone();
    validate_recovery_waits(&record, &waits).unwrap();
    assert!(RestoredState::from_record(&record).is_err());
    let bundle = storage::thread_index::SnapshotBundle {
        record,
        snapshot_revision: 17,
        source_revision: 19,
        provenance: None,
        latest_note: None,
    };
    let restored = RestoredState::from_recovery_bundle(&bundle, &waits).unwrap();
    assert_eq!(restored.messages, messages);
    assert_eq!(restored.recovery_waits, waits);
    assert_eq!(restored.snapshot_revision.get(), Some(17));
    assert_eq!(restored.host_inputs, vec!["prompt-1"]);
    assert_eq!(bundle.record, before);
    assert!(restored.recovery_waits[1].continuation.legacy_result);
}

#[test]
fn tools_waiting_proof_rejects_mismatched_contract_dimensions() {
    let (record, waits, messages) = fixture();
    type Mutation = fn(&mut Vec<DurableWait>);
    let mutations: &[Mutation] = &[
        |w| w[0].requester = crate::RunId::new(99),
        |w| w[0].targets.reverse(),
        |w| w[0].mode = DurableWaitMode::Any,
        |w| w[0].tool_call_id = "foreign".into(),
        |w| w[0].continuation.tool_name = "delegate".into(),
        |w| w[0].continuation.consume_mailbox = false,
        |w| w[0].continuation.legacy_result = true,
        |w| w[0].continuation.payload.as_mut().unwrap().position = 0,
        |w| w[0].continuation.payload.as_mut().unwrap().input = json!({}),
        |w| w[0].continuation.payload = None,
        |w| w[0].completions[0].id = "run-9".into(),
        |w| {
            let duplicate = w[0].questions[0].clone();
            w[0].questions.push(duplicate);
        },
        |w| {
            w.pop();
        },
        |w| w.reverse(),
    ];
    for mutate in mutations {
        let mut changed = waits.clone();
        mutate(&mut changed);
        assert!(validate_wait_messages(crate::RunId::new(1), &messages, &changed).is_err());
    }
    assert!(validate_recovery_waits(&record, &[]).is_err());
    let mut record = record;
    let mut descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
    descriptor.recovery_boundary = Some(CheckpointBoundary::ToolsApplied);
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    assert!(validate_recovery_waits(&record, &waits).is_err());
}

#[test]
fn unexecuted_external_tail_and_duplicate_or_reordered_results_are_not_safe() {
    let (_, waits, mut messages) = fixture();
    messages[2].content.push(ContentBlock::ToolUse {
        id: "shell-tail".into(),
        name: "shell".into(),
        input: json!({"command":"touch file"}),
    });
    assert!(validate_wait_messages(crate::RunId::new(1), &messages, &waits).is_err());
    messages[2].content.pop();
    messages.push(messages[3].clone());
    assert!(validate_wait_messages(crate::RunId::new(1), &messages, &waits).is_err());
    messages.pop();
    if let ContentBlock::ToolResult { tool_call_id, .. } = &mut messages[3].content[0] {
        *tool_call_id = "wait-1".into();
    }
    assert!(validate_wait_messages(crate::RunId::new(1), &messages, &waits).is_err());
}

#[test]
fn reply_deadline_and_answer_set_round_trip_without_rebasing() {
    let (_, mut waits, mut messages) = fixture();
    waits.truncate(1);
    messages[2].content.truncate(2);
    let input = json!({"message_id":"msg-4","timeout_ms":1000});
    messages[2].content[1] = ContentBlock::ToolUse {
        id: "wait-1".into(),
        name: "wait_reply".into(),
        input: input.clone(),
    };
    let wait = &mut waits[0];
    wait.targets = vec![crate::RunId::new(2)];
    wait.mode = DurableWaitMode::Any;
    wait.continuation.tool_name = "wait_reply".into();
    wait.continuation.payload = Some(WaitPayload {
        position: 1,
        input,
        kind: WaitKind::Reply {
            message_id: "msg-4".into(),
            deadline_unix_ms: 12345,
            delivered_answer_ids: vec!["a".into()],
        },
    });
    validate_wait_messages(crate::RunId::new(1), &messages, &waits).unwrap();
    let decoded: Vec<DurableWait> =
        serde_json::from_str(&serde_json::to_string(&waits).unwrap()).unwrap();
    assert_eq!(decoded, waits);
    if let WaitKind::Reply { message_id, .. } =
        &mut waits[0].continuation.payload.as_mut().unwrap().kind
    {
        *message_id = "wrong".into();
    }
    assert!(validate_wait_messages(crate::RunId::new(1), &messages, &waits).is_err());
}
