use config::Config;
use event_bus::{AgentRunPhase, EventBus};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use runtime::{
    CoordinationTopology, ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime,
};
use sandbox::{DirectSandbox, credential::FileCredentialStore};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tools::ToolExecutor;

fn configured(root: &tempfile::TempDir, mock: &StreamingMockOpenAi) -> runtime::AgentRuntime {
    std::fs::write(
        root.path().join("evorch.toml"),
        format!(
            r#"
[providers.local]
provider_type = "openai-compatible"
api_protocol = "openai-completions"
base_url = "{}"
credential = {{ type = "env", var = "TEAM_TEST_KEY" }}
models = ["mock-model"]
default_model = "mock-model"
[team]
enabled = true
"#,
            mock.base_url()
        ),
    )
    .unwrap();
    let config = Config::load(&config::LoadOptions {
        project_dir: Some(root.path().to_path_buf()),
        user_config_dir: Some(root.path().join("empty-user")),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(256));
    let composed = compose_runtime(RuntimeComposition {
        config: &config,
        executor: Arc::new(ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        bus,
        credential_store: Arc::new(
            FileCredentialStore::open(root.path().join("credentials")).unwrap(),
        ),
        env: Arc::new(routing::MapEnv::from_iter([("TEAM_TEST_KEY", "test-key")])),
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .unwrap();
    composed.runtime
}

async fn execute(parallel: bool) -> Duration {
    let root = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn(
        (0..4)
            .map(|id| {
                ScriptedResponse::text_stream(&id.to_string(), "mock-model", ["done"])
                    .with_delay(Duration::from_millis(250))
            })
            .collect(),
    );
    let runtime = configured(&root, &mock);
    let coordinator = runtime.delegate_background(
        Role::Orchestrator,
        "coordinate".into(),
        RunConfig {
            topology: CoordinationTopology::DynamicTeam { max_workers: 3 },
            ..Default::default()
        },
    );
    assert_eq!(
        runtime.wait(coordinator).await.unwrap(),
        AgentRunPhase::Done
    );
    let start = Instant::now();
    let mut workers = Vec::new();
    for _ in 0..3 {
        let worker = runtime
            .delegate_background_as_child(coordinator, Role::Worker, "work", RunConfig::default())
            .unwrap();
        if !parallel {
            assert_eq!(runtime.wait(worker).await.unwrap(), AgentRunPhase::Done);
        }
        workers.push(worker);
    }
    if parallel {
        let rejected = runtime
            .delegate_background_as_child(coordinator, Role::Worker, "fourth", RunConfig::default())
            .unwrap();
        assert_eq!(runtime.wait(rejected).await.unwrap(), AgentRunPhase::Error);
    }
    for worker in workers {
        assert_eq!(runtime.wait(worker).await.unwrap(), AgentRunPhase::Done);
    }
    let elapsed = start.elapsed();
    let requests = mock.recorded_requests();
    assert_eq!(requests.len(), 4);
    assert!(
        !requests[0].body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "task_claim")
    );
    for request in &requests[1..] {
        let tools = request.body["tools"].as_array().unwrap();
        for name in ["task_claim", "task_complete", "finding_append"] {
            assert!(tools.iter().any(|tool| tool["function"]["name"] == name));
        }
    }
    elapsed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_team_workers_are_faster_than_serial_over_mock_openai() {
    let serial = execute(false).await;
    let parallel = execute(true).await;
    eprintln!("team timing: parallel={parallel:?}, serial={serial:?}");
    assert!(
        parallel * 2 < serial,
        "parallel={parallel:?}, serial={serial:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_claims_appends_finding_and_completes_over_mock_openai() {
    let root = tempfile::tempdir().unwrap();
    let call = |name: &str, args: &str| {
        ScriptedResponse::tool_call(name, "mock-model", 0, name, name, [args])
    };
    let mock = StreamingMockOpenAi::spawn(vec![
        ScriptedResponse::text_stream("root", "mock-model", ["ready"]),
        call("task_claim", r#"{"task_id":"task-a"}"#),
        call(
            "finding_append",
            r#"{"task_id":"task-a","content":"verified finding","evidence":"test:team"}"#,
        ),
        call("task_complete", r#"{"task_id":"task-a","generation":1}"#),
        ScriptedResponse::text_stream("worker", "mock-model", ["done"]),
    ]);
    let runtime = configured(&root, &mock);
    let db_path = root.path().join("findings.db");
    let coordinator = runtime.delegate_background(
        Role::Orchestrator,
        "coordinate".into(),
        RunConfig {
            topology: CoordinationTopology::DynamicTeam { max_workers: 3 },
            finding_store: Some(db_path.clone()),
            ..Default::default()
        },
    );
    runtime.wait(coordinator).await.unwrap();
    let worker = runtime
        .delegate_background_as_child(
            coordinator,
            Role::Worker,
            "work",
            RunConfig {
                team_task: Some(runtime::team::TaskSpec {
                    id: "task-a".into(),
                    paths: vec![],
                }),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(runtime.wait(worker).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(
        runtime.team_tasks()[0].1[0].state,
        runtime::team::ClaimState::Complete
    );
    let db = storage::Database::open(&storage::StorageConfig {
        db_path,
        ..Default::default()
    })
    .unwrap();
    let findings = db.findings(&coordinator.to_string()).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].content, "verified finding");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_claim_expires_and_notifies_waiting_coordinator() {
    let root = tempfile::tempdir().unwrap();
    let mock = StreamingMockOpenAi::spawn(vec![
        ScriptedResponse::text_stream("root", "mock-model", ["waiting"]),
        ScriptedResponse::tool_call(
            "claim",
            "mock-model",
            0,
            "claim",
            "task_claim",
            [r#"{"task_id":"abandoned"}"#],
        ),
        ScriptedResponse::text_stream("worker", "mock-model", ["stopped without completing"]),
        ScriptedResponse::text_stream("notified", "mock-model", ["recovery observed"]),
    ]);
    let runtime = configured(&root, &mock);
    let coordinator = runtime.delegate_background(
        Role::Orchestrator,
        "coordinate".into(),
        RunConfig {
            topology: CoordinationTopology::DynamicTeam { max_workers: 3 },
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.inspect_agent(coordinator).unwrap().phase != AgentRunPhase::Waiting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let worker = runtime
        .delegate_background_as_child(
            coordinator,
            Role::Worker,
            "work",
            RunConfig {
                team_task: Some(runtime::team::TaskSpec {
                    id: "abandoned".into(),
                    paths: vec![],
                }),
                ..Default::default()
            },
        )
        .unwrap();
    runtime.wait(worker).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if mock.recorded_requests().iter().any(|request| {
                request.body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| {
                        message["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("Team leases expired"))
                    })
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        runtime.team_tasks()[0].1[0].state,
        runtime::team::ClaimState::Ready
    );
    runtime.cancel(coordinator).unwrap();
}
