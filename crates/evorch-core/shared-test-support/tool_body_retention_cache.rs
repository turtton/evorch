//! Production host retention -> durable history -> same-build reopen/resume.
//! Registered in core_cache, hence part of CACHE_CONTRACT_FILTER.
use mock_openai::{
    ScriptedResponse, StreamingMockOpenAi,
    cache_contract::{CacheProtocol, assert_append_only},
};
use std::sync::Arc;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    commands::*,
    ids::*,
    namespaces::{ConversationCommand, ConversationQuery, ConversationReply, ProjectionQuery},
    queries::*,
    views::*,
};
use workspace_state::{SidebarDocument, SidebarStore, ThreadRecord};
const NOW: i64 = 8_000_000_000_000_000_000;
const REMOVED: &str = storage::thread_retention::tool_bodies::REMOVED;
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
async fn open(
    root: &test_socket_dir::SplitDir,
    config: &config::Config,
) -> (BootstrappedHost, event_bus::EventReceiver) {
    let bus = Arc::new(event_bus::EventBus::new(4096));
    let provider_events = bus.subscribe();
    let model = workbench_host::bootstrap::models::compose_production_model(
        config,
        &workbench_host::bootstrap::models::ProductionModel {
            load_options: config::LoadOptions {
                user_config_dir: Some(root.path().into()),
                read_env: false,
                ..Default::default()
            },
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(root.path().join("credentials"))
                    .unwrap(),
            ),
            bus,
            env: Arc::new(routing::MapEnv::from_iter([(
                "EVORCH_TEST_KEY",
                "offline-key",
            )])),
            model_catalog_dir: Some(test_support::model_catalog_dir()),
        },
    )
    .unwrap();
    let mut core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.path().into(),
            project_root: root.path().into(),
            ownership_root: root.sockets().into(),
        },
        TestSeams {
            model,
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    core.enable_retention_producers_for_test().unwrap();
    (core, provider_events)
}
fn send(core: &BootstrappedHost, id: &str, text: &str) -> runtime::RunId {
    let send = SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        submission_id: SubmissionId::new(id).unwrap(),
        composer_role: ComposerRole::Worker,
        text: text.into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    };
    let mut host = core.host.lock().unwrap();
    host.reserve_new_chat_thread(&send).unwrap();
    host.handle_command(
        Command::Conversation(ConversationCommand::SendChat(send)),
        workbench_host::provenance::local_desktop(),
    )
    .unwrap()
    .run_id
    .unwrap()
    .as_str()
    .parse()
    .unwrap()
}
fn query(
    core: &BootstrappedHost,
    q: Query,
) -> Result<QueryReply, workbench_protocol::errors::WorkbenchError> {
    core.host.lock().unwrap().handle_query(
        &SessionId::new("retention").unwrap(),
        &q,
        &workbench_host::provenance::local_desktop(),
    )
}
fn history(core: &BootstrappedHost) -> Vec<CommittedMessage> {
    let QueryReply::Conversation(ConversationReply::CommittedHistory(p)) = query(
        core,
        Query::Conversation(ConversationQuery::ReadCommittedHistory(
            ReadCommittedHistory {
                direction: HistoryDirection::Forward,
                thread: ThreadId::new("t").unwrap(),
                run: None,
                page: PageRequest {
                    cursor: None,
                    limit: 100,
                },
            },
        )),
    )
    .unwrap() else {
        panic!()
    };
    p.items
}
fn assert_removed(core: &BootstrappedHost) {
    let rows = history(core);
    let tool = rows
        .iter()
        .find(|r| r.tool.as_ref().is_some_and(|t| t.call_id == "long-call"))
        .expect("tool card");
    assert_eq!(
        tool.content,
        MessageContent::Inline {
            text: REMOVED.into()
        }
    );
    let summary = tool.tool.as_ref().unwrap();
    assert_eq!(summary.tool_name, "read");
    assert_eq!(summary.status, ToolCallStatus::Succeeded);
    let Some(MessageContent::Inline { text }) = &summary.input else {
        panic!("short retained input {summary:?}")
    };
    assert!(text.contains(REMOVED));
}
fn provider_rows(path: &std::path::Path) -> Vec<Vec<rusqlite::types::Value>> {
    let c = rusqlite::Connection::open(path).unwrap();
    let mut result = vec![];
    for table in ["run_messages", "run_contexts"] {
        let mut s = c
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let columns = s.column_count();
        result.extend(
            s.query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
                .unwrap()
                .collect::<Result<Vec<Vec<rusqlite::types::Value>>, _>>()
                .unwrap(),
        );
    }
    result
}
#[tokio::test(flavor = "multi_thread")]
async fn tool_body_retention_reopen_resume_preserves_wire_prefix_and_provider_history() {
    let root = test_socket_dir::SplitDir::new();
    let body_path = root.path().join("long-tool-body.txt");
    std::fs::write(&body_path, "tool-output-".repeat(768)).unwrap();
    let justification = "input-comment".repeat(256);
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::tool_call(
                "tool",
                "mock-model",
                0,
                "long-call",
                "read",
                [serde_json::json!({"path":body_path,"justification":justification}).to_string()],
            ),
            ScriptedResponse::text_stream("first", "mock-model", ["first done"]),
            ScriptedResponse::text_stream("resumed", "mock-model", ["resumed done"]),
            ScriptedResponse::text_stream("next", "mock-model", ["next done"]),
        ],
        vec!["mock-model".into()],
    );
    std::fs::write(
        root.path().join("config.toml"),
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "EVORCH_TEST_KEY"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.worker]]
profile = "local"
[compaction]
context_window_tokens = 1000000
[storage.thread_retention]
auto_archive_after_days = 0
archived_delete_after_days = 0
tool_body_delete_after_days = 7
"#,
            mock.base_url()
        ),
    )
    .unwrap();
    let config = config::Config::load(&config::LoadOptions {
        user_config_dir: Some(root.path().into()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let store = SidebarStore::new(root.path().join("sidebar.json"));
    let mut doc = SidebarDocument::default();
    doc.sidebar
        .add_project(workspace_state::ProjectId::new("p"), "project", root.path())
        .unwrap();
    doc.sidebar
        .set_project_trust(
            &workspace_state::ProjectId::new("p"),
            workspace_state::TrustState::Approved,
        )
        .unwrap();
    doc.sidebar.threads.push(ThreadRecord::new(
        workspace_state::ThreadId::new("t"),
        workspace_state::ProjectId::new("p"),
        "retention",
    ));
    doc.sidebar.refresh_derived();
    store.write(&store.lock().unwrap(), &doc).unwrap();
    let (mut core, _) = open(&root, &config).await;
    let mut events = core.runtime.event_bus().subscribe();
    let run = send(&core, "first", "run the long tool");
    super::turn(&mut events, run).await;
    core.flush_retention_events_for_test().unwrap();
    let rows = history(&core);
    let old = rows
        .iter()
        .find(|r| r.tool.as_ref().is_some_and(|t| t.call_id == "long-call"))
        .unwrap_or_else(|| panic!("missing tool card: {rows:?}"))
        .clone();
    let MessageContent::Body(old_body) = old.content else {
        panic!("long output should be deferred: {old:?}")
    };
    // Every Waiting (including a completed interactive turn) remains protected,
    // even though the explicit retention clock puts its persisted activity in the past.
    for _ in 0..3 {
        let r = core.tick_retention_for_test(NOW).unwrap();
        assert_eq!(r.tool_bodies.as_ref().map_or(0, |p| p.changed), 0);
        assert_eq!(r.stopped_runs, 0);
    }
    core.runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    core.runtime.wait(run).await.unwrap();
    core.runtime.wait_retention_task(run).await.unwrap();
    core.flush_retention_events_for_test().unwrap();
    let path = core.storage_config().db_path.clone();
    let before = provider_rows(&path);
    let mut changed = 0;
    for _ in 0..8 {
        let r = core.tick_retention_for_test(NOW).unwrap();
        assert!(r.failures.is_empty(), "{r:?}");
        assert_eq!(r.stopped_runs, 0);
        changed += r.tool_bodies.as_ref().map_or(0, |p| p.changed);
        if changed >= 2 {
            break;
        }
    }
    assert_eq!(changed, 2);
    core.storage_writer().checkpoint_now().unwrap();
    assert_eq!(provider_rows(&path), before);
    assert_removed(&core);
    // An old body handle can still resolve a shared provider blob, or report
    // NotFound after GC; either is safe. Retaking the history shows the marker.
    let old_result = query(
        &core,
        Query::Projection(ProjectionQuery::ReadBody(ReadBody {
            body_id: old_body.body_id,
            offset: workbench_protocol::wire::ByteOffset(0),
            max_bytes: 1024,
        })),
    );
    assert!(
        old_result.is_ok()
            || old_result.is_err_and(|e| e.code == workbench_protocol::errors::ErrorCode::NotFound)
    );
    core.shutdown().await.unwrap();
    drop(core);
    let (mut core, mut usage) = open(&root, &config).await;
    assert_removed(&core);
    // Ordinary Send restores the current conversation without a takeover step.
    let mut resumed_events = core.runtime.event_bus().subscribe();
    assert_eq!(send(&core, "second", "continue after retention"), run);
    super::turn(&mut resumed_events, run).await;
    assert_removed(&core);
    assert_eq!(send(&core, "third", "one more turn"), run);
    super::turn(&mut resumed_events, run).await;
    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path.contains("completions"))
        .collect();
    assert_eq!(requests.len(), 4);
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0].body, &pair[1].body).unwrap();
    }
    // Independent wire proof that the *original* long tool input/result survives
    // the display rewrite, as well as affinity/settings/schema prefix checks above.
    let sent = requests[2].body.to_string();
    assert!(sent.contains("input-comment"));
    assert!(sent.contains("tool-output-"));
    assert!(!sent.contains(REMOVED));
    let completed: Vec<_> = usage
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|e| match e.kind {
            event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestCompleted {
                cache_read_tokens,
                ..
            }) => Some(cache_read_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(completed.len(), 2);
    assert!(
        completed.iter().all(|n| *n > 0),
        "input-derived prefix reuse after reopen and next turn"
    );
    core.shutdown().await.unwrap();
}
