use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use config::{EvalBudgetConfig, RoleBindingConfig};
use event_bus::{Event, EventKind, LifecycleEvent, MessageEvent, ToolEvent};

use super::*;
use crate::cost::Cost;
use crate::judge::{AspectResult, AspectVerdict, Preferred, ProcessAnalysis, Winner};
use crate::output::{BindingSummary, RunEntry, StartSummary};
use crate::report::{Overhead, PairReport, PriceSource};
use crate::source::tests::Recorded;
use crate::test_support::usage;

mod apply;
mod start;

const CONFIG: &str = r#"version = 3

[providers.main]
provider_type = "openai"
default_model = "gpt-test"
models = [{ id = "gpt-test", enabled = true, input_price = 2.0, output_price = 10.0 }]
"#;

/// Records notices and reloads, and answers every notice as the run's root.
struct RecordingHost {
    notices: tokio::sync::mpsc::UnboundedSender<(String, String)>,
    reloads: AtomicUsize,
}

impl EvaluatorHost for RecordingHost {
    fn deliver_notice(&self, run_id: &str, notice: &str) -> Result<String, String> {
        let _ = self.notices.send((run_id.to_owned(), notice.to_owned()));
        Ok(format!("{run_id}-root"))
    }

    fn reload_config(&self) -> Result<(), String> {
        self.reloads.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Fixture {
    recorded: Recorded,
    user: tempfile::TempDir,
    _bin: tempfile::TempDir,
    bus: Arc<EventBus>,
    host: Arc<RecordingHost>,
    notices: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<(String, String)>>>,
    tools: Vec<Arc<dyn Tool>>,
}

impl Fixture {
    fn new() -> Self {
        let recorded = Recorded::new();
        let user = tempfile::tempdir().unwrap();
        std::fs::write(user.path().join("config.toml"), CONFIG).unwrap();
        let bin = tempfile::tempdir().unwrap();
        let bus = Arc::new(EventBus::new(1024));
        let (sender, notices) = tokio::sync::mpsc::unbounded_channel();
        let host = Arc::new(RecordingHost {
            notices: sender,
            reloads: AtomicUsize::new(0),
        });
        let tools = tools(EvaluatorContext {
            database: recorded.config.db_path.clone(),
            storage: recorded.storage.handle(),
            user_config_dir: user.path().to_path_buf(),
            catalog_dir: None,
            bus: Arc::clone(&bus),
            host: Arc::clone(&host) as Arc<dyn EvaluatorHost>,
            cli: Some(start::fake_cli(bin.path())),
        });
        Self {
            recorded,
            user,
            _bin: bin,
            bus,
            host,
            notices: Mutex::new(Some(notices)),
            tools,
        }
    }

    async fn call_in_run(&self, name: &str, args: Value) -> ToolResult {
        let ctx = ToolExecutionContext {
            run_id: "run-7".into(),
            thread_id: None,
            call_id: Some(format!("call-{name}")),
            ..Default::default()
        };
        tokio::time::timeout(
            Duration::from_secs(20),
            self.tool(name).execute_with_context(&ctx, args),
        )
        .await
        .expect("the tool answers without waiting for an approval nobody gives")
        .unwrap()
    }

    /// Answer the next approval request and return what it showed.
    fn answer_approval(&self, approved: bool) -> tokio::task::JoinHandle<Value> {
        let mut events = self.bus.subscribe();
        let bus = Arc::clone(&self.bus);
        tokio::spawn(async move {
            loop {
                let event = events.recv().await.unwrap();
                if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, input, .. }) =
                    event.kind
                {
                    bus.emit(Event::new(ToolEvent::ApprovalResolved {
                        call_id,
                        approved,
                    }));
                    return input.unwrap();
                }
            }
        })
    }

    async fn notice(&self) -> (String, String) {
        let mut notices = self.notices.lock().unwrap().take().unwrap();
        let notice = tokio::time::timeout(Duration::from_secs(20), notices.recv())
            .await
            .expect("a completion notice")
            .unwrap();
        *self.notices.lock().unwrap() = Some(notices);
        notice
    }

    fn pending_notices(&self) -> usize {
        let mut notices = self.notices.lock().unwrap();
        let notices = notices.as_mut().unwrap();
        let mut pending = 0;
        while notices.try_recv().is_ok() {
            pending += 1;
        }
        pending
    }

    async fn pin(&self, thread_id: &str, problem: &str) {
        let saved = self
            .call(
                "eval_save_sample",
                json!({"thread_id": thread_id, "problem": problem, "focus_roles": ["worker"]}),
            )
            .await;
        assert!(!saved.is_error, "{}", saved.content);
    }

    fn project(&self) -> PathBuf {
        self.recorded.source.root().canonicalize().unwrap()
    }

    /// A recorded thread whose root delegated to an explorer, which read a file,
    /// was denied a shell command and made one priced request.
    async fn thread(&self, thread_id: &str) -> String {
        let root = self
            .recorded
            .conversation(thread_id, &self.project())
            .await
            .to_string();
        let child = format!("{root}-explorer");
        let handle = self.recorded.storage.handle();
        let events = [
            Event::new(LifecycleEvent::AgentRunStarted {
                run_id: child.clone(),
                parent_run_id: Some(root.clone()),
                agent_name: "explorer".into(),
                role: "explorer".into(),
            }),
            Event::new(ToolEvent::ToolStarted {
                tool_name: "read".into(),
                call_id: "call-1".into(),
                input: Some(json!({"path": "lib.rs"})),
                run_id: Some(child.clone()),
            }),
            Event::new(ToolEvent::ApprovalRequested {
                tool_name: "shell".into(),
                call_id: format!("{child}:call-2"),
                input: Some(json!({"command": "curl example.com"})),
            }),
            Event::new(ToolEvent::ExecutionDenied {
                tool_name: "shell".into(),
                call_id: format!("{child}:call-2"),
                reason: "the user declined".into(),
            }),
            Event::new(MessageEvent::MessageCompleted {
                run_id: root.clone(),
                text: "patched lib.rs".into(),
            }),
        ];
        for event in &events {
            handle.append_event(Some("gui"), event).unwrap();
        }
        let mut request = usage(
            &format!("{thread_id}-r1"),
            "main",
            "gpt-test",
            "agent",
            1_000_000,
        );
        request.run_id = Some(child);
        request.parent_run_id = Some(root.clone());
        handle.record_usage_requests(vec![request]).unwrap();
        root
    }

    fn tool(&self, name: &str) -> &Arc<dyn Tool> {
        self.tools.iter().find(|tool| tool.name() == name).unwrap()
    }

    async fn call(&self, name: &str, args: Value) -> ToolResult {
        self.tool(name).execute(args).await.unwrap()
    }

    async fn json(&self, name: &str, args: Value) -> Value {
        let result = self.call(name, args).await;
        assert!(!result.is_error, "{name} failed: {}", result.content);
        serde_json::from_str(&result.content).unwrap()
    }
}

#[test]
fn every_role_tool_is_provided() {
    // Given / When: the tools are built.
    let fixture = Fixture::new();
    let names: Vec<&str> = fixture.tools.iter().map(|tool| tool.name()).collect();

    // Then: they are exactly the Evaluator's tools.
    assert_eq!(names, agents::EVALUATOR_TOOLS);
}

#[tokio::test]
async fn threads_are_listed_and_inspected_with_their_runs_cost_and_denials() {
    // Given: a recorded thread with a delegated run, a denied command and a priced request.
    let fixture = Fixture::new();
    let root = fixture.thread("thread-1").await;

    // When: threads are listed and the thread is inspected.
    let list = fixture.json("eval_list_threads", json!({})).await;
    let inspected = fixture
        .json("eval_inspect_thread", json!({"thread_id": "thread-1"}))
        .await;

    // Then: the listing shows the request, replayability and the cost of every run.
    let [thread] = list["threads"].as_array().unwrap().as_slice() else {
        panic!("one thread: {list}");
    };
    assert_eq!(thread["thread_id"], "thread-1");
    assert_eq!(thread["first_request"], "fix the bug");
    assert_eq!(thread["replayable"], true);
    assert_eq!(thread["sample"], Value::Null);
    assert_eq!(thread["cost"]["usd"], 2.0);
    assert_eq!(thread["cost"]["complete"], true);

    // Then: the inspection shows what the user saw, the delegation, the denial and the cost.
    assert_eq!(inspected["role"], "Worker");
    let conversation: Vec<(&str, &str)> = inspected["conversation"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["kind"].as_str().unwrap(),
                entry["text"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        conversation,
        [
            ("user", "fix the bug"),
            ("answer", "lib.rs"),
            ("user", "also add a test"),
            ("user", "thanks, ship it"),
            ("assistant", "patched lib.rs"),
        ]
    );
    assert_eq!(inspected["delegations"][0]["parent_run_id"], root);
    assert_eq!(inspected["delegations"][0]["role"], "explorer");
    assert_eq!(inspected["denied_executions"][0]["tool"], "shell");
    assert!(
        inspected["denied_executions"][0]["input"]
            .as_str()
            .unwrap()
            .contains("curl example.com")
    );
    assert_eq!(inspected["metrics"]["tool_calls"]["read"], 1);
    assert_eq!(inspected["metrics"]["denied_requests"], 1);
    assert_eq!(inspected["metrics"]["user_turns"], 4);
    assert_eq!(inspected["cost"]["usd"], 2.0);
    assert!(
        inspected["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().unwrap().contains("delegated to explorer"))
    );
}

#[tokio::test]
async fn projects_filter_the_listing() {
    // Given: a recorded thread in the source repository.
    let fixture = Fixture::new();
    fixture.thread("thread-1").await;

    // When: threads are listed for a sibling directory and for the repository.
    let elsewhere = format!("{}-other", fixture.project().display());
    let other = fixture
        .json("eval_list_threads", json!({"project": elsewhere}))
        .await;
    let same = fixture
        .json(
            "eval_list_threads",
            json!({"project": fixture.project(), "limit": 1}),
        )
        .await;

    // Then: only the repository's listing holds it.
    assert!(other["threads"].as_array().unwrap().is_empty());
    assert_eq!(same["threads"][0]["thread_id"], "thread-1");
}

#[tokio::test]
async fn samples_are_pinned_with_known_roles_and_only_for_replayable_threads() {
    // Given: a replayable thread and one started outside git.
    let fixture = Fixture::new();
    fixture.thread("thread-1").await;
    let outside = fixture.recorded.source.dir.path().join("plain");
    std::fs::create_dir(&outside).unwrap();
    fixture
        .recorded
        .conversation("plain", &outside.canonicalize().unwrap())
        .await;

    // When: samples are saved with an unknown role, for the unreplayable thread,
    // and for the replayable thread with roles named in several ways.
    let unknown = fixture
        .call(
            "eval_save_sample",
            json!({"thread_id": "thread-1", "problem": "slow", "focus_roles": ["Janitor"]}),
        )
        .await;
    let plain = fixture
        .call(
            "eval_save_sample",
            json!({"thread_id": "plain", "problem": "slow"}),
        )
        .await;
    let saved = fixture
        .json(
            "eval_save_sample",
            json!({
                "thread_id": "thread-1",
                "problem": " skipped the failing test ",
                "focus_roles": ["Worker", "roles.planner", "planner", "WebResearcher"],
            }),
        )
        .await;

    // Then: the first two are refused with their reasons and the last is pinned
    // under role addresses.
    assert!(unknown.is_error && unknown.content.contains("Janitor"));
    assert!(plain.is_error && plain.content.contains("not_git_repository"));
    assert_eq!(saved["pinned"], "thread-1");
    let listed = fixture.json("eval_list_threads", json!({})).await;
    let pinned = listed["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["thread_id"] == "thread-1")
        .unwrap();
    assert_eq!(pinned["sample"]["problem"], "skipped the failing test");
    assert_eq!(
        pinned["sample"]["focus_roles"],
        json!(["worker", "roles.planner", "roles.web_researcher"])
    );

    // When: the sample is removed twice.
    let removed = fixture
        .call("eval_remove_sample", json!({"thread_id": "thread-1"}))
        .await;
    let again = fixture
        .call("eval_remove_sample", json!({"thread_id": "thread-1"}))
        .await;

    // Then: the first removal unpins it and the second reports that nothing was pinned.
    assert!(!removed.is_error);
    assert!(again.is_error);
}

fn manifest(finished: bool) -> Manifest {
    Manifest {
        thread_id: "thread-1".into(),
        source_db: "/nowhere/evorch-events.db".into(),
        start: StartSummary {
            project_root: "/project".into(),
            repository: "/project".into(),
            repo_prefix: String::new(),
            head_commit: "abc".into(),
            head_ref: None,
            diff_bytes: 0,
            untracked_entries: 0,
            captured_at_ns: 1,
        },
        original_turns: Vec::new(),
        problem: Some("skipped the failing test".into()),
        verify: None,
        candidate_overlay: None,
        swapped_roles: Vec::new(),
        configurations: vec!["baseline".into(), "candidate".into()],
        repeat: 1,
        simulator: BindingSummary {
            source: "agents.worker".into(),
            binding: RoleBindingConfig::default(),
        },
        budget: EvalBudgetConfig::default(),
        started_at: "2026-10-20T00:00:00.000Z".into(),
        finished_at: finished.then(|| "2026-10-20T00:20:00.000Z".into()),
        runs: vec![RunEntry {
            label: "baseline".into(),
            index: 1,
            dir: "runs/baseline-1".into(),
            nonce: "nonce".into(),
            started_at: "2026-10-20T00:00:00.000Z".into(),
            finished_at: finished.then(|| "2026-10-20T00:10:00.000Z".into()),
            status: None,
        }],
    }
}

fn evaluation_report() -> Report {
    let verdict = |winner, reason: &str| AspectVerdict {
        winner,
        reason: reason.into(),
    };
    Report {
        thread_id: "thread-1".into(),
        problem: Some("skipped the failing test".into()),
        verify: None,
        configurations: vec!["baseline".into(), "candidate".into()],
        candidate_overlay: None,
        swapped_roles: Vec::new(),
        repeat: 1,
        budget: EvalBudgetConfig::default(),
        simulator: BindingSummary {
            source: "agents.worker".into(),
            binding: RoleBindingConfig::default(),
        },
        judge: BindingSummary {
            source: "agents.orchestrator".into(),
            binding: RoleBindingConfig::default(),
        },
        prices: PriceSource {
            catalog_fetched_at: None,
            note: "configured prices".into(),
        },
        runs: Vec::new(),
        costs: vec![crate::report::ConfigurationCost {
            label: "candidate".into(),
            runs: 1,
            cost: Cost {
                usd: 1.5,
                api_equivalent_usd: 1.5,
                unpriced: Vec::new(),
            },
        }],
        overhead: Overhead::default(),
        pairs: vec![PairReport {
            index: 1,
            baseline: "baseline-1".into(),
            candidate: "candidate-1".into(),
            verdicts: vec![AspectResult {
                aspect: "problem".into(),
                winner: Preferred::Candidate,
                baseline_first: verdict(Winner::B, &"ran the test ".repeat(200)),
                candidate_first: verdict(Winner::A, "ran the test"),
            }],
            judging_error: None,
            process: Some(ProcessAnalysis {
                time_and_tokens: "faster".into(),
                detours: "none".into(),
                delegation: "same".into(),
                swapped_roles: "worker".into(),
                difference: "ran the test first".into(),
            }),
            process_error: None,
        }],
        tally: Vec::new(),
        warnings: Vec::new(),
        generated_at: "2026-10-20T00:30:00.000Z".into(),
    }
}

#[tokio::test]
async fn reports_are_listed_summarized_and_read_only_under_the_evaluation_directory() {
    // Given: a reported evaluation, one still running, and a report outside eval/.
    let fixture = Fixture::new();
    let root = fixture.user.path().join("eval");
    let done = root.join("20261020-000000-thread-1");
    let running = root.join("20261021-000000-thread-1");
    let outside = fixture.user.path().join("elsewhere");
    for dir in [&done, &running, &outside] {
        std::fs::create_dir_all(dir).unwrap();
    }
    output::write_json(&done.join(output::MANIFEST_FILE), &manifest(true)).unwrap();
    output::write_json(&done.join(output::REPORT_JSON_FILE), &evaluation_report()).unwrap();
    output::write_json(&running.join(output::MANIFEST_FILE), &manifest(false)).unwrap();
    output::write_json(
        &outside.join(output::REPORT_JSON_FILE),
        &evaluation_report(),
    )
    .unwrap();

    // When: evaluations are listed, then each directory is read.
    let listed = fixture.json("eval_report", json!({})).await;
    let summary = fixture
        .json("eval_report", json!({"dir": "20261020-000000-thread-1"}))
        .await;
    let progress = fixture
        .json("eval_report", json!({"dir": running.display().to_string()}))
        .await;
    let escaped = fixture
        .call("eval_report", json!({"dir": "../elsewhere"}))
        .await;
    let absolute = fixture
        .call("eval_report", json!({"dir": outside.display().to_string()}))
        .await;

    // Then: the listing is newest first with progress.
    assert_eq!(listed["evaluations"][0]["dir"], "20261021-000000-thread-1");
    assert_eq!(listed["evaluations"][0]["report"], false);
    assert_eq!(listed["evaluations"][1]["report"], true);

    // Then: the report is summarized with bounded reasons and cost marks.
    let verdict = &summary["pairs"][0]["verdicts"][0];
    assert_eq!(verdict["winner"], "candidate");
    assert!(
        verdict["baseline_first_reason"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= REASON_CHARS + 1
    );
    assert_eq!(summary["costs"]["candidate"]["api_equivalent"], true);
    assert_eq!(
        summary["pairs"][0]["process"]["difference"],
        "ran the test first"
    );

    // Then: an unfinished evaluation reports its progress and outside paths are refused.
    assert_eq!(progress["report"], Value::Null);
    assert_eq!(progress["progress"]["finished_runs"], 0);
    assert!(escaped.is_error);
    assert!(absolute.is_error);
}
