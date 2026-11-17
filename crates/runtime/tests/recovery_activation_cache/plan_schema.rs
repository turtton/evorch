//! Catalog-only emulation of the predecessor at c40fbf71. The frozen ToolSpec
//! comes from that build's spec function; encoding, HTTP, cache, capture,
//! evidence consumption and activation all use the production paths.
use super::*;
use providers::ToolSpec;
use storage::recovery::restart::tools::RestartToolsEvidence;

fn retired_tool() -> ToolSpec {
    let path = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("tests/recovery_activation_cache/predecessor-plan-tool.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

pub(super) fn old_tools(successor: &[ToolSpec]) -> Vec<ToolSpec> {
    let mut old = successor
        .iter()
        .filter(|t| !["plan_read", "plan_update"].contains(&t.name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    old.push(retired_tool());
    old
}

pub(super) fn legacy_call() -> ScriptedResponse {
    call(
        "legacy-plan-call",
        &retired_tool().name,
        json!({"items":[{"content":"Historical task","status":"pending"}]}),
    )
}

#[tokio::test]
async fn planned_plan_schema_boundary_keeps_history_defers_notice_and_reuses_prefix() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        // Same-build catalog mutation remains forbidden.
        for different_build in [false, true] {
            recover_case(false, true, different_build, false, wire, SchemaCase::Plan).await;
        }
    }
}

pub(super) fn assert_evidence(e: &RestartToolsEvidence) {
    wait_schema::assert_evidence(e);
    println!(
        "plan schema boundary: old_build={} new_build={} old_catalog={} new_catalog={} old_effective={} new_effective={}",
        e.old_build,
        e.new_build,
        e.old_catalog_fingerprint,
        e.new_catalog_fingerprint,
        e.old_effective_fingerprint,
        e.new_effective_fingerprint
    );
}

pub(super) fn assert_transition(
    wire: Wire,
    requests: &[Value],
    boundary: usize,
    old: &[ToolSpec],
    new: &[ToolSpec],
    notice_deferred: bool,
) {
    let before = &requests[boundary - 1];
    let after = &requests[boundary];
    assert_eq!(before["tools"], wait_schema::wire_tools(wire, old));
    assert_eq!(after["tools"], wait_schema::wire_tools(wire, new));
    assert!(old.iter().any(|t| t == &retired_tool()));
    assert!(!new.iter().any(|t| t.name == retired_tool().name));
    for name in ["plan_read", "plan_update"] {
        assert!(!old.iter().any(|t| t.name == name));
        assert!(new.iter().any(|t| t.name == name));
    }
    // Full definitions and order of every unrelated tool must be identical.
    let unchanged_old = old
        .iter()
        .filter(|t| t.name != retired_tool().name)
        .cloned()
        .collect::<Vec<_>>();
    let unchanged_new = new
        .iter()
        .filter(|t| !["plan_read", "plan_update"].contains(&t.name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(unchanged_old, unchanged_new);
    let history = if matches!(wire, Wire::ChatCompletions) {
        "messages"
    } else {
        "input"
    };
    let prefix = before[history].as_array().unwrap();
    assert!(
        prefix
            .iter()
            .any(|m| m["tool_call_id"] == "legacy-plan-call" || m["call_id"] == "legacy-plan-call")
    );
    for request in &requests[boundary..] {
        assert_eq!(
            &request[history].as_array().unwrap()[..prefix.len()],
            prefix
        );
        let stable = |request: &Value| {
            let mut object = request.as_object().unwrap().clone();
            object.remove("tools");
            object.remove(history);
            object
        };
        assert_eq!(
            stable(request),
            stable(before),
            "system/settings/affinity are not a boundary"
        );
    }
    if notice_deferred {
        assert!(
            !after[history]
                .to_string()
                .contains(super::super::plan_cache::NOTICE),
            "tools-only request must defer notice"
        );
    }
    let following = &requests[boundary + 1];
    assert_eq!(
        following[history]
            .to_string()
            .matches(super::super::plan_cache::NOTICE)
            .count(),
        1
    );
    let appended = following[history].as_array().unwrap().last().unwrap();
    if notice_deferred {
        assert_eq!(
            appended["role"], "user",
            "notice follows the complete tool round"
        );
        assert!(
            appended
                .to_string()
                .contains(super::super::plan_cache::NOTICE)
        );
    }
    assert_append_only(wire.protocol(), after, following).unwrap();
}

#[tokio::test]
async fn non_handoff_plan_catalog_restart_keeps_history_and_reuses_the_new_prefix() {
    let mut h = harness(
        vec![
            legacy_call(),
            read_response(0),
            text_response("saved"),
            super::super::plan_cache::read("current-plan"),
            read_response(2),
            text_response("resumed"),
        ],
        1_000_000,
    );
    let current = settings(&h);
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("unplanned-restart.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    let predecessor = Arc::new(wait_schema::PredecessorCatalog {
        plan: true,
        ..Default::default()
    });
    let (old, _old_model, mut receiver) = process_with_catalog(
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
            Role::Worker,
            "Original task".into(),
            RunConfig {
                conversation: true,
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
    super::super::plan_cache::age_record(&config, run);
    let (new, _model, mut receiver) =
        process_with_read(&h, &current, &db, &config, Arc::new(BulkRead));
    new.resume_chat(
        run,
        RunConfig {
            conversation: true,
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    )
    .unwrap();
    events.extend(through_phase(&mut receiver, run, AgentRunPhase::Waiting).await);
    new.send_message(run, "Resume with the current build".into())
        .unwrap();
    events.extend(through_phase(&mut receiver, run, AgentRunPhase::Waiting).await);
    new.cancel(run).unwrap();
    new.wait(run).await.unwrap();
    let requests = chat_requests(&h);
    assert_eq!(requests.len(), 6);
    let successor = predecessor.successor_tools.lock().unwrap();
    assert_transition(
        Wire::ChatCompletions,
        &requests,
        3,
        &old_catalog,
        &successor,
        false,
    );
    let usage = completed_requests(&events, run);
    assert_eq!(usage.len(), requests.len());
    assert!(usage[3].1 < usage[2].0, "changed catalog is actually cold");
    for (before, after) in [(0, 1), (1, 2), (3, 4), (4, 5)] {
        assert_append_only(CacheProtocol::OpenAi, &requests[before], &requests[after]).unwrap();
        assert_eq!(usage[after].1, usage[before].0);
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
    // h owns an unused initial runtime; neither it nor the old process may
    // supply the successor's catalog or a hidden restart transaction.
    assert!(
        storage::Database::open(&config)
            .unwrap()
            .restart_transaction("routed-wire")
            .unwrap()
            .is_none()
    );
    h.runtime = new;
}

#[tokio::test]
async fn same_build_plan_restart_appends_the_pending_notice_without_a_schema_boundary() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        recover_case(false, true, false, false, wire, SchemaCase::PlanUnchanged).await;
    }
}

#[tokio::test]
async fn interrupted_plan_schema_boundary_retries_exactly_then_delivers_the_durable_notice() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        recover_case(false, true, true, false, wire, SchemaCase::PlanRetry).await;
    }
}

pub(super) fn assert_same_build_notice(wire: Wire, requests: &[Value], boundary: usize) {
    let before = &requests[boundary - 1];
    let history = if matches!(wire, Wire::ChatCompletions) {
        "messages"
    } else {
        "input"
    };
    for request in &requests[boundary..] {
        assert_eq!(request["tools"], before["tools"]);
        assert_eq!(
            request[history]
                .to_string()
                .matches(super::super::plan_cache::NOTICE)
                .count(),
            1
        );
    }
    let first = requests[boundary][history]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    assert_eq!(first["role"], "user");
    assert!(first.to_string().contains(super::super::plan_cache::NOTICE));
    for pair in requests.windows(2) {
        assert_append_only(wire.protocol(), &pair[0], &pair[1]).unwrap();
    }
}
