//! production composition 経由の skill / user AGENTS reload と入力由来 cache / wire prefix 契約。
//! 合成 token 数は課金推定ではない。run 間の prefix 継続は要求しない。

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use config::{Config, LoadOptions};
use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, EventReceiver, LifecycleEvent, ProviderEvent,
    UsageAggregator,
};
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::{
    AgentRuntime, ModelSource, Role, RunConfig, RunId, RuntimeComposition, WorkspaceSeam,
    compose_runtime,
};
use sandbox::credential::FileCredentialStore;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};

const MODEL: &str = "mock-model";
const KEY_ENV: &str = "EVORCH_SKILL_CACHE_TEST_KEY";
const V1: &str = "HOT-RELOAD-BODY-SENTINEL-V1";
const V2: &str = "HOT-RELOAD-BODY-SENTINEL-V2";
const AGENTS_V1: &str = "USER-AGENTS-SENTINEL-V1";
const AGENTS_V2: &str = "USER-AGENTS-SENTINEL-V2";
const SCOPE_ENV: &str = "EVORCH_SKILL_CACHE_TEST_SCOPE";

struct GatedRead {
    entered: Notify,
    release: Notify,
}

#[async_trait::async_trait]
impl Tool for GatedRead {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({"type":"object", "required":["index"], "properties":{"index":{"type":"integer"}}})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        if input["index"] == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(ToolResult::success(
            format!("観測結果 {}\n", input["index"]).repeat(80),
        ))
    }
}

struct Harness {
    _directory: tempfile::TempDir,
    skills: PathBuf,
    user_agents_md: PathBuf,
    runtime: AgentRuntime,
    receiver: EventReceiver,
    mock: StreamingMockOpenAi,
    read: Arc<GatedRead>,
}

fn write_skill(skills: &Path, name: &str, description: &str, body: &str) {
    let dir = skills.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .unwrap();
}

fn read_response(index: usize) -> ScriptedResponse {
    ScriptedResponse::tool_call(
        &format!("response-{index}"),
        MODEL,
        0,
        &format!("call-{index}"),
        "read",
        [json!({"index": index}).to_string()],
    )
}

fn harness(scope: &str) -> Harness {
    let (directory, repo) = support::init_git_repo();
    let skills = match scope {
        "repo" => repo.join(".evorch/skills"),
        "home" => PathBuf::from(std::env::var_os("HOME").unwrap()).join(".agents/skills"),
        _ => panic!("unknown fixture scope: {scope}"),
    };
    write_skill(&skills, "demo", "Demo reload skill", V1);
    let user_config = directory.path().join("user-config");
    std::fs::create_dir(&user_config).unwrap();
    let user_agents_md = user_config.join("AGENTS.md");
    std::fs::write(&user_agents_md, AGENTS_V1).unwrap();
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        read_response(0),
        read_response(1),
        ScriptedResponse::text_stream("first-done", MODEL, ["done"]),
        read_response(2),
        read_response(3),
        ScriptedResponse::text_stream("second-done", MODEL, ["done"]),
    ]);
    std::fs::create_dir_all(repo.join(".evorch")).unwrap();
    std::fs::write(
        config::project_main_config_path(&repo),
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"
[[routing.routes.orchestrator]]
profile = "local"
[compaction]
context_window_tokens = 1000000
threshold = 0.9
summarizer = "structural"
"#,
            mock.base_url()
        ),
    )
    .unwrap();
    let config = Config::load(&LoadOptions {
        project_dir: Some(repo.clone()),
        user_config_dir: Some(user_config.clone()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let receiver = bus.subscribe();
    let read = Arc::new(GatedRead {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(read.clone()).unwrap();
    let (factory, _) = support::recording_factory();
    let runtime = compose_runtime(RuntimeComposition {
        user_config_dir: Some(user_config),
        config: &config,
        bus,
        executor: Arc::new(executor),
        credential_store: Arc::new(
            FileCredentialStore::open(directory.path().join("credentials")).unwrap(),
        ),
        env: Arc::new(MapEnv::from_iter([(KEY_ENV, "offline-test-key")])),
        model_source: ModelSource::Configured,
        workspace: Some(WorkspaceSeam::with_factory(repo.clone(), factory).unwrap()),
    })
    .unwrap()
    .runtime
    .with_compaction(config.compaction.clone());
    Harness {
        _directory: directory,
        skills,
        user_agents_md,
        runtime,
        receiver,
        mock,
        read,
    }
}

fn requests(mock: &StreamingMockOpenAi) -> Vec<Value> {
    mock.recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect()
}

fn system(request: &Value) -> String {
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    messages[0]["content"].to_string()
}

async fn through_done(receiver: &mut EventReceiver, run: RunId) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let event = receiver.recv().await.unwrap();
        let terminal = matches!(&event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
            if run_id == &run.to_string() && matches!(to, AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped));
        events.push(event);
        if terminal {
            return events;
        }
    }
}

// provider request ID で usage を対応付け、同 run 内の隣接 wire request だけを検証する。
fn verify_run(run: RunId, events: &[Event], requests: &[Value]) {
    let mut starts = Vec::new();
    let mut completed = HashMap::new();
    let mut aggregator = UsageAggregator::new();
    for event in events {
        match &event.kind {
            EventKind::Provider(ProviderEvent::RequestStarted {
                request_id,
                run_id: Some(id),
                ..
            }) if id == &run.to_string() => starts.push(request_id.clone()),
            EventKind::Provider(ProviderEvent::RequestCompleted {
                request_id,
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) if id == &run.to_string() => {
                assert!(
                    completed
                        .insert(request_id.clone(), (*input_tokens, *cache_read_tokens))
                        .is_none()
                );
            }
            EventKind::Usage(usage) => aggregator.record(usage, &event.meta),
            EventKind::Compaction(_) => panic!("reload must not require compaction"),
            EventKind::Diagnostic(diagnostic) => assert_ne!(diagnostic.code, "CacheRegression"),
            _ => {}
        }
    }
    assert_eq!(requests.len(), 3);
    assert_eq!(starts.len(), requests.len());
    assert_eq!(completed.len(), requests.len());
    let mut total_input = 0;
    let mut total_cached = 0;
    for (index, id) in starts.iter().enumerate() {
        let (input, cached) = completed[id];
        assert!(input > 0 && cached <= input);
        total_input += input;
        total_cached += cached;
        if index > 0 {
            assert_append_only(
                CacheProtocol::OpenAi,
                &requests[index - 1],
                &requests[index],
            )
            .unwrap();
            assert!(
                cached >= completed[&starts[index - 1]].0,
                "input-derived mock must reuse the previous request"
            );
        }
    }
    let buckets = aggregator.drain();
    assert_eq!(
        buckets.iter().map(|b| b.request_count).sum::<u64>(),
        requests.len() as u64
    );
    assert_eq!(
        buckets.iter().map(|b| b.input_tokens).sum::<u64>(),
        total_input
    );
    assert_eq!(
        buckets.iter().map(|b| b.cache_read_tokens).sum::<u64>(),
        total_cached
    );
    assert!(total_cached > 0);
}

#[tokio::test]
async fn skill_and_user_agents_reload_preserve_wire_prefix_and_refresh_only_new_runs() {
    let Ok(scope) = std::env::var(SCOPE_ENV) else {
        // Isolate HOME/XDG in child processes; do not mutate the test runner environment.
        for scope in ["repo", "home"] {
            let directory = tempfile::tempdir().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "skill_and_user_agents_reload_preserve_wire_prefix_and_refresh_only_new_runs",
                    "--nocapture",
                ])
                .env(SCOPE_ENV, scope)
                .env("HOME", directory.path().join("home"))
                .env("XDG_CONFIG_HOME", directory.path().join("xdg"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{scope} fixture failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    let mut h = harness(&scope);
    let load_demo = || RunConfig {
        load_skills: vec!["demo".into(), "git-best-practices".into()],
        ..Default::default()
    };
    let first =
        h.runtime
            .delegate_background(Role::Orchestrator, "First skill run".into(), load_demo());
    tokio::select! {
        _ = h.read.entered.notified() => {}
        phase = h.runtime.wait(first) => panic!("run ended before entering the tool gate: {phase:?}"),
    }
    let initial = requests(&h.mock);
    assert_eq!(initial.len(), 1);
    assert!(system(&initial[0]).contains(V1));
    assert!(system(&initial[0]).contains(AGENTS_V1));
    let (_, git_body) = runtime::skill::split_frontmatter(include_str!(
        "../skills/builtin/git-best-practices/SKILL.md"
    ))
    .unwrap();
    assert!(system(&initial[0]).contains("<!-- skill:git-best-practices BEGIN -->"));
    // wire content は JSON 文字列なので同じ表現で本文の存在を確認する。
    let encoded_body = serde_json::to_string(git_body).unwrap();
    assert!(system(&initial[0]).contains(&encoded_body[1..encoded_body.len() - 1]));
    assert!(!system(&initial[0]).contains(V2));

    // tool gate で実行を止め、既送信 System の本文と metadata の両方を陳腐化させる。
    write_skill(&h.skills, "demo", "Demo reload skill", V2);
    std::fs::write(&h.user_agents_md, AGENTS_V2).unwrap();
    write_skill(
        &h.skills,
        "second",
        "Second reload skill",
        "SECOND-BODY-NOT-LOADED",
    );
    h.read.release.notify_one();
    let first_events = through_done(&mut h.receiver, first).await;
    assert_eq!(h.runtime.wait(first).await.unwrap(), AgentRunPhase::Done);
    let first_requests = requests(&h.mock);
    verify_run(first, &first_events, &first_requests);
    for request in &first_requests {
        assert_eq!(system(request), system(&initial[0]));
        assert!(system(request).contains(V1));
        assert!(system(request).contains(AGENTS_V1));
        assert!(!request.to_string().contains(AGENTS_V2));
        assert!(!request.to_string().contains(V2));
        assert!(!system(request).contains("- second: Second reload skill"));
    }

    let second =
        h.runtime
            .delegate_background(Role::Orchestrator, "Second skill run".into(), load_demo());
    let second_events = through_done(&mut h.receiver, second).await;
    assert_eq!(h.runtime.wait(second).await.unwrap(), AgentRunPhase::Done);
    let all_requests = requests(&h.mock);
    let second_requests = &all_requests[first_requests.len()..];
    verify_run(second, &second_events, second_requests);
    for request in second_requests {
        assert!(system(request).contains(V2));
        assert!(system(request).contains(AGENTS_V2));
        assert!(!system(request).contains(AGENTS_V1));
        assert!(!system(request).contains(V1));
        assert!(system(request).contains("- second: Second reload skill"));
        assert!(!system(request).contains("SECOND-BODY-NOT-LOADED"));
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
}
