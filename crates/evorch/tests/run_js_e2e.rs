//! `run_js` against the real `evorch js-worker` process (ADR 0039).
//!
//! The worker runs through `DirectSandbox` here so the tests do not depend on
//! bubblewrap; `bwrap_*` runs it in the production sandbox and is ignored by
//! default.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use event_bus::EventBus;
use sandbox::DirectSandbox;
use serde_json::{Value, json};
use tools::tools::run_js::{
    ChildCall, ChildOutcome, RunJs, RunJsHost, WorkerLauncher, description, normalize,
};
use tools::{ToolExecutionContext, ToolExecutor};

struct TestHost {
    executor: Arc<ToolExecutor>,
    names: Vec<String>,
    seen: Mutex<Vec<String>>,
    settings: config::RunJsConfig,
}

impl std::fmt::Debug for TestHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TestHost")
    }
}

#[async_trait::async_trait]
impl RunJsHost for TestHost {
    fn tool_names(&self) -> Vec<String> {
        self.names.clone()
    }

    fn settings(&self) -> config::RunJsConfig {
        self.settings.clone()
    }

    fn shares_wave(&self, name: &str) -> bool {
        matches!(name, "read" | "grep")
    }

    async fn call(&self, call: ChildCall) -> ChildOutcome {
        let child_id = format!("{}/{}", call.parent_call_id, call.seq);
        self.seen.lock().unwrap().push(child_id.clone());
        let ctx = ToolExecutionContext {
            run_id: "run-e2e".into(),
            ..Default::default()
        };
        match self
            .executor
            .execute(&ctx, &call.name, &child_id, call.args.clone())
            .await
        {
            Ok(result) => ChildOutcome {
                is_error: result.is_error,
                result: Ok(normalize::normalize(&call.name, &call.args, &result).into_value()),
                paths: Vec::new(),
                written: Vec::new(),
                failure: None,
                warning: None,
            },
            Err(error) => ChildOutcome {
                result: Err(error.agent_message()),
                is_error: true,
                paths: Vec::new(),
                written: Vec::new(),
                failure: Some(error.agent_message()),
                warning: None,
            },
        }
    }
}

fn worker_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_evorch"))
}

struct Fixture {
    executor: Arc<ToolExecutor>,
    host: Arc<TestHost>,
}

fn fixture(launcher: WorkerLauncher) -> Fixture {
    fixture_with_tools(launcher, &["read", "grep"])
}

fn fixture_with_tools(launcher: WorkerLauncher, names: &[&str]) -> Fixture {
    let mut executor = ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(256)),
        Arc::new(DirectSandbox::new_unchecked()),
        None,
    );
    executor
        .register(Arc::new(RunJs::with_launcher(launcher)))
        .unwrap();
    let executor = Arc::new(executor);
    let host = Arc::new(TestHost {
        executor: Arc::clone(&executor),
        names: names.iter().map(|name| (*name).to_owned()).collect(),
        seen: Mutex::new(Vec::new()),
        settings: config::RunJsConfig::default(),
    });
    Fixture { executor, host }
}

fn direct_launcher() -> WorkerLauncher {
    WorkerLauncher::injected(
        Arc::new(DirectSandbox::new_unchecked()),
        worker_binary(),
        vec!["js-worker".into()],
    )
}

async fn run_code(fixture: &Fixture, code: &str, timeout_ms: Option<u64>) -> tools::ToolResult {
    let ctx = ToolExecutionContext {
        run_id: "run-e2e".into(),
        run_js_host: Some(fixture.host.clone()),
        ..Default::default()
    };
    let mut args = json!({"code": code});
    if let Some(timeout_ms) = timeout_ms {
        args["timeout_ms"] = timeout_ms.into();
    }
    fixture
        .executor
        .execute(&ctx, "run_js", "call-1", args)
        .await
        .unwrap()
}

fn run_detail(result: &tools::ToolResult) -> Value {
    result.detail.as_ref().unwrap()["run_js"].clone()
}

#[tokio::test]
async fn tools_read_returns_the_normalised_first_page_and_a_continuation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("long.txt");
    let text: String = (1..=700).map(|line| format!("line {line}\n")).collect();
    std::fs::write(&path, &text).unwrap();
    let fixture = fixture(direct_launcher());

    let result = run_code(
        &fixture,
        &format!(
            "const r = await tools.read({{path: {path:?}}}); return [r.ok, r.truncated, r.next.offset, r.text.split('\\n').length];",
            path = path.to_string_lossy()
        ),
        None,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.starts_with("=> [true,true,301,301]"),
        "{}",
        result.content
    );
    assert!(result.content.ends_with("[run_js: 1 calls, 0 failed]"));
    // The child's text never reaches the model.
    assert!(!result.content.contains("line 2\n"));
    assert_eq!(run_detail(&result)["calls"][0]["name"], "read");
}

#[tokio::test]
async fn read_text_assembles_every_page() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("long.txt");
    let text: String = (1..=700).map(|line| format!("line {line}\n")).collect();
    std::fs::write(&path, &text).unwrap();
    let fixture = fixture(direct_launcher());

    let result = run_code(
        &fixture,
        &format!(
            "const text = await readText({path:?}); print(text.length); return text.endsWith('line 700\\n');",
            path = path.to_string_lossy()
        ),
        None,
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(
        result
            .content
            .starts_with(&format!("{}\n=> true", text.len())),
        "{}",
        result.content
    );
    let calls = run_detail(&result)["calls"].as_array().unwrap().len();
    assert_eq!(calls, 3, "700 lines are read in pages of 300");
    assert_eq!(
        fixture.host.seen.lock().unwrap().as_slice(),
        ["call-1/1", "call-1/2", "call-1/3"]
    );
}

#[tokio::test]
async fn exceptions_report_user_code_line_numbers() {
    let fixture = fixture(direct_launcher());
    let result = run_code(
        &fixture,
        "const a = 1;\nconst b = 2;\nthrow new Error(\"boom\");",
        None,
    )
    .await;
    assert!(result.is_error);
    assert!(result.content.contains("Error: boom"), "{}", result.content);
    assert!(result.content.contains("run_js:3"), "{}", result.content);
}

#[tokio::test]
async fn an_infinite_loop_is_stopped_at_the_deadline() {
    let fixture = fixture(direct_launcher());
    let result = run_code(&fixture, "for (;;) {}", Some(300)).await;
    assert!(result.is_error);
    assert!(result.content.contains("timed out"), "{}", result.content);
    let detail = run_detail(&result);
    assert_eq!(detail["timed_out"], true);
    assert_eq!(detail["limit_hit"], "timeout");
}

#[tokio::test]
async fn a_worker_that_cannot_start_fails_the_call_instead_of_running_unsandboxed() {
    let fixture = fixture(WorkerLauncher::injected(
        Arc::new(DirectSandbox::new_unchecked()),
        PathBuf::from("/nonexistent/evorch-js-worker"),
        Vec::new(),
    ));
    let result = run_code(&fixture, "return 1;", None).await;
    assert!(result.is_error);
    assert!(
        result.content.starts_with("run_js is unavailable"),
        "{}",
        result.content
    );
}

/// Needs bubblewrap with user namespaces; `cargo test -- --ignored` to run.
#[tokio::test]
#[ignore = "requires bubblewrap"]
async fn bwrap_worker_runs_without_workspace_or_environment() {
    // SAFETY: this test binary runs this test alone with --ignored and no other
    // thread reads the variable concurrently.
    unsafe { std::env::set_var("EVORCH_JS_WORKER", worker_binary()) };
    let fixture = fixture(WorkerLauncher::production());
    let result = run_code(
        &fixture,
        "return [typeof process, typeof require, typeof fetch];",
        None,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result
            .content
            .starts_with("=> [\"undefined\",\"undefined\",\"undefined\"]"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn the_examples_in_the_description_run_as_written() {
    let directory = tempfile::tempdir().unwrap();
    let paths: Vec<String> = ["a.rs", "b.rs", "c.rs"]
        .iter()
        .map(|name| directory.path().join(name).to_string_lossy().into_owned())
        .collect();
    std::fs::write(&paths[0], "unsafe { OldName }\n").unwrap();
    std::fs::write(&paths[1], "fn safe() {}\n").unwrap();
    std::fs::write(&paths[2], "OldName again\n").unwrap();
    let with_paths = |example: &str| {
        ["a.rs", "b.rs", "c.rs"]
            .iter()
            .zip(&paths)
            .fold(example.to_owned(), |code, (name, path)| {
                code.replace(&format!("\"{name}\""), &format!("{path:?}"))
            })
    };
    let fixture = fixture_with_tools(direct_launcher(), &["read", "grep", "edit"]);

    let found = run_code(&fixture, &with_paths(description::EXAMPLES[0]), None).await;
    assert!(!found.is_error, "{}", found.content);
    assert!(
        found.content.contains(&format!("=> {}\n", paths[0])),
        "{}",
        found.content
    );

    let edited = run_code(&fixture, &with_paths(description::EXAMPLES[1]), None).await;
    assert!(!edited.is_error, "{}", edited.content);
    assert!(
        edited.content.contains(&format!("{}: true", paths[0])),
        "{}",
        edited.content
    );
    assert_eq!(
        std::fs::read_to_string(&paths[0]).unwrap(),
        "unsafe { NewName }\n"
    );
    assert_eq!(
        std::fs::read_to_string(&paths[1]).unwrap(),
        "fn safe() {}\n"
    );
    assert_eq!(
        std::fs::read_to_string(&paths[2]).unwrap(),
        "NewName again\n"
    );
}
