use std::sync::Mutex;

use config::ProviderTypeConfig;
use event_bus::{Event, ToolEvent};
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

struct Fixture {
    root: tempfile::TempDir,
    judges: Arc<Mutex<Vec<Arc<ScriptedModel>>>>,
}

impl Fixture {
    /// An evaluation directory with one baseline and one candidate run. The baseline
    /// uses a priced API model and hit a DNS failure; the candidate ran on a Claude
    /// subscription, called an unpriced model, and exhausted its budget.
    fn new(problem: Option<&str>, config: &str) -> Self {
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
                role: "worker".into(),
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

        let dns = [
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
            &[],
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
