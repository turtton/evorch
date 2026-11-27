//! Actual pre-change `lane_send` ToolSpec (flat lane_id + revisions; now a `targets` array).
//! The stored legacy call is rejected by the current handler: it proves the sent arguments
//! and error result survive byte-for-byte in the prefix, not old handler compatibility.
use super::*;
use providers::ToolSpec;

pub(super) fn old_tools(successor: &[ToolSpec]) -> Vec<ToolSpec> {
    let path = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("tests/recovery_activation_cache/predecessor-lane-send-tools.json");
    let old: Vec<ToolSpec> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    successor
        .iter()
        .map(|t| old.iter().find(|o| o.name == t.name).unwrap_or(t).clone())
        .collect()
}
pub(super) fn legacy_args() -> Value {
    json!({"operation_id":"old-send","lane_id":"lane-old","expected_management_revision":1,
        "expected_brief_revision":1,"message":"sent before the multi-target change"})
}
pub(super) fn legacy_call() -> ScriptedResponse {
    call("old-lane-send", "lane_send", legacy_args())
}
pub(super) fn assert_transition(
    wire: Wire,
    requests: &[Value],
    boundary: usize,
    old: &[ToolSpec],
    new: &[ToolSpec],
) {
    let before = &requests[boundary - 1];
    let after = &requests[boundary];
    assert_eq!(before["tools"], wait_schema::wire_tools(wire, old));
    assert_eq!(after["tools"], wait_schema::wire_tools(wire, new));
    assert_eq!(old, old_tools(new));
    let names = |tools: &[ToolSpec]| tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(old), names(new));
    let before_send = old.iter().find(|t| t.name == "lane_send").unwrap();
    let after_send = new.iter().find(|t| t.name == "lane_send").unwrap();
    assert_ne!(before_send.description, after_send.description);
    assert!(
        before_send.input_schema["properties"]
            .get("lane_id")
            .is_some()
    );
    assert!(
        before_send.input_schema["properties"]
            .get("targets")
            .is_none()
    );
    assert_eq!(
        after_send.input_schema["properties"]["targets"]["maxItems"],
        32
    );
    assert!(
        after_send.input_schema["properties"]
            .get("lane_id")
            .is_none()
    );
    // Nothing but lane_send differs: same tools, same order, same definitions.
    let without_send = |tools: &[ToolSpec]| {
        tools
            .iter()
            .filter(|t| t.name != "lane_send")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(without_send(old), without_send(new));
    assert_eq!(
        old.iter().position(|t| t.name == "lane_send"),
        new.iter().position(|t| t.name == "lane_send")
    );
    let history = if matches!(wire, Wire::ChatCompletions) {
        "messages"
    } else {
        "input"
    };
    let prefix = before[history].as_array().unwrap();
    for request in &requests[boundary..] {
        assert_eq!(
            &request[history].as_array().unwrap()[..prefix.len()],
            prefix
        );
        let stable = |v: &Value| {
            let mut v = v.as_object().unwrap().clone();
            v.remove("tools");
            v.remove(history);
            v
        };
        assert_eq!(stable(before), stable(request), "system/settings/affinity");
    }
    // The pre-change call and its result stay in the prefix exactly as they were sent.
    let sent = legacy_args().to_string();
    let (arguments, outputs) = if matches!(wire, Wire::ChatCompletions) {
        let call = prefix
            .iter()
            .find_map(|m| {
                m["tool_calls"]
                    .as_array()?
                    .iter()
                    .find(|c| c["id"] == "old-lane-send")
            })
            .unwrap();
        assert_eq!(call["function"]["name"], "lane_send");
        let outputs = prefix
            .iter()
            .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "old-lane-send")
            .map(|m| m["content"].clone())
            .collect::<Vec<_>>();
        (call["function"]["arguments"].clone(), outputs)
    } else {
        let call = prefix
            .iter()
            .find(|m| m["type"] == "function_call" && m["call_id"] == "old-lane-send")
            .unwrap();
        assert_eq!(call["name"], "lane_send");
        let outputs = prefix
            .iter()
            .filter(|m| m["type"] == "function_call_output" && m["call_id"] == "old-lane-send")
            .map(|m| m["output"].clone())
            .collect::<Vec<_>>();
        (call["arguments"].clone(), outputs)
    };
    assert_eq!(
        arguments,
        json!(sent),
        "arguments are the exact sent string"
    );
    assert_eq!(outputs.len(), 1, "one result for the call");
    for pair in requests[..boundary].windows(2) {
        assert_append_only(wire.protocol(), &pair[0], &pair[1]).unwrap();
    }
    for pair in requests[boundary..].windows(2) {
        assert_append_only(wire.protocol(), &pair[0], &pair[1]).unwrap();
        assert_eq!(
            pair[0]["tools"], pair[1]["tools"],
            "one tools-only boundary"
        );
    }
}

#[tokio::test]
async fn planned_lane_send_boundary_and_unchanged_build_reuse() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        // The same build cannot claim a schema change: refused before any HTTP request.
        recover_case(false, true, false, false, wire, SchemaCase::LaneSend).await;
        recover_case(false, true, true, false, wire, SchemaCase::LaneSend).await;
        recover_case(
            false,
            true,
            false,
            false,
            wire,
            SchemaCase::LaneSendUnchanged,
        )
        .await;
        recover_case(
            false,
            true,
            true,
            false,
            wire,
            SchemaCase::LaneSendUnchanged,
        )
        .await;
    }
}

#[tokio::test]
async fn non_handoff_lane_send_catalog_restart_keeps_history_and_reuses_prefix() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let mut h = harness_with_wire(
            wire,
            vec![
                legacy_call(),
                read_response(0),
                text_response("saved"),
                read_response(2),
                text_response("resumed"),
            ],
            1_000_000,
            Arc::new(BulkRead),
            Vec::new(),
            None,
            0.9,
            |_| {},
        );
        let current = settings(&h);
        let config = storage::StorageConfig {
            db_path: h._directory.path().join("lane-send-restart.db"),
            ..Default::default()
        };
        let db = storage::Storage::open(config.clone()).unwrap();
        let predecessor = Arc::new(wait_schema::PredecessorCatalog {
            lane_send: true,
            ..Default::default()
        });
        let (old, _model, mut receiver) = process_with_catalog(
            &h,
            &current,
            &db,
            &config,
            Arc::new(BulkRead),
            Some(predecessor.clone()),
        );
        let run = old
            .delegate_chat(
                "wire-recovery-thread",
                Role::Orchestrator,
                "Original task".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut events = through_phase(&mut receiver, run, AgentRunPhase::Waiting).await;
        old.stop(run, runtime::StopScope::SelfOnly).unwrap();
        events.extend(through_phase(&mut receiver, run, AgentRunPhase::Stopped).await);
        old.wait(run).await.unwrap();
        let old_catalog = old_tools(&predecessor.successor_tools.lock().unwrap());
        let (new, _model, mut receiver) =
            process_with_read(&h, &current, &db, &config, Arc::new(BulkRead));
        new.resume_chat(
            run,
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
        events.extend(through_phase(&mut receiver, run, AgentRunPhase::Waiting).await);
        new.send_message(run, "Resume with current build".into())
            .unwrap();
        events.extend(through_phase(&mut receiver, run, AgentRunPhase::Waiting).await);
        new.cancel(run).unwrap();
        new.wait(run).await.unwrap();
        let requests = h
            .mock
            .recorded_requests()
            .into_iter()
            .filter(|r| r.path == wire.path())
            .map(|r| r.body)
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 5);
        assert_transition(
            wire,
            &requests,
            3,
            &old_catalog,
            &predecessor.successor_tools.lock().unwrap(),
        );
        let usage = completed_requests(&events, run);
        assert_eq!(usage.len(), requests.len());
        assert!(usage[3].1 < usage[2].0, "changed tools really are cold");
        for (a, b) in [(0, 1), (1, 2), (3, 4)] {
            assert!(usage[b].1 >= usage[a].0);
        }
        assert_eq!(h.mock.remaining_scripts(), 0);
        assert!(
            storage::Database::open(&config)
                .unwrap()
                .restart_transaction("routed-wire")
                .unwrap()
                .is_none()
        );
        h.runtime = new;
    }
}
