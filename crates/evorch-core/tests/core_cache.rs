//! Real Unix client -> host -> routing -> HTTP provider. Cache usage comes from
//! the fixture's input-derived cache, and wire prefix assertions are independent.
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use event_bus::{EventBus, EventKind, ProviderEvent};
use evorch_core::{backend::HostBackend, instance::Instance, unix};
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_host::{CommandHost, commands::CommandService, projects::StaticProjects};
use workbench_protocol::{commands::*, envelope::*, events::*, handshake::*, ids::*, queries::*};
use workbench_web::{WebServer, WebServerConfig};

fn send(thread: &str, submission: &str, text: &str) -> Command {
    Command::SendChat(SendChat {
        project: ProjectId::new("project").unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        submission_id: SubmissionId::new(submission).unwrap(),
        composer_role: ComposerRole::Worker,
        text: text.into(),
        skills: vec![],
        images: vec![],
        model_preference: None,
        fork_seed: None,
    })
}

async fn turn(events: &mut event_bus::EventReceiver, run: runtime::RunId) {
    loop {
        let event = events.recv().await.unwrap();
        match event.kind {
            EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted { run_id, .. })
                if run_id == run.to_string() =>
            {
                return;
            }
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: event_bus::AgentRunPhase::Error,
                reason,
                ..
            }) if run_id == run.to_string() => panic!("run failed: {reason:?}"),
            _ => (),
        }
    }
}

async fn sse_event(response: &mut reqwest::Response, buffer: &mut Vec<u8>) -> EventEnvelope {
    loop {
        if let Some(end) = buffer.windows(2).position(|w| w == b"\n\n") {
            let frame: Vec<_> = buffer.drain(..end + 2).collect();
            for line in std::str::from_utf8(&frame).unwrap().lines() {
                if let Some(data) = line.strip_prefix("data:")
                    && let Ok(event) = serde_json::from_str(data.trim())
                {
                    return event;
                }
            }
        } else {
            buffer.extend_from_slice(&response.chunk().await.unwrap().expect("SSE ended"));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unix_and_web_preserve_model_prefix_and_mask_committed_output() {
    let dir = tempfile::Builder::new()
        .prefix("p7-")
        .tempdir_in("/tmp")
        .unwrap();
    let secret = format!("sk-proj-{}", "A".repeat(64));
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::text_stream("first", "mock-model", ["hello"]),
            ScriptedResponse::text_stream("second", "mock-model", ["continued"]),
        ],
        vec!["mock-model".into()],
    );
    std::fs::write(
        dir.path().join("config.toml"),
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
"#,
            mock.base_url()
        ),
    )
    .unwrap();
    let config = config::Config::load(&config::LoadOptions {
        user_config_dir: Some(dir.path().into()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let mut observed = bus.subscribe();
    let mut turns = bus.subscribe();
    let storage_config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(storage_config.clone()).unwrap();
    let pricing = Arc::new(RwLock::new(recorder::pricing::UsagePricing::new(
        config.providers.clone(),
        None,
    )));
    let mut bridge = OwnedStorageBridge::spawn(
        bus.clone(),
        store,
        |h| StorageBridge::new(h, "core-test").with_usage_ledger(pricing),
        Duration::from_secs(3600),
    )
    .unwrap();
    let runtime = runtime::compose_runtime(runtime::RuntimeComposition {
        config: &config,
        bus: bus.clone(),
        executor: Arc::new(tools::ToolExecutor::new(bus.clone())),
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(dir.path().join("credentials")).unwrap(),
        ),
        env: Arc::new(routing::MapEnv::from_iter([(
            "EVORCH_TEST_KEY",
            "offline-test-key",
        )])),
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(dir.path().into()),
        model_source: runtime::ModelSource::Configured,
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_run_store(runtime::RunStore::open(&storage_config, bridge.handle()).unwrap());
    let owner = Arc::new(
        runtime::ownership::OwnerHost::open(
            &dir.path().join("owners"),
            Default::default(),
            bus.clone(),
        )
        .unwrap(),
    );
    let service = CommandService::new(runtime.clone(), tokio::runtime::Handle::current())
        .with_event_bus(bus.clone())
        .with_team_writer(bridge.handle())
        .with_ownership(owner.clone());
    let mut projects = StaticProjects::default();
    projects.insert(&ProjectId::new("project").unwrap(), dir.path().into());
    let host = Arc::new(Mutex::new(
        CommandHost::new(service).with_projects(projects),
    ));
    let instance = Instance::acquire(&dir.path().join("config"), dir.path()).unwrap();
    let listener = instance.bind().unwrap();
    let backend = Arc::new(HostBackend::new(host, instance.identity.clone()));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let socket_backend = backend.clone();
    let socket_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        unix::connection(stream, socket_backend, stopped)
            .await
            .unwrap();
    });
    let build = BuildInfo {
        name: "test".into(),
        version: "1".into(),
        revision: None,
    };
    let client = WorkbenchClient::connect(
        UnixTransport::connect(&instance.socket).await.unwrap(),
        ClientConfig::new(ClientId::new("desktop-test").unwrap(), build),
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
    let reply = client
        .command(send("thread", "one", "first prompt"))
        .await
        .unwrap();
    let run: runtime::RunId = reply.run_id.unwrap().as_str().parse().unwrap();
    turn(&mut turns, run).await;
    bridge.flush().unwrap();
    let query = Query::ReadCommittedHistory(ReadCommittedHistory {
        thread: ThreadId::new("thread").unwrap(),
        run: None,
        page: Default::default(),
    });
    let history = client.query(query.clone()).await.unwrap();
    let serialized = serde_json::to_string(&history).unwrap();
    assert!(serialized.contains("hello"));
    assert!(!serialized.contains(&secret));
    loop {
        let event = subscription.next().await.unwrap();
        if let UiEvent::MessageCommitted { message } = &event.event
            && serde_json::to_string(message).unwrap().contains("hello")
        {
            assert!(!serde_json::to_string(&event).unwrap().contains(&secret));
            break;
        }
    }
    let web = WebServer::start(
        WebServerConfig {
            enabled: true,
            bind: ([127, 0, 0, 1], 0).into(),
            ..Default::default()
        },
        backend.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let http = reqwest::Client::new();
    let root = format!("http://{}", web.local_addr());
    assert!(
        http.get(format!("{root}/api/v1/health"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let request = RequestEnvelope {
        request_id: RequestId::new("web-two").unwrap(),
        session_id: SessionId::new("web-test").unwrap(),
        body: RequestBody::Command(send("thread", "two", "second prompt")),
    };
    let response = http
        .post(format!("{root}/api/v1/request"))
        .header("Origin", &root)
        .json(&request)
        .send()
        .await
        .unwrap();
    let response: ReplyEnvelope = response.json().await.unwrap();
    assert!(
        matches!(response.result, ReplyResult::Success(_)),
        "{response:?}"
    );
    turn(&mut turns, run).await;
    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path.contains("completions"))
        .collect();
    assert_eq!(requests.len(), 2);
    assert_append_only(CacheProtocol::OpenAi, &requests[0].body, &requests[1].body).unwrap();
    let usage: Vec<_> = observed
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                cache_read_tokens,
                ..
            }) => Some((input_tokens, cache_read_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].1, 0);
    assert!(usage[0].0 > 0 && usage[1].1 >= usage[0].0);

    mock.enqueue([
        ScriptedResponse::tool_call(
            "ask",
            "mock-model",
            0,
            "ask",
            "ask_user",
            [serde_json::json!({"title":"Which format?", "blocking":false}).to_string()],
        ),
        ScriptedResponse::text_stream("waiting", "mock-model", ["Awaiting format"]),
        ScriptedResponse::text_stream("answered", "mock-model", ["Applied JSON"]),
    ]);
    client
        .command(send("thread", "three", "Ask me about the format"))
        .await
        .unwrap();
    turn(&mut turns, run).await;
    let QueryReply::Questions(questions) = client
        .query(Query::ListQuestions(PendingQuery {
            thread: Some(ThreadId::new("thread").unwrap()),
            pending_only: true,
            page: Default::default(),
        }))
        .await
        .unwrap()
    else {
        panic!("questions reply")
    };
    let question = &questions.items[0];
    let response: ReplyEnvelope = http
        .post(format!("{root}/api/v1/request"))
        .header("Origin", &root)
        .json(&RequestEnvelope {
            request_id: RequestId::new("web-answer").unwrap(),
            session_id: SessionId::new("web-test").unwrap(),
            body: RequestBody::Command(Command::AnswerUserQuestion(AnswerUserQuestion {
                thread: ThreadId::new("thread").unwrap(),
                question_id: question.id.clone(),
                question_revision: question.revision,
                answer: "JSON".into(),
            })),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        matches!(response.result, ReplyResult::Success(_)),
        "{response:?}"
    );
    turn(&mut turns, run).await;
    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path.contains("completions"))
        .collect();
    assert_eq!(requests.len(), 5);
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0].body, &pair[1].body).unwrap();
    }
    let more_usage: Vec<_> = observed
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                cache_read_tokens,
                ..
            }) => Some((input_tokens, cache_read_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(more_usage.len(), 3);
    let all_usage = [usage, more_usage].concat();
    for pair in all_usage.windows(2) {
        assert!(pair[1].1 >= pair[0].0);
    }

    // Secrets in diagnostics and approval summaries must never cross either
    // transport; model-output secrets are already refused by durable storage.
    let Command::SendChat(mut unknown) = send("unknown", "unknown-sub", "do not run") else {
        unreachable!()
    };
    unknown.project = ProjectId::new(&secret).unwrap();
    let response = http
        .post(format!("{root}/api/v1/request"))
        .header("Origin", &root)
        .json(&RequestEnvelope {
            request_id: RequestId::new("web-diagnostic").unwrap(),
            session_id: SessionId::new("web-test").unwrap(),
            body: RequestBody::Command(Command::SendChat(unknown.clone())),
        })
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!response.contains(&secret));
    let diagnostic = client
        .command(Command::SendChat(unknown))
        .await
        .unwrap_err();
    assert!(!format!("{diagnostic:?}").contains(&secret));
    let registry = backend.projection.approval_registry();
    let mut sse = http
        .get(format!("{root}/api/v1/events?scope=all"))
        .send()
        .await
        .unwrap();
    assert!(sse.status().is_success());
    let mut buffer = Vec::new();
    let pending = registry.register(
        sandbox::approval::ApprovalRequest {
            run_id: &run.to_string(),
            thread_id: Some("thread"),
            tool_name: "shell",
            call_id: "call",
            input: Some(serde_json::json!({"credential": secret})),
        },
        Duration::from_secs(3600),
    );
    loop {
        let event = subscription.next().await.unwrap();
        if let UiEvent::ApprovalChanged { approval } = &event.event {
            assert!(!approval.summary.contains(&secret));
            client
                .command(Command::DecideToolApproval(DecideToolApproval {
                    approval_id: approval.id.clone(),
                    core_epoch: approval.core_epoch,
                    approval_revision: approval.revision,
                    decision: ApprovalDecision::Deny,
                }))
                .await
                .unwrap();
            break;
        }
    }
    assert_eq!(
        pending.wait().await,
        sandbox::approval::ApprovalState::Denied
    );
    loop {
        if let UiEvent::ApprovalChanged { approval } = sse_event(&mut sse, &mut buffer).await.event
        {
            assert!(!approval.summary.contains(&secret));
            break;
        }
    }
    let pending = registry.register(
        sandbox::approval::ApprovalRequest {
            run_id: &run.to_string(),
            thread_id: Some("thread"),
            tool_name: "shell",
            call_id: "web-call",
            input: None,
        },
        Duration::from_secs(3600),
    );
    let approval = loop {
        if let UiEvent::ApprovalChanged { approval } = sse_event(&mut sse, &mut buffer).await.event
            && approval.id.as_str() == pending.id().value.as_str()
        {
            break approval;
        }
    };
    let reply: ReplyEnvelope = http
        .post(format!("{root}/api/v1/request"))
        .header("Origin", &root)
        .json(&RequestEnvelope {
            request_id: RequestId::new("web-approval").unwrap(),
            session_id: SessionId::new("web-test").unwrap(),
            body: RequestBody::Command(Command::DecideToolApproval(DecideToolApproval {
                approval_id: approval.id,
                core_epoch: approval.core_epoch,
                approval_revision: approval.revision,
                decision: ApprovalDecision::Deny,
            })),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(reply.result, ReplyResult::Success(_)), "{reply:?}");
    assert_eq!(
        pending.wait().await,
        sandbox::approval::ApprovalState::Denied
    );
    drop(sse);

    // An existing GUI owner cannot be adopted by this core ingress.
    let other =
        runtime::ownership::OwnerHost::open(&dir.path().join("owners"), Default::default(), bus)
            .unwrap();
    let _permit = other.start("foreign").unwrap();
    let denied = client
        .command(send("foreign", "foreign-sub", "must not run"))
        .await
        .unwrap_err();
    assert!(format!("{denied:?}").contains("PermissionDenied"));

    stop.send(true).unwrap();
    socket_task.await.unwrap();
    backend.close();
    web.shutdown().await.unwrap();
    owner.begin_quiesce().unwrap();
    runtime.stop(run, runtime::StopScope::Subtree).unwrap();
    runtime.wait(run).await.unwrap();
    bridge.flush().unwrap();
    assert!(!owner.release_ready().unwrap());
    bridge.shutdown();
}
