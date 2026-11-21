//! Exact wait ToolSpec from main 4a4c7fb6f06719fb72b12b70cd860e3d071b7b60,
//! immediately before implicit-parent liveness and mode-only normalization.
use super::*;
use providers::ToolSpec;

pub(super) fn old_tools(successor: &[ToolSpec]) -> Vec<ToolSpec> {
    let path = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("tests/recovery_activation_cache/predecessor-wait-liveness-tools.json");
    let old: Vec<ToolSpec> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    successor
        .iter()
        .map(|t| old.iter().find(|o| o.name == t.name).unwrap_or(t).clone())
        .collect()
}
pub(super) fn legacy_call() -> ScriptedResponse {
    call("old-wait", "wait", json!({"run_id":"run-999"}))
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
    let old_wait = old.iter().find(|t| t.name == "wait").unwrap();
    let new_wait = new.iter().find(|t| t.name == "wait").unwrap();
    assert_ne!(old_wait.description, new_wait.description);
    let mut expected = old_wait.clone();
    expected.description = new_wait.description.clone();
    expected.input_schema["oneOf"][2]["not"]["anyOf"]
        .as_array_mut()
        .unwrap()
        .pop();
    assert_eq!(expected, *new_wait);
    assert_eq!(
        old.iter().filter(|t| t.name != "wait").collect::<Vec<_>>(),
        new.iter().filter(|t| t.name != "wait").collect::<Vec<_>>()
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
async fn planned_wait_liveness_schema_boundary_and_unchanged_build_reuse() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        recover_case(false, true, true, false, wire, SchemaCase::WaitLiveness).await;
        recover_case(
            false,
            true,
            false,
            false,
            wire,
            SchemaCase::WaitLivenessUnchanged,
        )
        .await;
        recover_case(
            false,
            true,
            true,
            false,
            wire,
            SchemaCase::WaitLivenessUnchanged,
        )
        .await;
    }
}

#[tokio::test]
async fn non_handoff_wait_liveness_catalog_restart_keeps_history_and_reuses_prefix() {
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
            db_path: h._directory.path().join("attachment-restart.db"),
            ..Default::default()
        };
        let db = storage::Storage::open(config.clone()).unwrap();
        let predecessor = Arc::new(wait_schema::PredecessorCatalog {
            wait_liveness: true,
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
