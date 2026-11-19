use mock_openai::{
    ScriptedResponse, StreamingMockOpenAi,
    cache_contract::{CacheProtocol, assert_append_only},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command as OsCommand, Stdio},
};
use storage::recovery::restart::RestartPhase;
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_protocol::{
    commands::*,
    events::*,
    handshake::BuildInfo,
    ids::*,
    namespaces::{ConversationCommand, ExecutionEvent, ProjectionEvent},
    views::RunPhase,
};
mod mcp;
pub(super) mod worker;

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn checkpoint(output: &mut impl BufRead, marker: Option<&str>) -> Value {
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "process exited before {marker:?}"
        );
        if let Some(marker) = marker {
            if let Some((_, value)) = line.split_once(marker) {
                return serde_json::from_str(value).unwrap();
            }
        } else if let Ok(value) = serde_json::from_str::<Value>(&line)
            && value["status"] == "ready"
        {
            return value;
        }
    }
}
fn fixture(root: &Path, mock: &StreamingMockOpenAi) {
    let config = root.join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(
        root.join("read.txt"),
        "sent tool result must not be rewritten\n".repeat(128),
    )
    .unwrap();
    std::fs::write(
        config.join("config.toml"),
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "EVORCH_EXEC_CACHE_KEY"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.orchestrator]]
profile = "local"
[[routing.routes.worker]]
profile = "local"
[agents.orchestrator.generation]
temperature = 0.25
max_tokens = 321
[compaction]
context_window_tokens = 1000000
"#,
            mock.base_url()
        ),
    )
    .unwrap();
    let cache = root.join("cache/evorch");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::copy(
        test_support::model_catalog_dir().join("models-dev.json"),
        cache.join("models-dev.json"),
    )
    .unwrap();
    // CoreGraphRecovery deliberately owns its own current catalog directory,
    // rather than falling back to the routed model's XDG cache or the network.
    let recovery_catalog = config.join("catalog");
    std::fs::create_dir(&recovery_catalog).unwrap();
    std::fs::copy(
        test_support::model_catalog_dir().join("models-dev.json"),
        recovery_catalog.join("models-dev.json"),
    )
    .unwrap();
    let mut sidebar = workspace_state::SidebarState::default();
    sidebar
        .add_project(workspace_state::ProjectId::new("p"), "cache fixture", root)
        .unwrap();
    std::fs::write(
        config.join("sidebar.json"),
        serde_json::to_vec(&sidebar).unwrap(),
    )
    .unwrap();
}
fn stripped_copy(source: &Path, destination: &Path) -> PathBuf {
    std::fs::copy(source, destination).unwrap();
    // Hashing large debug sections repeatedly can exhaust the runner watchdog.
    // Never mutate the shared Cargo artifact; probe and exec this exact copy.
    assert!(
        OsCommand::new("strip")
            .arg("--strip-debug")
            .arg(destination)
            .status()
            .unwrap()
            .success()
    );
    destination.to_path_buf()
}
fn send(submission: &str) -> Command {
    Command::Conversation(ConversationCommand::SendChat(SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("cache-thread").unwrap(),
        submission_id: SubmissionId::new(submission).unwrap(),
        composer_role: ComposerRole::Orchestrator,
        text: format!("Continue {submission}"),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    }))
}
async fn waiting(
    client: &WorkbenchClient,
    subscription: &mut workbench_client::Subscription,
    run: &str,
) {
    let mut resyncs = 0;
    while let Some(event) = subscription.next().await {
        if let UiEvent::Projection(ProjectionEvent::ResyncRequired { reason }) = &event.event {
            resyncs += 1;
            assert!(resyncs <= 3, "too many successor resyncs: {reason:?}");
            // An explicit protocol invalidation requires a fresh snapshot, not
            // a timer, an unconditional polling loop, or a re-sent command.
            // A late Delivered receipt is projected at its acceptance time,
            // potentially before an already-published assistant message:
            // queries/history.rs detects that rewrite and invalidates the epoch.
            // This is not permission to replace the provider's sent history.
            *subscription = client
                .subscribe(Subscribe {
                    scope: SubscriptionScope::All,
                    after: None,
                })
                .await
                .unwrap();
            if subscription
                .started()
                .initial_page
                .as_ref()
                .is_some_and(|page| {
                    page.items.iter().any(|item| {
                        matches!(item, SnapshotItem::Run(summary)
                        if summary.id.as_str() == run
                            && summary.phase == RunPhase::WaitingForUser)
                    })
                })
            {
                return;
            }
            continue;
        }
        if let UiEvent::Execution(ExecutionEvent::RunChanged { run: summary }) = event.event
            && summary.id.as_str() == run
        {
            match summary.phase {
                RunPhase::WaitingForUser => return,
                RunPhase::Failed | RunPhase::Cancelled | RunPhase::Done => {
                    panic!("successor run ended: {:?}", summary.phase)
                }
                _ => (),
            }
        }
    }
    panic!(
        "subscription ended before successor turn completed: {:?}",
        subscription.end()
    );
}
pub(super) async fn check(changed_catalog: bool, disconnected_mcp: bool) {
    let root = tempfile::tempdir().unwrap();
    let sockets = test_socket_dir::SocketDir::new();
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::tool_call(
            "read-request",
            "mock-model",
            0,
            "original-read",
            "read",
            [json!({"path":root.path().join("read.txt")}).to_string()],
        ),
        ScriptedResponse::text_stream("before", "mock-model", ["parked"]),
        ScriptedResponse::text_stream("boundary", "mock-model", ["first successor turn"]),
        ScriptedResponse::text_stream("reuse", "mock-model", ["reused new prefix"]),
    ]);
    fixture(root.path(), &mock);
    let product = stripped_copy(
        Path::new(&std::env::var_os("CARGO_BIN_EXE_evorch-core").unwrap()),
        &root.path().join("product"),
    );
    let predecessor_path = stripped_copy(
        &std::env::current_exe().unwrap(),
        &root.path().join("predecessor"),
    );
    let mut process = Process(
        OsCommand::new(&predecessor_path)
            .args(["--ignored", "--exact", "cache_predecessor", "--nocapture"])
            .current_dir(root.path())
            .env("EVORCH_MASKED_TEST_CHILD", "1")
            .env("EVORCH_EXEC_CACHE_ROOT", root.path())
            .env("EVORCH_EXEC_CACHE_PRODUCT", &product)
            .env(
                "EVORCH_EXEC_CACHE_OLD_CATALOG",
                if changed_catalog { "1" } else { "0" },
            )
            .env("EVORCH_EXEC_CACHE_KEY", "offline-exec-cache-key")
            .env(
                "EVORCH_EXEC_CACHE_MCP",
                u8::from(disconnected_mcp).to_string(),
            )
            .env("XDG_RUNTIME_DIR", sockets.path())
            .env("XDG_STATE_HOME", sockets.path().join("state"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut input = process.0.stdin.take().unwrap();
    let mut output = BufReader::new(process.0.stdout.take().unwrap());
    let before = checkpoint(&mut output, Some("CACHE-BEFORE "));
    let predecessor = core_instance::handoff::elf_identity(&predecessor_path).unwrap();
    let successor = core_instance::handoff::elf_identity(&product).unwrap();
    assert_ne!(predecessor, successor, "never forge a same-build identity");
    assert_eq!(before["pid"], process.0.id());
    assert_eq!(
        mock.recorded_requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        2
    );
    if disconnected_mcp {
        let sent = mock
            .recorded_requests()
            .into_iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .map(|r| r.body)
            .collect::<Vec<_>>();
        mcp::assert_frozen_wire(&sent);
    }
    input.write_all(b"exec\n").unwrap();
    input.flush().unwrap();
    let ready = checkpoint(&mut output, None);
    assert_eq!(
        std::fs::canonicalize(format!("/proc/{}/exe", process.0.id())).unwrap(),
        std::fs::canonicalize(&product).unwrap(),
        "same PID actually exec'd the unmodified production product"
    );
    let client = WorkbenchClient::connect(
        UnixTransport::connect(Path::new(ready["socket"].as_str().unwrap()))
            .await
            .unwrap(),
        ClientConfig::new(
            ClientId::new("cache-proof").unwrap(),
            BuildInfo {
                name: "cache-proof".into(),
                version: "1".into(),
                revision: None,
            },
        ),
    )
    .await
    .unwrap();
    let mut subscription = client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .unwrap();
    let run = before["run"].as_str().unwrap();
    for submission in ["boundary", "reuse"] {
        let reply = client.command(send(submission)).await.unwrap();
        assert_eq!(
            reply.run_id.unwrap().as_str(),
            run,
            "resume the captured run, not a cold conversation"
        );
        waiting(&client, &mut subscription, run).await;
    }
    subscription.unsubscribe().await.unwrap();
    drop(client);
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(process.0.id() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    assert!(process.0.wait().unwrap().success());
    assert!(
        !Path::new(&format!("/proc/{}", process.0.id())).exists(),
        "successor reaped"
    );
    let config = storage::StorageConfig {
        db_path: root.path().join("config/evorch-events.db"),
        ..Default::default()
    };
    let db = storage::Database::open(&config).unwrap();
    let tx = db.restart_transaction("exec-cache").unwrap().unwrap();
    assert!(matches!(
        tx.phase,
        RestartPhase::Adopted | RestartPhase::Completed
    ));
    assert!(tx.ready_at_ms.is_some());
    assert_eq!(tx.metadata.old_build, predecessor);
    assert_eq!(tx.metadata.successor_build, successor);
    db.validate_restart_capture_digest("exec-cache").unwrap();
    let member = tx.expected.iter().find(|m| m.run_id == run).unwrap();
    let (evidence, consumed) = db
        .restart_tools_evidence("exec-cache", run, &member.incarnation)
        .unwrap()
        .expect("production must record the comparison");
    assert_eq!(evidence.boundary, changed_catalog);
    assert_eq!(
        consumed, changed_catalog,
        "exactly one tools-only capability"
    );
    assert_eq!(evidence.old_build, predecessor);
    assert_eq!(evidence.new_build, successor);
    assert_eq!(evidence.old.stable_context, evidence.new.stable_context);
    assert_eq!(
        evidence.old_effective_fingerprint != evidence.new_effective_fingerprint,
        changed_catalog
    );
    assert_eq!(
        evidence.old_catalog_fingerprint != evidence.new_catalog_fingerprint,
        changed_catalog
    );
    let requests = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| r.body)
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 4);
    if disconnected_mcp {
        mcp::assert_frozen_wire(&requests);
    }
    assert_eq!(mock.remaining_scripts(), 0);
    let original = &requests[1];
    let first = &requests[2];
    let next = &requests[3];
    assert_append_only(CacheProtocol::OpenAi, &requests[0], original).unwrap();
    let tool_results = |request: &Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        tool_results(original).len(),
        1,
        "a real sent ToolResult is present"
    );
    assert_eq!(tool_results(original), tool_results(first));
    assert_eq!(tool_results(first), tool_results(next));
    assert!(
        tool_results(original)[0]["content"]
            .as_str()
            .unwrap()
            .contains("sent tool result must not be rewritten")
    );
    // ChatCompletions deliberately omits wire affinity. Verify it in the DB
    // and pre-adapter ProviderInput instead; Responses wire affinity is covered
    // by codex_compaction_keeps_the_run_identity_include_and_cache_affinity and
    // codex_catalog_refresh_keeps_the_established_wire_prefix_and_cache_affinity.
    assert!(
        requests
            .iter()
            .all(|request| request.get("prompt_cache_key").is_none())
    );
    assert_eq!(original["prompt_cache_key"], first["prompt_cache_key"]);
    assert_eq!(first["prompt_cache_key"], next["prompt_cache_key"]);
    assert_eq!(original["tools"] != first["tools"], changed_catalog);
    if changed_catalog {
        let names = |request: &Value| {
            request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let old_names = names(original);
        let new_names = names(first);
        assert!(!old_names.iter().any(|n| n == "inspect_artifact"));
        assert_eq!(
            new_names
                .iter()
                .filter(|n| *n == "inspect_artifact")
                .count(),
            1
        );
        assert_eq!(
            new_names
                .into_iter()
                .filter(|n| n != "inspect_artifact")
                .collect::<Vec<_>>(),
            old_names
        );
        assert_eq!(
            first["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool["function"]["name"] != "inspect_artifact")
                .cloned()
                .collect::<Vec<_>>(),
            *original["tools"].as_array().unwrap(),
            "all other schemas and their order remain exact"
        );
        assert!(assert_append_only(CacheProtocol::OpenAi, original, first).is_err());
    }
    // Independent wire check: permit tools alone at this one boundary. Entire
    // history, system instructions, model/settings and affinity must still match.
    let mut as_before = first.clone();
    as_before["tools"] = original["tools"].clone();
    assert_append_only(CacheProtocol::OpenAi, original, &as_before).unwrap();
    assert_append_only(CacheProtocol::OpenAi, first, next).unwrap();
    let conn = rusqlite::Connection::open(&config.db_path).unwrap();
    // stable_context is a digest; inspect affinity_key in resume_intents instead.
    let current_affinity: String = conn
        .query_row(
            "SELECT json_extract(payload,'$.provider_input.affinity_key') FROM resume_intents WHERE run_id=?1",
            [run],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(current_affinity, run);
    let successor_usage = conn
        .prepare(
            "SELECT input_tokens, cache_read_tokens FROM usage_requests WHERE run_id=?1 AND rowid>?2 ORDER BY at_ns, rowid",
        )
        .unwrap()
        .query_map(rusqlite::params![run, before["usage_watermark"].as_i64().unwrap()], |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(successor_usage.len(), 2);
    let mut usage: Vec<(u64, u64)> = serde_json::from_value(before["usage"].clone()).unwrap();
    usage.extend(successor_usage);
    assert_eq!(usage.len(), 4);
    assert!(usage[1].1 > 0);
    if !changed_catalog {
        assert_eq!(
            usage[2].1, usage[1].0,
            "unchanged tools preserve the complete sent prefix"
        );
    }
    assert_eq!(
        usage[2].1 < usage[1].0,
        changed_catalog,
        "input-derived cache sees only the documented boundary"
    );
    assert_eq!(
        usage[3].1, usage[2].0,
        "the following turn reuses the new prefix"
    );
}
