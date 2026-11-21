//! Run-owned rule evidence changes only append decisions, never an already sent prefix.
use super::*;
use std::path::Path;

const ROOT_A: &str = "PROJECT-RULE-CACHE-ROOT-A";
const ROOT_B: &str = "PROJECT-RULE-CACHE-ROOT-B";
const NESTED: &str = "PROJECT-RULE-CACHE-NESTED";

fn read(id: &str, path: &Path) -> ScriptedResponse {
    call(id, "read", json!({"path":path}))
}

fn source(root: &Path) -> Arc<runtime::RulesSource> {
    Arc::new(runtime::RulesSource::new(
        runtime::ProjectTrust::Approved,
        runtime::RulesSettings {
            context_window_tokens: 1_000_000,
            response_headroom_tokens: 16_384,
            max_injection_bytes: 65_536,
        },
        None,
        Some(root.to_path_buf()),
        None,
    ))
}

fn files(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join("AGENTS.md"), ROOT_A).unwrap();
    std::fs::write(root.join("x.rs"), "target contents\n").unwrap();
}

fn make(
    wire: Wire,
    script: impl FnOnce(&Path) -> Vec<ScriptedResponse>,
) -> (Harness, tempfile::TempDir, storage::Storage) {
    // Paths have to be known before constructing the HTTP script.
    let project = tempfile::tempdir().unwrap();
    files(project.path());
    let mut h = harness_with_wire(
        wire,
        script(project.path()),
        1_000_000,
        Arc::new(tools::Read),
        vec![Arc::new(tools::Grep::default())],
        None,
        0.9,
        |_| {},
    );
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("rule-cache.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    let current = recovery_activation_cache::settings(&h);
    let (runtime, _model, receiver) = recovery_activation_cache::process_with_rule_tools(
        &h,
        &current,
        &db,
        &config,
        Arc::new(tools::Read),
        vec![Arc::new(tools::Grep::default())],
    );
    h.runtime = runtime.with_project_rules(source(project.path()));
    h.receiver = receiver;
    (h, project, db)
}

fn descriptor(config: &storage::StorageConfig, run: RunId) -> Value {
    let connection = rusqlite::Connection::open(&config.db_path).unwrap();
    let text: String = connection
        .query_row(
            "SELECT config_json FROM run_contexts WHERE run_id=?1",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn requests(h: &Harness) -> Vec<Value> {
    h.mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == h.wire.path())
        .map(|request| request.body)
        .filter(|body| !is_compaction_trigger_request(body))
        .collect()
}

fn count(request: &Value, marker: &str) -> usize {
    request.to_string().matches(marker).count()
}

async fn waiting(h: &mut Harness, run: RunId, events: &mut Vec<Event>) {
    events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Waiting).await);
}

async fn finish(h: &mut Harness, run: RunId) {
    h.runtime.cancel(run).unwrap();
    h.runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn startup_read_grep_read_does_not_repeat_rules_and_preserves_all_three_wire_prefixes() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let (mut h, root_dir, _project_db) = make(wire, |root| {
            vec![
                read("first", &root.join("x.rs")),
                call(
                    "search",
                    "grep",
                    json!({"path":root.join("x.rs"),"pattern":"target"}),
                ),
                read("again", &root.join("x.rs")),
                text_response("done"),
            ]
        });
        let _root = root_dir.path();
        let run =
            h.runtime
                .delegate_background(Role::Worker, "Inspect".into(), RunConfig::default());
        let events = through_phase(&mut h.receiver, run, AgentRunPhase::Done).await;
        h.runtime.wait(run).await.unwrap();
        let requests = requests(&h);
        assert_eq!(requests.len(), 4);
        for request in &requests {
            assert_eq!(
                count(request, ROOT_A),
                1,
                "only the startup system contains the root"
            );
        }
        let result = if matches!(wire, Wire::ChatCompletions) {
            tool_result_text(&requests[2], "search")
        } else {
            requests[2]["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "function_call_output" && item["call_id"] == "search")
                .unwrap()["output"]
                .to_string()
        };
        assert!(result.contains("target contents"));
        verify_trace(&h, run, &events, 0);
    }
}

#[tokio::test]
async fn new_and_changed_rules_append_only_the_new_sections_on_all_three_wires() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let (mut h, root_dir, _project_db) = make(wire, |root| {
            vec![
                read("root", &root.join("x.rs")),
                text_response("first turn"),
                read("nested", &root.join("nested/x.rs")),
                text_response("second turn"),
                read("changed", &root.join("x.rs")),
                call(
                    "search",
                    "grep",
                    json!({"path":root.join("nested/x.rs"),"pattern":"target"}),
                ),
                read("unchanged", &root.join("nested/x.rs")),
                text_response("done"),
            ]
        });
        let root = root_dir.path();
        let run = h.runtime.delegate_background(
            Role::Worker,
            "Inspect".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        );
        let mut events = Vec::new();
        waiting(&mut h, run, &mut events).await;
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/AGENTS.md"), NESTED).unwrap();
        std::fs::write(root.join("nested/x.rs"), "target nested").unwrap();
        h.runtime
            .send_message(run, "Inspect the new nested target".into())
            .unwrap();
        waiting(&mut h, run, &mut events).await;
        std::fs::write(root.join("AGENTS.md"), ROOT_B).unwrap();
        h.runtime
            .send_message(run, "Inspect changed rules".into())
            .unwrap();
        waiting(&mut h, run, &mut events).await;
        finish(&mut h, run).await;
        let requests = requests(&h);
        assert_eq!(requests.len(), 8);
        for (i, request) in requests.iter().enumerate() {
            assert_eq!(count(request, ROOT_A), 1);
            assert_eq!(count(request, NESTED), usize::from(i >= 3));
            assert_eq!(count(request, ROOT_B), usize::from(i >= 5));
        }
        verify_trace(&h, run, &events, 0);
    }
}

#[tokio::test]
async fn normal_restore_and_legacy_missing_evidence_keep_history_and_affinity_on_all_three_wires() {
    use super::recovery_activation_cache::{process_with_read, settings};
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        for case in ["normal", "legacy", "changed"] {
            let (mut h, root_dir, _project_db) = make(wire, |root| {
                vec![
                    read("before", &root.join("x.rs")),
                    text_response("saved"),
                    read("after", &root.join("x.rs")),
                    read("again", &root.join("x.rs")),
                    text_response("done"),
                ]
            });
            let root = root_dir.path();
            let (config, db) = plan_cache::store(&mut h);
            let current = settings(&h);
            // Both processes use the same settings and tool catalog. The helper's
            // read-only catalog deliberately has no grep in either process.
            let (old, _model, mut receiver) =
                process_with_read(&h, &current, &db, &config, Arc::new(tools::Read));
            let old = old.with_project_rules(source(root));
            let run = old
                .delegate_chat(
                    "rule-restore",
                    Role::Worker,
                    "Inspect".into(),
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
            assert!(
                descriptor(&config, run)
                    .get("project_rule_injections")
                    .is_some()
            );
            if case == "legacy" {
                let connection = rusqlite::Connection::open(&config.db_path).unwrap();
                assert_eq!(connection.execute(
                    "UPDATE run_contexts SET config_json = json_remove(config_json, '$.project_rule_injections'), config_len = length(CAST(json_remove(config_json, '$.project_rule_injections') AS BLOB)) WHERE run_id = ?1",
                    [run.to_string()],
                ).unwrap(), 1);
            }
            if case == "changed" {
                std::fs::write(root.join("AGENTS.md"), ROOT_B).unwrap();
            }
            let (new, _model, receiver) =
                process_with_read(&h, &current, &db, &config, Arc::new(tools::Read));
            h.receiver = receiver;
            h.runtime = new.with_project_rules(source(root));
            h.runtime
                .resume_chat(
                    run,
                    RunConfig {
                        interactive: true,
                        keep_alive: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            waiting(&mut h, run, &mut events).await;
            assert_eq!(
                requests(&h).len(),
                2,
                "completed turns do not replay on restore"
            );
            h.runtime.send_message(run, "Continue".into()).unwrap();
            waiting(&mut h, run, &mut events).await;
            finish(&mut h, run).await;
            let requests = requests(&h);
            assert_eq!(requests.len(), 5);
            for (i, request) in requests.iter().enumerate() {
                assert_eq!(
                    count(request, ROOT_A),
                    1 + usize::from(case == "legacy" && i >= 3)
                );
                assert_eq!(
                    count(request, ROOT_B),
                    usize::from(case == "changed" && i >= 3)
                );
            }
            verify_trace(&h, run, &events, 0);
        }
    }
}

#[tokio::test]
async fn live_and_saved_compaction_clear_all_rule_evidence_then_reinject_once_on_all_three_wires() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        for saved in [false, true] {
            let (mut h, root_dir, _project_db) = make(wire, |root| {
                let mut script = vec![
                    read("before", &root.join("x.rs")),
                    text_response(&"old analysis to summarize ".repeat(100)),
                    text_response("latest completed turn"),
                ];
                if matches!(wire, Wire::Codex) {
                    // The separate official-compaction request consumes a reply.
                    // Without a compaction item the runtime uses its structural fallback.
                    script.push(text_response("No official compaction item"));
                }
                script.extend([
                    read("after", &root.join("x.rs")),
                    read("again", &root.join("x.rs")),
                    text_response("done"),
                ]);
                script
            });
            let _root = root_dir.path();
            let config = storage::StorageConfig {
                db_path: h._directory.path().join("rule-cache.db"),
                ..Default::default()
            };
            let run = h.runtime.delegate_background(
                Role::Worker,
                "Inspect".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            );
            let mut events = Vec::new();
            waiting(&mut h, run, &mut events).await;
            h.runtime
                .send_message(run, "Keep this latest complete turn".into())
                .unwrap();
            waiting(&mut h, run, &mut events).await;
            if saved {
                h.runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
                events.extend(through_phase(&mut h.receiver, run, AgentRunPhase::Stopped).await);
                h.runtime.wait(run).await.unwrap();
                h.runtime
                    .compact_saved_chat(run, RunConfig::default())
                    .await
                    .unwrap();
                loop {
                    let event = h.receiver.recv().await.unwrap();
                    let ready = matches!(&event.kind, EventKind::Compaction(CompactionEvent::Compacted {run_id,..}) if run_id == &run.to_string());
                    events.push(event);
                    if ready {
                        break;
                    }
                }
                assert!(
                    descriptor(&config, run)
                        .get("project_rule_injections")
                        .is_none()
                );
                h.runtime.resume_chat(run, RunConfig::default()).unwrap();
                waiting(&mut h, run, &mut events).await;
            } else {
                h.runtime.compact(run).unwrap();
            }
            h.runtime
                .send_message(run, "Continue after the boundary".into())
                .unwrap();
            waiting(&mut h, run, &mut events).await;
            finish(&mut h, run).await;
            let requests = requests(&h);
            assert_eq!(
                requests.len(),
                6,
                "wire={} saved={saved}",
                wire.provider_type()
            );
            assert_eq!(
                count(&requests[3], ROOT_A),
                1,
                "startup system remains across replacement"
            );
            assert_eq!(
                count(&requests[4], ROOT_A),
                2,
                "all evidence was cleared, including startup"
            );
            assert_eq!(
                count(&requests[5], ROOT_A),
                2,
                "the next identical rule is suppressed"
            );
            verify_trace(&h, run, &events, 1);
        }
    }
}
