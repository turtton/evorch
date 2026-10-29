use std::sync::Mutex;

use config::ProviderTypeConfig;
use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, ToolEvent};
use providers::ContentBlock;

use super::*;
use crate::output::{RunEntry, StartSummary};
use crate::roles::ModelRef;
use crate::source::{UserTurn, UserTurnKind};
use crate::test_support::{ScriptedModel, Step, catalog, text, usage, write_run_db};

const CONFIG: &str = r#"version = 3

[providers.claude]
provider_type = "anthropic-subscription"
default_model = "claude-sonnet-9"

[providers.api]
provider_type = "openai"
default_model = "gpt-9"

[providers.other]
provider_type = "openai-compatible"
default_model = "gemini-9"

[routing.routes]
orchestrator = [{ profile = "claude" }]
judge = [{ profile = "other" }]
"#;

const JUDGE_BINDING: &str = "\n[eval.judge]\nmodels = [{ route = \"judge\" }]\n";

fn model(profile: &str, provider_type: ProviderTypeConfig, model: &str) -> ModelRef {
    ModelRef {
        profile: profile.into(),
        provider_type: Some(provider_type),
        model: model.into(),
    }
}

fn verdicts(winners: [&str; 5], problem: bool) -> Step {
    let mut answer = serde_json::Map::new();
    for (aspect, winner) in judge::aspects(problem).into_iter().zip(winners) {
        answer.insert(
            aspect.into(),
            serde_json::json!({"winner": winner, "reason": format!("{aspect} because {winner}")}),
        );
    }
    text(&serde_json::Value::Object(answer).to_string())
}

fn analysis() -> Step {
    text(
        &serde_json::json!({
            "time_and_tokens": "the candidate spent its budget on reads",
            "detours": "none",
            "delegation": "appropriate",
            "swapped_roles": "the worker edited directly",
            "difference": "the swapped worker differs",
        })
        .to_string(),
    )
}

/// Answers of a judge in order: baseline first, candidate first, process analysis.
/// The candidate wins requirement and the problem in both orders, the baseline wins
/// correctness in both, scope is a tie, and efficiency follows the first position.
fn judge_answers() -> Vec<Step> {
    vec![
        verdicts(["B", "A", "tie", "A", "B"], true),
        verdicts(["A", "B", "tie", "A", "A"], true),
        analysis(),
    ]
}

fn started(run: &str, parent: Option<&str>, role: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: role.into(),
        role: role.into(),
    })
}

fn said(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::MessageCompleted {
        run_id: run.into(),
        text: text.into(),
    })
}

fn finished(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::FinalResultPublished {
        run_id: run.into(),
        text: text.into(),
    })
}

fn ended(run: &str, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from: AgentRunPhase::Running,
        to,
        reason: None,
    })
}

/// A root run that delegated to a worker, which finished with `result`, and to
/// an explorer, which only answered in a message.
fn delegation(label: &str, result: &str) -> Vec<Event> {
    vec![
        started("root", None, "orchestrator"),
        started(&format!("{label}-worker"), Some("root"), "worker"),
        started(&format!("{label}-explorer"), Some("root"), "explorer"),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "root".into(),
            text: format!("{label} root text"),
        }),
        Event::new(MessageEvent::MessageCompleted {
            run_id: format!("{label}-explorer"),
            text: format!("{label} explorer found the module"),
        }),
        Event::new(MessageEvent::MessageCompleted {
            run_id: format!("{label}-worker"),
            text: "draft, superseded by finish".into(),
        }),
        Event::new(MessageEvent::FinalResultPublished {
            run_id: format!("{label}-worker"),
            text: result.into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: format!("{label}-worker"),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        }),
    ]
}

/// root (orchestrator) -> worker -> reviewer, where the reviewer never calls
/// `finish` and answered several times, the last answer being blank; and a scout
/// that only produced a blank answer.
fn nested_delegation(label: &str, verdict: &str) -> Vec<Event> {
    let (worker, reviewer, scout) = (
        format!("{label}-worker"),
        format!("{label}-reviewer"),
        format!("{label}-scout"),
    );
    vec![
        started("root", None, "orchestrator"),
        started(&worker, Some("root"), "worker"),
        started(&reviewer, Some(&worker), "reviewer"),
        started(&scout, Some("root"), "explorer"),
        said(&reviewer, "first impression"),
        said(&reviewer, verdict),
        said(&reviewer, "  \n "),
        said(&scout, "   "),
        said(&worker, "worker draft"),
        finished(&worker, &format!("{label} worker report")),
        ended(&reviewer, AgentRunPhase::Stopped),
        ended(&worker, AgentRunPhase::Done),
    ]
}

struct Fixture {
    root: tempfile::TempDir,
    judges: Arc<Mutex<Vec<Arc<ScriptedModel>>>>,
}

impl Fixture {
    /// An evaluation directory with one baseline and one candidate run. The baseline
    /// uses a priced API model and hit a DNS failure; the candidate ran on a Claude
    /// subscription, called an unpriced model, and exhausted its budget.
    fn new(problem: Option<&str>, config: &str) -> Self {
        Self::delegating(problem, config, "worker", delegation)
    }

    /// [`Fixture::new`] swapping `swapped` and with the runs `delegations` gives
    /// for each side's label and final result.
    fn delegating(
        problem: Option<&str>,
        config: &str,
        swapped: &str,
        delegations: fn(&str, &str) -> Vec<Event>,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let fixture = Self {
            root,
            judges: Arc::new(Mutex::new(Vec::new())),
        };
        std::fs::create_dir_all(fixture.user()).unwrap();
        std::fs::write(fixture.user().join("config.toml"), config).unwrap();
        let dir = fixture.dir();
        std::fs::create_dir_all(&dir).unwrap();
        let entry = |label: &str| RunEntry {
            label: label.into(),
            index: 1,
            dir: format!("runs/{label}-1"),
            nonce: format!("{label}-nonce"),
            started_at: "2026-10-20T00:00:00.000Z".into(),
            finished_at: Some("2026-10-20T00:10:00.000Z".into()),
            status: Some(Status::Completed),
        };
        let manifest = Manifest {
            thread_id: "thread-1".into(),
            source_db: "/nowhere/evorch-events.db".into(),
            start: StartSummary {
                project_root: "/project".into(),
                repository: "/project".into(),
                repo_prefix: String::new(),
                head_commit: "abc".into(),
                head_ref: Some("refs/heads/main".into()),
                diff_bytes: 0,
                untracked_entries: 0,
                captured_at_ns: 1,
            },
            original_turns: vec![UserTurn {
                kind: UserTurnKind::InitialPrompt,
                text: "fix the bug".into(),
                question: None,
                reference: None,
            }],
            problem: problem.map(str::to_owned),
            verify: Some("cargo test --offline".into()),
            candidate_overlay: Some("[agents.worker]\nmodels = [{ route = \"claude\" }]\n".into()),
            swapped_roles: vec![SwappedRole {
                role: swapped.into(),
                baseline: vec![model("api", ProviderTypeConfig::OpenAi, "gpt-9")],
                candidate: vec![model(
                    "claude",
                    ProviderTypeConfig::AnthropicSubscription,
                    "claude-sonnet-9",
                )],
            }],
            configurations: vec![BASELINE.into(), CANDIDATE.into()],
            repeat: 1,
            simulator: BindingSummary {
                source: "agents.worker (eval.user_simulator is unset)".into(),
                binding: config::RoleBindingConfig::default(),
            },
            budget: EvalBudgetConfig::default(),
            started_at: "2026-10-20T00:00:00.000Z".into(),
            finished_at: Some("2026-10-20T00:20:00.000Z".into()),
            runs: vec![entry(BASELINE), entry(CANDIDATE)],
        };
        output::write_json(&dir.join(output::MANIFEST_FILE), &manifest).unwrap();

        let mut dns = vec![
            Event::new(ToolEvent::ToolStarted {
                tool_name: "shell".into(),
                call_id: "1".into(),
                input: Some(serde_json::json!({"command": "cargo update"})),
                run_id: Some("root".into()),
                parent_call_id: None,
            }),
            Event::new(ToolEvent::ToolCompleted {
                tool_name: "shell".into(),
                call_id: "1".into(),
                is_error: true,
                output: Some("error: Could not resolve host: index.crates.io".into()),
                detail: None,
                run_id: Some("root".into()),
                parent_call_id: None,
            }),
        ];
        dns.extend(delegations("baseline", "baseline verdict"));
        fixture.run(
            BASELINE,
            Status::Completed,
            Verification {
                command: "cargo test --offline".into(),
                passed: true,
                exit_code: Some(0),
                timed_out: false,
                elapsed_ms: 10,
                stdout_tail: "ok".into(),
                stderr_tail: String::new(),
                error: None,
            },
            &dns,
            vec![
                usage("b1", "api", "gpt-9", "agent", 1_000_000),
                usage("b2", "api", "gpt-9", "user_simulation", 1_000_000),
            ],
        );
        fixture.run(
            CANDIDATE,
            Status::Incomplete,
            Verification {
                command: "cargo test --offline".into(),
                passed: false,
                exit_code: Some(101),
                timed_out: false,
                elapsed_ms: 10,
                stdout_tail: String::new(),
                stderr_tail: "test failed".into(),
                error: None,
            },
            &delegations("candidate", "candidate verdict"),
            vec![
                usage("c1", "claude", "claude-sonnet-9", "agent", 1_000_000),
                usage("c2", "local", "mystery", "agent", 10),
            ],
        );
        fixture
    }

    fn user(&self) -> PathBuf {
        self.root.path().join("user")
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("eval")
    }

    fn run(
        &self,
        label: &str,
        status: Status,
        verification: Verification,
        events: &[Event],
        requests: Vec<storage::usage::UsageRequestRecord>,
    ) {
        let run_dir = self.dir().join(format!("runs/{label}-1"));
        std::fs::create_dir_all(&run_dir).unwrap();
        let outcome = Outcome {
            status,
            reason: format!("{label} ended"),
            user_turns: 1,
            tokens: 0,
            tool_calls: 0,
            elapsed_ms: 60_000,
            denied_requests: Vec::new(),
            pushed_refs: Vec::new(),
            verification: Some(verification),
        };
        let conversation = [
            Entry::User {
                text: "fix the bug".into(),
            },
            Entry::Assistant {
                text: format!("{label} answer"),
            },
        ];
        let diff = format!("diff --git a/src.rs b/src.rs\n+{label}\n");
        output::write_run(&run_dir, &outcome, &conversation, Some(diff.as_bytes())).unwrap();
        write_run_db(&run_dir.join(output::DATABASE_FILE), events, requests);
    }

    fn args(&self, rejudge: bool) -> ReportArgs {
        ReportArgs {
            dirs: vec![self.dir()],
            user_config_dir: Some(self.user()),
            rejudge,
        }
    }

    async fn deps(&self, answers: Vec<Step>) -> ReportDeps {
        let judges = Arc::clone(&self.judges);
        ReportDeps {
            env: Arc::new(routing::MapEnv::default()),
            judge: Some(Arc::new(move |bus| {
                let model = ScriptedModel::new(answers.clone());
                model.attach(bus);
                judges.lock().unwrap().push(Arc::clone(&model));
                model
            })),
            credentials: None,
            catalog: Some(
                catalog(serde_json::json!({
                    "openai": {"models": {"gpt-9": {"id": "gpt-9",
                        "cost": {"input": 1.0, "output": 10.0}}}},
                    "anthropic": {"models": {"claude-sonnet-9": {"id": "claude-sonnet-9",
                        "family": "claude-sonnet", "cost": {"input": 3.0, "output": 15.0}}}},
                }))
                .await,
            ),
        }
    }

    fn judge_requests(&self) -> Vec<serde_json::Value> {
        self.judges
            .lock()
            .unwrap()
            .iter()
            .flat_map(|model| model.requests())
            .map(|messages| {
                let ContentBlock::Text { text } = &messages[1].content[0] else {
                    panic!("text payload");
                };
                serde_json::from_str(text).unwrap()
            })
            .collect()
    }

    fn judgments(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir().join(output::JUDGMENTS_DIR))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

fn kinds(report: &Report) -> Vec<WarningKind> {
    report.warnings.iter().map(|warning| warning.kind).collect()
}

#[tokio::test]
async fn reports_reprice_judge_both_orders_and_write_json_and_markdown() {
    // Given: an evaluation directory with a problem statement and a judge bound to the
    // orchestrator's Claude model, the family of the candidate's swapped worker.
    let fixture = Fixture::new(Some("the worker ignored the failing test"), CONFIG);
    let deps = fixture.deps(judge_answers()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the order-swapped verdicts are aggregated per aspect, a disagreement is a tie.
    let [report] = reports.as_slice() else {
        panic!("one report");
    };
    let tally: Vec<(&str, u32, u32, u32)> = report
        .tally
        .iter()
        .map(|tally| {
            (
                tally.aspect.as_str(),
                tally.candidate_wins,
                tally.baseline_wins,
                tally.ties,
            )
        })
        .collect();
    assert_eq!(
        tally,
        [
            ("requirement", 1, 0, 0),
            ("correctness", 0, 1, 0),
            ("scope", 0, 0, 1),
            ("efficiency", 0, 0, 1),
            ("problem", 1, 0, 0),
        ]
    );
    let process = report.pairs[0].process.as_ref().unwrap();
    assert_eq!(process.difference, "the swapped worker differs");

    // Then: one price table reprices every run; the simulated user is overhead, the
    // subscription is an API equivalent, and the unpriced model makes the cost incomplete.
    let baseline = report.cost_of(BASELINE).unwrap();
    assert!((baseline.usd - 1.0).abs() < 1e-9 && baseline.is_complete());
    let candidate = report.cost_of(CANDIDATE).unwrap();
    assert!((candidate.usd - 3.0).abs() < 1e-9 && candidate.is_api_equivalent());
    assert_eq!(candidate.unpriced[0].model, "mystery");
    assert!((report.overhead.user_simulation.cost.usd - 1.0).abs() < 1e-9);
    assert_eq!(report.overhead.judging.tokens.requests, 3);
    assert!(report.prices.catalog_fetched_at.is_some());
    assert!(report.judge.source.contains("unset"));

    // Then: self-preference, cost incompleteness and the DNS failure are warned about.
    let kinds = kinds(report);
    for kind in [
        WarningKind::SelfPreference,
        WarningKind::CostIncomplete,
        WarningKind::ExternalDependency,
    ] {
        assert!(kinds.contains(&kind), "{kind:?} in {:?}", report.warnings);
    }

    // Then: the judge saw the original request, the problem, each side's evidence and cost,
    // with the candidate first in the second comparison, and the swapped roles in the analysis.
    let requests = fixture.judge_requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0]["original_user_messages"][0]["text"],
        "fix the bug"
    );
    assert_eq!(
        requests[0]["problem"],
        "the worker ignored the failing test"
    );
    assert_eq!(requests[0]["A"]["status"], "completed");
    assert_eq!(requests[0]["A"]["cost"]["usd"], 1.0);
    assert_eq!(requests[0]["B"]["verification"]["passed"], false);
    assert!(
        requests[0]["B"]["diff"]
            .as_str()
            .unwrap()
            .contains("+candidate")
    );
    assert_eq!(requests[1]["A"]["status"], "incomplete");
    assert_eq!(requests[2]["swapped_roles"], serde_json::json!(["worker"]));
    assert!(requests[2]["baseline"]["timeline"].is_array());

    // Then: each side carries the delegated runs' final outputs apart from the root's
    // conversation; the swapped worker is marked as the focus and comes first, with
    // its `finish` result rather than its last message.
    let delegated = &requests[0]["A"]["delegated_outputs"];
    assert_eq!(delegated[0]["role"], "worker");
    assert_eq!(delegated[0]["focus"], true);
    assert_eq!(delegated[0]["delegated_by"], "orchestrator");
    assert_eq!(delegated[0]["phase"], "Done");
    assert_eq!(delegated[0]["final_output"], "baseline verdict");
    assert_eq!(delegated[1]["role"], "explorer");
    assert_eq!(delegated[1]["focus"], false);
    assert_eq!(
        delegated[1]["final_output"],
        "baseline explorer found the module"
    );
    assert_eq!(delegated.as_array().unwrap().len(), 2);
    assert_eq!(
        requests[1]["A"]["delegated_outputs"][0]["final_output"],
        "candidate verdict"
    );
    assert_eq!(
        requests[2]["candidate"]["delegated_outputs"][0]["final_output"],
        "candidate verdict"
    );

    // Then: both files are written, the JSON holds the same report, and judgments are cached.
    let written: Report = read_json(&fixture.dir().join(output::REPORT_JSON_FILE)).unwrap();
    assert_eq!(&written, report);
    let markdown =
        std::fs::read_to_string(fixture.dir().join(output::REPORT_MARKDOWN_FILE)).unwrap();
    for expected in [
        "# Evaluation of thread thread-1",
        "**Problem:** the worker ignored the failing test",
        "Swapped `worker`: gpt-9 → claude-sonnet-9",
        "| baseline-1 | completed |",
        "(API-equivalent) (cost incomplete)",
        "| problem | 1 | 0 | 0 |",
        "- Difference: the swapped worker differs",
        "## Warnings",
    ] {
        assert!(markdown.contains(expected), "{expected}\n{markdown}");
    }
    assert_eq!(
        fixture.judgments(),
        [
            "pair-1-baseline_first.json",
            "pair-1-candidate_first.json",
            "pair-1-process.json"
        ]
    );
}

#[tokio::test]
async fn cached_judgments_are_reused_until_a_rejudge_is_asked_for() {
    // Given: a directory that was already reported once.
    let fixture = Fixture::new(None, CONFIG);
    let answers = vec![
        verdicts(["A", "A", "A", "A", ""], false),
        verdicts(["B", "B", "B", "B", ""], false),
        analysis(),
    ];
    let first = report(fixture.args(false), &fixture.deps(answers.clone()).await)
        .await
        .unwrap();

    // When: it is reported again, and then again with --rejudge.
    let again = report(fixture.args(false), &fixture.deps(answers.clone()).await)
        .await
        .unwrap();
    let judges_after_reuse = fixture.judges.lock().unwrap().len();
    let rejudged = report(fixture.args(true), &fixture.deps(answers).await)
        .await
        .unwrap();

    // Then: the cache answers the second report without a judge, and the rejudge asks anew.
    assert_eq!(judges_after_reuse, 1);
    assert_eq!(again[0].pairs, first[0].pairs);
    assert_eq!(again[0].overhead.judging, first[0].overhead.judging);
    assert_eq!(fixture.judges.lock().unwrap().len(), 2);
    assert_eq!(fixture.judge_requests().len(), 6);
    assert_eq!(rejudged[0].tally, first[0].tally);
    assert_eq!(first[0].tally[0].baseline_wins, 1);
}

#[tokio::test]
async fn judge_failures_are_reported_and_never_cached() {
    // Given: a judge bound through eval.judge to a model of another family that only
    // returns unusable answers.
    let fixture = Fixture::new(None, &format!("{CONFIG}{JUDGE_BINDING}"));
    let deps = fixture.deps(Vec::new()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the report is still written, with the failures, every attempt as overhead,
    // no cached judgment, and no self-preference warning for an unrelated family.
    let report = &reports[0];
    assert!(report.pairs[0].verdicts.is_empty());
    assert!(report.pairs[0].judging_error.is_some());
    assert!(report.pairs[0].process_error.is_some());
    assert_eq!(report.overhead.judging.tokens.requests, 9);
    assert_eq!(report.judge.source, "eval.judge");
    let kinds = kinds(report);
    assert!(kinds.contains(&WarningKind::JudgingFailed));
    assert!(!kinds.contains(&WarningKind::SelfPreference));
    assert!(fixture.judgments().is_empty());
    assert!(fixture.dir().join(output::REPORT_MARKDOWN_FILE).exists());
}

#[tokio::test]
async fn several_threads_are_tabulated_side_by_side_without_a_ranking() {
    // Given: reports of two threads.
    let fixture = Fixture::new(Some("the worker ignored the failing test"), CONFIG);
    let mut reports = report(fixture.args(false), &fixture.deps(judge_answers()).await)
        .await
        .unwrap();
    let mut other = reports[0].clone();
    other.thread_id = "thread-2".into();
    other.pairs.clear();
    reports.push(other);

    // When: the cross-thread table is built.
    let table = cross_thread_table(&reports);

    // Then: each thread has its own counts and costs, and an unjudged thread has none.
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines[0],
        "| thread | requirement W/L/T | correctness W/L/T | scope W/L/T | efficiency W/L/T | problem W/L/T | baseline cost | candidate cost |"
    );
    assert_eq!(
        lines[2],
        "| thread-1 | 1/0/0 | 0/1/0 | 0/0/1 | 0/0/1 | 1/0/0 | $1.0000 | $3.0000 (API-equivalent) (cost incomplete) |"
    );
    assert!(lines[3].starts_with("| thread-2 | - | - | - | - | - |"));
    assert_eq!(lines.len(), 4);
}

fn swapped(role: &str) -> Vec<SwappedRole> {
    vec![SwappedRole {
        role: role.into(),
        baseline: Vec::new(),
        candidate: Vec::new(),
    }]
}

fn record(events: Vec<Event>) -> RunRecord {
    RunRecord {
        requests: Vec::new(),
        events,
    }
}

/// `count` runs of `role` below the root, `<role>1` .. `<role>N`, each answering `text`.
fn runs_of(role: &str, count: usize, text: &str) -> Vec<Event> {
    (1..=count)
        .flat_map(|n| {
            let run = format!("{role}{n}");
            [started(&run, Some("root"), role), said(&run, text)]
        })
        .collect()
}

fn run_ids(entries: &[serde_json::Value]) -> Vec<&str> {
    entries
        .iter()
        .map(|entry| entry["run_id"].as_str().unwrap_or("-"))
        .collect()
}

#[tokio::test]
async fn nested_delegations_reach_both_the_comparison_and_the_analysis() {
    // Given: a reviewer two levels below the root (root -> worker -> reviewer) is the swapped
    // role; it never calls finish, and a scout only produced a blank answer.
    let fixture = Fixture::delegating(None, CONFIG, "reviewer", nested_delegation);
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    report(fixture.args(false), &deps).await.unwrap();

    // Then: the reviewer comes first as the focus, attributed to the worker that delegated
    // to it, with its last non-blank answer; the worker is not a focus and reports its
    // finish result; the scout has no output; and the root text stays in the conversation.
    let requests = fixture.judge_requests();
    assert_eq!(requests.len(), 3);
    let expect = |side: &serde_json::Value, label: &str| {
        let delegated = side["delegated_outputs"].as_array().unwrap();
        assert_eq!(
            run_ids(delegated),
            [
                format!("{label}-reviewer"),
                format!("{label}-worker"),
                format!("{label}-scout")
            ]
        );
        let reviewer = &delegated[0];
        assert_eq!(reviewer["role"], "reviewer");
        assert_eq!(reviewer["focus"], true);
        assert_eq!(reviewer["delegated_by"], "worker");
        assert_eq!(reviewer["phase"], "Stopped");
        assert_eq!(reviewer["final_output"], format!("{label} verdict"));
        let worker = &delegated[1];
        assert_eq!(worker["focus"], false);
        assert_eq!(worker["delegated_by"], "orchestrator");
        assert_eq!(worker["phase"], "Done");
        assert_eq!(worker["final_output"], format!("{label} worker report"));
        let scout = &delegated[2];
        assert_eq!(scout["delegated_by"], "orchestrator");
        assert!(scout["phase"].is_null() && scout["final_output"].is_null());
    };
    expect(&requests[0]["A"], "baseline");
    expect(&requests[0]["B"], "candidate");
    expect(&requests[1]["A"], "candidate");
    expect(&requests[2]["baseline"], "baseline");
    expect(&requests[2]["candidate"], "candidate");
    assert!(
        requests[0]["A"]["conversation"]
            .to_string()
            .contains("baseline answer")
    );
    assert!(
        !requests[0]["A"]["conversation"]
            .to_string()
            .contains("verdict")
    );
}

fn judge_answers_without_problem() -> Vec<Step> {
    vec![
        verdicts(["B", "A", "tie", "A", "B"], false),
        verdicts(["A", "B", "tie", "A", "A"], false),
        analysis(),
    ]
}

#[test]
fn delegated_output_excerpts_are_cut_at_the_limit_on_a_character_boundary() {
    // Given: a focus output (three-byte characters) and another output (four-byte
    // characters), each exactly at its limit, one character over it, and far over it.
    let at = |role: &str, run: &str, chars: usize, unit: &str| {
        [
            started(run, Some("root"), role),
            finished(run, &unit.repeat(chars)),
        ]
    };
    let events: Vec<Event> = [
        at("worker", "focus-at", FOCUS_OUTPUT_CHARS, "あ"),
        at("worker", "focus-over", FOCUS_OUTPUT_CHARS + 1, "あ"),
        at("worker", "focus-far", FOCUS_OUTPUT_CHARS * 3, "あ"),
        at("explorer", "other-at", OTHER_OUTPUT_CHARS, "😀"),
        at("explorer", "other-over", OTHER_OUTPUT_CHARS + 1, "😀"),
        at("explorer", "other-far", OTHER_OUTPUT_CHARS * 3, "😀"),
    ]
    .into_iter()
    .flatten()
    .collect();

    // When: the outputs are bounded for the judge.
    let entries = delegated_outputs(&record(events), &swapped("worker"));

    // Then: an output at the limit is untouched, one over it keeps exactly the limit's
    // characters with a marker that counts them, and nothing is split inside a character.
    let output = |run: &str| {
        entries
            .iter()
            .find(|entry| entry["run_id"] == run)
            .and_then(|entry| entry["final_output"].as_str())
            .unwrap()
    };
    let cut = |run: &str, unit: &str, limit: usize, total: usize| {
        let text = output(run);
        let marker = format!("\n[... truncated: {limit} of {total} characters shown ...]");
        assert_eq!(text, format!("{}{marker}", unit.repeat(limit)), "{run}");
    };
    assert_eq!(output("focus-at"), "あ".repeat(FOCUS_OUTPUT_CHARS));
    cut(
        "focus-over",
        "あ",
        FOCUS_OUTPUT_CHARS,
        FOCUS_OUTPUT_CHARS + 1,
    );
    cut(
        "focus-far",
        "あ",
        FOCUS_OUTPUT_CHARS,
        FOCUS_OUTPUT_CHARS * 3,
    );
    assert_eq!(output("other-at"), "😀".repeat(OTHER_OUTPUT_CHARS));
    cut(
        "other-over",
        "😀",
        OTHER_OUTPUT_CHARS,
        OTHER_OUTPUT_CHARS + 1,
    );
    cut(
        "other-far",
        "😀",
        OTHER_OUTPUT_CHARS,
        OTHER_OUTPUT_CHARS * 3,
    );
    let focus_flags: Vec<_> = entries.iter().map(|entry| entry["focus"].clone()).collect();
    assert_eq!(
        focus_flags,
        [true, true, true, false, false, false].map(serde_json::Value::from)
    );
}

#[test]
fn delegated_outputs_keep_the_newest_runs_of_each_group_and_count_the_rest() {
    // Given: exactly the cap of focus runs and of other runs.
    let at_cap = [
        runs_of("worker", DELEGATED_FOCUS_MAX, "w"),
        runs_of("explorer", DELEGATED_OTHER_MAX, "e"),
    ]
    .concat();

    // When / Then: nothing is omitted and the focus runs come before the others.
    let entries = delegated_outputs(&record(at_cap), &swapped("worker"));
    assert_eq!(entries.len(), DELEGATED_FOCUS_MAX + DELEGATED_OTHER_MAX);
    assert!(entries.iter().all(|entry| entry["kind"].is_null()));
    assert_eq!(run_ids(&entries)[0], "worker1");
    assert_eq!(run_ids(&entries)[DELEGATED_FOCUS_MAX], "explorer1");

    // Given: one focus run and three other runs over the caps.
    let over_cap = [
        runs_of("worker", DELEGATED_FOCUS_MAX + 1, "w"),
        runs_of("explorer", DELEGATED_OTHER_MAX + 3, "e"),
    ]
    .concat();

    // When: they are bounded.
    let entries = delegated_outputs(&record(over_cap), &swapped("worker"));

    // Then: the oldest runs are dropped, each group is followed by its own omitted count,
    // and the focus group still comes first.
    let mut expected: Vec<String> = (2..=DELEGATED_FOCUS_MAX + 1)
        .map(|n| format!("worker{n}"))
        .collect();
    expected.push("-".into());
    expected.extend((4..=DELEGATED_OTHER_MAX + 3).map(|n| format!("explorer{n}")));
    expected.push("-".into());
    assert_eq!(run_ids(&entries), expected);
    let omitted = |index: usize| {
        let entry = &entries[index];
        (
            entry["kind"].clone(),
            entry["focus"].clone(),
            entry["runs"].clone(),
        )
    };
    assert_eq!(
        omitted(DELEGATED_FOCUS_MAX),
        ("omitted".into(), true.into(), 1.into())
    );
    assert_eq!(
        omitted(entries.len() - 1),
        ("omitted".into(), false.into(), 3.into())
    );
}

#[test]
fn a_run_without_a_usable_answer_has_no_output_and_the_root_is_never_listed() {
    // Given: the root answers, a child answers only with whitespace, another child ends
    // with an error without answering, and a third answers twice without calling finish.
    let events = vec![
        started("root", None, "orchestrator"),
        said("root", "root text"),
        started("blank", Some("root"), "explorer"),
        said("blank", " \n\t "),
        started("failed", Some("root"), "worker"),
        ended("failed", AgentRunPhase::Error),
        started("chatty", Some("root"), "reviewer"),
        said("chatty", "first"),
        said("chatty", "second"),
        said("chatty", ""),
    ];

    // When: the delegated outputs are read.
    let entries = delegated_outputs(&record(events), &[]);

    // Then: only the children are listed, none is a focus without swapped roles, blank
    // answers are no output, the phase is kept, and the last non-blank answer wins.
    assert_eq!(run_ids(&entries), ["blank", "failed", "chatty"]);
    assert!(entries.iter().all(|entry| entry["focus"] == false));
    assert!(entries[0]["final_output"].is_null());
    assert!(entries[1]["final_output"].is_null());
    assert_eq!(entries[1]["phase"], "Error");
    assert_eq!(entries[2]["final_output"], "second");
}
