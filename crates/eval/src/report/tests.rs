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
            "affected_roles": "the worker edited directly",
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
        waiting: None,
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
            waiting: None,
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
            observed: None,
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
            sample: None,
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
            common_overlay: None,
            changes: Default::default(),
            pins: None,
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
    let tally: Vec<(&str, u32, u32, u32, u32)> = report
        .tally
        .iter()
        .map(|tally| {
            (
                tally.aspect.as_str(),
                tally.candidate_wins,
                tally.baseline_wins,
                tally.ties,
                tally.order_disagreements,
            )
        })
        .collect();
    assert_eq!(
        tally,
        [
            ("requirement", 1, 0, 0, 0),
            ("correctness", 0, 1, 0, 0),
            ("scope", 0, 0, 1, 0),
            ("efficiency", 0, 0, 0, 1),
            ("problem", 1, 0, 0, 0),
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
    assert_eq!(requests[2]["affected_roles"], serde_json::json!(["worker"]));
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
        "unknown (priced part $3.0000 (API-equivalent))",
        "| problem | 1 | 0 | 0 | 0 |",
        "| efficiency | 0 | 0 | 0 | 1 |",
        "(orders disagreed: no result)",
        "(tie in both orders)",
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
async fn an_unusable_process_analysis_leaves_the_verdicts_and_tally_intact() {
    // Given: both comparisons are usable but the process analysis never is.
    let fixture = Fixture::new(Some("the worker ignored the failing test"), CONFIG);
    let mut answers = judge_answers();
    answers.truncate(2);
    let deps = fixture.deps(answers).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the verdicts and their tally are those of the usable comparisons, the failed
    // analysis is named with the answer that was returned, and only it is left uncached.
    let report = &reports[0];
    assert_eq!(report.pairs[0].verdicts.len(), 5);
    assert!(report.pairs[0].judging_error.is_none());
    assert!(report.pairs[0].process.is_none());
    let error = report.pairs[0].process_error.as_deref().unwrap();
    assert!(error.contains("unusable answer"), "{error}");
    assert!(error.contains("done"), "{error}");
    assert_eq!(report.tally[0].candidate_wins, 1);
    assert_eq!(report.tally[1].baseline_wins, 1);
    assert_eq!(fixture.judgments().len(), 2);
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
        "| thread | requirement W/L/T/D | correctness W/L/T/D | scope W/L/T/D | efficiency W/L/T/D | problem W/L/T/D | baseline cost | candidate cost |"
    );
    assert_eq!(
        lines[2],
        "| thread-1 | 1/0/0/0 | 0/1/0/0 | 0/0/1/0 | 0/0/0/1 | 1/0/0/0 | $1.0000 | unknown (priced part $3.0000 (API-equivalent)) |"
    );
    assert!(lines[3].starts_with("| thread-2 | - | - | - | - | - |"));
    assert_eq!(lines.len(), 4);
}

fn focus(roles: &[String]) -> Focus<'_> {
    Focus {
        roles,
        bindings: &[],
        applied: None,
    }
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
    let entries = delegated_outputs(&record(events), &focus(&["worker".to_owned()]));

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
    let entries = delegated_outputs(&record(at_cap), &focus(&["worker".to_owned()]));
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
    let entries = delegated_outputs(&record(over_cap), &focus(&["worker".to_owned()]));

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
    let entries = delegated_outputs(&record(events), &focus(&[]));

    // Then: only the children are listed, none is a focus without swapped roles, blank
    // answers are no output, the phase is kept, and the last non-blank answer wins.
    assert_eq!(run_ids(&entries), ["blank", "failed", "chatty"]);
    assert!(entries.iter().all(|entry| entry["focus"] == false));
    assert!(entries[0]["final_output"].is_null());
    assert!(entries[1]["final_output"].is_null());
    assert_eq!(entries[1]["phase"], "Error");
    assert_eq!(entries[2]["final_output"], "second");
}

/// The baseline replay finished: its root and explorer, whose end the fixture leaves out,
/// are recorded as done.
fn end_baseline_runs(fixture: &Fixture) {
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: fixture
            .dir()
            .join("runs/baseline-1")
            .join(output::DATABASE_FILE),
        ..storage::StorageConfig::default()
    })
    .unwrap();
    for run in ["root", "baseline-explorer"] {
        storage
            .handle()
            .append_event(Some("eval"), &ended(run, AgentRunPhase::Done))
            .unwrap();
    }
}

fn pin_checks(fixture: &Fixture, checks: Vec<storage::eval_sample::Check>) {
    let path = fixture.dir().join(output::MANIFEST_FILE);
    let mut manifest: Manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let sample = EvalSample {
        checks,
        ..EvalSample::new("thread-1".into(), "the shell failed".into(), Vec::new(), 1)
    };
    manifest.sample = Some(output::PinnedSample::of(sample));
    output::write_json(&path, &manifest).unwrap();
}

fn shell_errors(max: u32) -> storage::eval_sample::Check {
    storage::eval_sample::Check::ToolErrors {
        tool: "shell".into(),
        max,
        scope: storage::eval_sample::CheckScope::Tree,
        calls: storage::eval_sample::CallRange::Direct,
    }
}

#[tokio::test]
async fn a_report_without_checks_says_there_is_no_machine_judgment() {
    // Given: an evaluation of a thread whose sample declared no checks.
    let fixture = Fixture::new(None, CONFIG);
    let deps = fixture.deps(judge_answers()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: neither the report nor its runs claim a machine verdict, and the markdown says so.
    let report = &reports[0];
    assert!(report.checks.is_none());
    assert!(report.runs.iter().all(|run| run.checks.is_none()));
    let markdown =
        std::fs::read_to_string(fixture.dir().join(output::REPORT_MARKDOWN_FILE)).unwrap();
    assert!(markdown.contains("no machine judgment"), "{markdown}");
}

#[tokio::test]
async fn checks_are_graded_per_run_and_the_replay_validity_is_kept_apart() {
    // Given: pinned checks that the baseline's DNS failure breaks (its shell call failed),
    // and a candidate that stopped before it completed.
    let fixture = Fixture::new(None, CONFIG);
    pin_checks(&fixture, vec![shell_errors(0), shell_errors(5)]);
    end_baseline_runs(&fixture);
    let deps = fixture.deps(judge_answers()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the baseline fails its first check and passes the second; the unfinished
    // candidate cannot pass an upper bound by finding no errors.
    let report = &reports[0];
    let baseline = report.runs[0].checks.as_ref().unwrap();
    assert_eq!(
        baseline
            .results
            .iter()
            .map(|result| result.verdict)
            .collect::<Vec<_>>(),
        [checks::Verdict::Fail, checks::Verdict::Pass]
    );
    assert_eq!(baseline.quality(), checks::Quality::Fail);
    let candidate = report.runs[1].checks.as_ref().unwrap();
    assert!(!candidate.validity.valid);
    assert!(
        candidate
            .results
            .iter()
            .all(|result| result.verdict == checks::Verdict::Indeterminate),
        "{candidate:?}"
    );
    assert_ne!(candidate.quality(), checks::Quality::Pass);
    let markdown =
        std::fs::read_to_string(fixture.dir().join(output::REPORT_MARKDOWN_FILE)).unwrap();
    assert!(markdown.contains("## Machine checks"), "{markdown}");
    assert!(markdown.contains("did not complete"), "{markdown}");
}

#[tokio::test]
async fn a_run_whose_recorded_database_is_gone_is_not_a_passing_run() {
    // Given: pinned checks that only an absence of errors satisfies, and a baseline whose
    // database was deleted after the replay.
    let fixture = Fixture::new(None, CONFIG);
    pin_checks(&fixture, vec![shell_errors(5)]);
    std::fs::remove_file(
        fixture
            .dir()
            .join("runs/baseline-1")
            .join(output::DATABASE_FILE),
    )
    .unwrap();
    let deps = fixture.deps(judge_answers()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the empty record is not read as "no errors": the check is indeterminate and
    // the record invalid, so the run has no quality pass.
    let baseline = reports[0].runs[0].checks.as_ref().unwrap();
    assert_eq!(baseline.results[0].verdict, checks::Verdict::Indeterminate);
    assert!(baseline.results[0].reason.contains("evidence is missing"));
    assert!(!baseline.validity.valid);
    assert_ne!(baseline.quality(), checks::Quality::Pass);
}

use crate::changes::ToolDescriptionChange;
use crate::metrics::ModelUse;
use crate::output::{AppliedDescriptions, Observed};

const READ_BASELINE: &str = "hash-of-the-builds-read";
const READ_CANDIDATE: &str = "hash-of-the-new-read";

fn edit_manifest(fixture: &Fixture, edit: impl FnOnce(&mut Manifest)) {
    let path = fixture.dir().join(output::MANIFEST_FILE);
    let mut manifest: Manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut manifest);
    output::write_json(&path, &manifest).unwrap();
}

/// The candidate only rewrites the description of `read` for the worker's model `api/gpt-9`;
/// `observed` is what its replay recorded of what it used.
fn describe_read(fixture: &Fixture, observed: Option<Observed>) {
    describe_tool(fixture, "read", observed);
}

fn describe_tool(fixture: &Fixture, tool: &str, observed: Option<Observed>) {
    edit_manifest(fixture, |manifest| {
        manifest.swapped_roles.clear();
        manifest.changes = Changes {
            bindings: Vec::new(),
            tool_descriptions: vec![ToolDescriptionChange {
                profile: "api".into(),
                route_model_id: "gpt-9".into(),
                tool: tool.into(),
                baseline_hash: READ_BASELINE.into(),
                candidate_hash: READ_CANDIDATE.into(),
            }],
            affected_roles: vec!["worker".into()],
        };
        for entry in &mut manifest.runs {
            entry.observed = if entry.label == CANDIDATE {
                observed.clone()
            } else {
                Some(Observed {
                    models: vec![model_use("api", "gpt-9")],
                    tool_descriptions: vec![frozen("baseline-worker", "worker", "gpt-9", &[])],
                })
            };
        }
    });
}

/// A run's snapshot for `api`/`model` that overrides `tools`, all of which it was offered.
fn frozen(run_id: &str, role: &str, model: &str, tools: &[&str]) -> AppliedDescriptions {
    AppliedDescriptions {
        run_id: run_id.into(),
        role: role.into(),
        profile: Some("api".into()),
        route_model_id: Some(model.into()),
        hash: "snapshot".into(),
        tools: tools.iter().map(|name| (*name).to_owned()).collect(),
        offered: Some(tools.iter().map(|name| (*name).to_owned()).collect()),
    }
}

fn used_by(role: &str, model: &str) -> ModelUse {
    ModelUse {
        role: Some(role.into()),
        ..model_use("api", model)
    }
}

fn model_use(profile: &str, model: &str) -> ModelUse {
    ModelUse {
        role: Some("worker".into()),
        provider: profile.into(),
        profile: Some(profile.into()),
        model: model.into(),
        requests: 2,
    }
}

/// The candidate's worker run froze the changed description of `read` and used only its model.
fn applied_read() -> Observed {
    applied_by("read", "candidate-worker", "worker", &["read", "grep"])
}

/// A run `run_id` of `role` froze the description of `tool` for the described model, and
/// its own record says its model was offered `offered`.
fn applied_by(tool: &str, run_id: &str, role: &str, offered: &[&str]) -> Observed {
    Observed {
        models: vec![model_use("api", "gpt-9")],
        tool_descriptions: vec![AppliedDescriptions {
            run_id: run_id.into(),
            role: role.into(),
            profile: Some("api".into()),
            route_model_id: Some("gpt-9".into()),
            hash: "snapshot".into(),
            tools: vec![tool.into()],
            offered: Some(offered.iter().map(|name| (*name).to_owned()).collect()),
        }],
    }
}

fn pending_kinds(report: &Report) -> Vec<PendingKind> {
    report
        .breakdown
        .validity
        .pending
        .iter()
        .map(|pending| pending.kind)
        .collect()
}

#[tokio::test]
async fn a_description_only_candidate_is_judged_on_its_real_changes_without_naming_the_sides() {
    // Given: a candidate that differs only in a tool description, which its worker used.
    let fixture = Fixture::new(None, CONFIG);
    describe_read(&fixture, Some(applied_read()));
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: both comparisons carry the change by position, each hash under the side that
    // sends it, and no role binding is claimed.
    let requests = fixture.judge_requests();
    assert_eq!(requests.len(), 3);
    let change = |request: usize, side: &str| {
        requests[request]["changes"]["tool_descriptions"][0][format!("description_hash_{side}")]
            .clone()
    };
    assert_eq!(change(0, "A"), READ_BASELINE);
    assert_eq!(change(0, "B"), READ_CANDIDATE);
    assert_eq!(change(1, "A"), READ_CANDIDATE);
    assert_eq!(change(1, "B"), READ_BASELINE);
    for comparison in [&requests[0], &requests[1]] {
        assert_eq!(
            comparison["changes"]["role_bindings"],
            serde_json::json!([])
        );
        assert_eq!(
            comparison["changes"]["affected_roles"],
            serde_json::json!(["worker"])
        );
        // The labels never reach the comparison's view of the changes.
        let shown = comparison["changes"].to_string();
        assert!(
            !shown.contains("baseline") && !shown.contains("candidate"),
            "{shown}"
        );
    }

    // Then: the worker that used the description is the focus on both sides, and the analysis
    // is told the changes, which were used, and that no role was swapped.
    assert_eq!(requests[0]["A"]["delegated_outputs"][0]["focus"], true);
    assert_eq!(requests[0]["B"]["delegated_outputs"][0]["focus"], true);
    assert_eq!(
        requests[2]["changes"]["tool_descriptions"][0]["tool"],
        "read"
    );
    assert_eq!(requests[2]["changes"]["bindings"], serde_json::json!([]));
    assert_eq!(
        requests[2]["applied"]["descriptions"][0]["used_by_the_candidate"],
        true
    );
    assert_eq!(
        requests[2]["applied"]["model_fallbacks"],
        serde_json::json!([])
    );

    // Then: the report records the same changes, and nothing in the description's use holds
    // the judgment back.
    let report = &reports[0];
    assert_eq!(report.changes.tool_descriptions.len(), 1);
    assert!(report.swapped_roles.is_empty());
    let kinds = pending_kinds(report);
    for held in [
        PendingKind::NotApplied,
        PendingKind::ModelFallback,
        PendingKind::EvidenceMissing,
    ] {
        assert!(!kinds.contains(&held), "{held:?} in {kinds:?}");
    }
    let applied = &report.breakdown.effective_changes.tool_descriptions[0];
    assert_eq!(applied.applied_in, ["candidate-1"]);
    assert!(applied.not_applied_in.is_empty());
}

#[tokio::test]
async fn a_description_no_run_used_or_a_model_fallback_holds_the_judgment() {
    // Given: three candidates whose changed description was never frozen, whose worker went
    // to another model, and whose replay recorded nothing.
    let unused = Observed {
        tool_descriptions: Vec::new(),
        ..applied_read()
    };
    let mut fell_back = applied_read();
    fell_back
        .models
        .push(model_use("claude", "claude-sonnet-9"));
    for (observed, expected) in [
        (Some(unused), PendingKind::NotApplied),
        (Some(fell_back), PendingKind::ModelFallback),
        (None, PendingKind::EvidenceMissing),
    ] {
        let fixture = Fixture::new(None, CONFIG);
        describe_read(&fixture, observed.clone());
        let deps = fixture.deps(judge_answers_without_problem()).await;

        // When: the directory is reported.
        let reports = report(fixture.args(false), &deps).await.unwrap();

        // Then: the judgment is held for that reason, and the report warns of it.
        let report = &reports[0];
        assert!(!report.breakdown.validity.valid);
        assert!(
            pending_kinds(report).contains(&expected),
            "{expected:?} in {:?}",
            report.breakdown.validity
        );
        assert!(kinds(report).contains(&WarningKind::ComparisonPending));

        // Then: a description the candidate never used does not make its worker the focus.
        if expected == PendingKind::NotApplied {
            let requests = fixture.judge_requests();
            assert_eq!(requests[0]["A"]["delegated_outputs"][0]["focus"], true);
            assert_eq!(requests[0]["B"]["delegated_outputs"][0]["focus"], false);
        }
    }
}

#[tokio::test]
async fn judgments_made_under_other_instructions_or_changes_are_not_reused() {
    // Given: a directory already judged, whose cached first comparison was made under
    // another identity (an older instruction, payload version or set of changes).
    let fixture = Fixture::new(None, CONFIG);
    let answers = judge_answers_without_problem();
    report(fixture.args(false), &fixture.deps(answers.clone()).await)
        .await
        .unwrap();
    let cached = fixture
        .dir()
        .join(output::JUDGMENTS_DIR)
        .join("pair-1-baseline_first.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cached).unwrap()).unwrap();
    assert!(
        value["identity"]
            .as_str()
            .is_some_and(|identity| !identity.is_empty())
    );
    value["identity"] = "an-older-version".into();
    std::fs::write(&cached, serde_json::to_vec(&value).unwrap()).unwrap();
    let judges_before = fixture.judges.lock().unwrap().len();

    // When: it is reported again.
    let again = report(
        fixture.args(false),
        &fixture.deps(vec![answers[0].clone()]).await,
    )
    .await
    .unwrap();

    // Then: only the stale judgment is asked again, once, and the others are reused.
    assert_eq!(fixture.judges.lock().unwrap().len(), judges_before + 1);
    let newest = fixture.judges.lock().unwrap().last().unwrap().requests();
    assert_eq!(newest.len(), 1);
    assert!(again[0].pairs[0].judging_error.is_none());
    assert_eq!(again[0].pairs[0].verdicts.len(), 4);
}

#[tokio::test]
async fn the_report_splits_context_usage_quality_changes_cost_and_validity() {
    // Given: an evaluation whose baseline runtime recorded its initial context, whose
    // candidate recorded none, and whose candidate ran an unpriced model.
    let fixture = Fixture::new(None, CONFIG);
    let recorded = output::BaseContextSummary {
        role: "Worker".into(),
        category: Some("conversation".into()),
        selected_model: "api/gpt-9".into(),
        system_bytes: 4_000,
        system_tokens: 1_000,
        tools_bytes: 8_000,
        tools_tokens: 2_000,
        tool_count: 1,
        tools: vec![output::ToolSize {
            name: "read".into(),
            description_bytes: 100,
            tokens: 25,
        }],
    };
    std::fs::write(
        fixture
            .dir()
            .join("runs/baseline-1")
            .join(output::BASE_CONTEXT_FILE),
        serde_json::to_vec(&recorded).unwrap(),
    )
    .unwrap();
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the initial context is the estimate the runtime recorded, and unknown where none
    // was.
    let breakdown = &reports[0].breakdown;
    assert_eq!(
        breakdown.initial_context[0].context.as_ref(),
        Some(&recorded)
    );
    assert!(breakdown.initial_context[0].note.contains("estimated"));
    assert!(breakdown.initial_context[1].context.is_none());

    // Then: the first request and the cumulative usage are separate measurements.
    let first = breakdown.first_request[0].usage.as_ref().unwrap();
    assert_eq!(first.input_tokens, 1_000_000);
    assert_eq!(breakdown.cumulative[0].requests, 1);
    assert_eq!(breakdown.cumulative[0].input_tokens, 1_000_000);
    assert!(breakdown.cumulative[0].complete);

    // Then: cost is split by payer; the candidate's unpriced model makes its cost and the
    // total unknown rather than the priced part.
    let agent = |label: &str| {
        &breakdown
            .cost
            .agent
            .iter()
            .find(|agent| agent.label == label)
            .unwrap()
            .cost
    };
    assert!((agent(BASELINE).usd.unwrap() - 1.0).abs() < 1e-9);
    assert_eq!(agent(CANDIDATE).usd, None);
    assert!(
        agent(CANDIDATE)
            .unknown
            .as_deref()
            .unwrap()
            .contains("mystery")
    );
    assert!((agent(CANDIDATE).priced_usd - 3.0).abs() < 1e-9);
    assert!((breakdown.cost.simulator.usd.unwrap() - 1.0).abs() < 1e-9);
    assert_eq!(breakdown.cost.judge.requests, 3);
    assert_eq!(breakdown.cost.total.usd, None);

    // Then: the candidate did not complete, and the judge's efficiency orders disagreed;
    // both hold the judgment, and neither is counted as a tie.
    let kinds = pending_kinds(&reports[0]);
    assert!(kinds.contains(&PendingKind::ReplayIncomplete), "{kinds:?}");
    assert!(
        kinds.contains(&PendingKind::JudgeOrderDisagreement),
        "{kinds:?}"
    );
    let efficiency = breakdown
        .quality
        .aspects
        .iter()
        .find(|aspect| aspect.aspect == "efficiency")
        .unwrap();
    assert_eq!((efficiency.ties, efficiency.order_disagreements), (0, 1));

    // Then: the markdown shows each part under its own heading, with unknown spelled out.
    let markdown =
        std::fs::read_to_string(fixture.dir().join(output::REPORT_MARKDOWN_FILE)).unwrap();
    for expected in [
        "## Initial context (estimate)",
        "## First request (measured)",
        "## Cumulative usage (measured)",
        "## Cost by payer",
        "## Validity",
        "| candidate-1 | unknown | unknown |",
        "unknown (priced part $3.0000",
        "**Judgment pending.**",
    ] {
        assert!(markdown.contains(expected), "{expected}\n{markdown}");
    }
}

#[tokio::test]
async fn a_first_request_without_reported_usage_is_unknown_and_never_zero() {
    // Given: a candidate whose only request reported no usage, and a baseline run without a
    // database at all.
    let fixture = Fixture::new(None, CONFIG);
    let mut silent = usage("c1", "claude", "claude-sonnet-9", "agent", 0);
    silent.usage_reported = false;
    std::fs::remove_file(
        fixture
            .dir()
            .join("runs/candidate-1")
            .join(output::DATABASE_FILE),
    )
    .unwrap();
    write_run_db(
        &fixture
            .dir()
            .join("runs/candidate-1")
            .join(output::DATABASE_FILE),
        &delegation("candidate", "candidate verdict"),
        vec![silent],
    );
    std::fs::remove_file(
        fixture
            .dir()
            .join("runs/baseline-1")
            .join(output::DATABASE_FILE),
    )
    .unwrap();
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the first request, the cumulative sums and the costs say unknown, with the reason.
    let breakdown = &reports[0].breakdown;
    for first in &breakdown.first_request {
        assert!(
            first.usage.is_none() && first.unknown.is_some(),
            "{first:?}"
        );
    }
    assert!(breakdown.cumulative.iter().all(|usage| !usage.complete));
    for labeled in &breakdown.cost.agent {
        assert_eq!(labeled.cost.usd, None, "{labeled:?}");
    }
    assert!(
        breakdown.cost.agent[0]
            .cost
            .unknown
            .as_deref()
            .unwrap()
            .contains("baseline-1")
    );
    let kinds = pending_kinds(&reports[0]);
    assert!(kinds.contains(&PendingKind::EvidenceMissing), "{kinds:?}");
}

#[tokio::test]
async fn a_description_frozen_only_by_a_role_that_cannot_see_the_tool_is_not_applied() {
    // Given: a changed `write` description. The orchestrator uses the same model, so its
    // snapshot lists `write`, but the orchestrator is never offered it and no worker ran.
    let fixture = Fixture::new(None, CONFIG);
    describe_tool(
        &fixture,
        "write",
        Some(applied_by(
            "write",
            "candidate-root",
            "Orchestrator",
            &["delegate", "read"],
        )),
    );
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the change is not in force, the orchestrator's snapshot is no evidence for it, and
    // the judgment is held.
    let first = &reports[0];
    assert!(pending_kinds(first).contains(&PendingKind::NotApplied));
    let effective = &first.breakdown.effective_changes.tool_descriptions[0];
    assert_eq!(effective.not_applied_in, ["candidate-1"]);
    assert!(effective.applied_in.is_empty());
    let requests = fixture.judge_requests();
    assert_eq!(requests[0]["B"]["delegated_outputs"][0]["focus"], false);

    // When: the worker, which is offered `write`, froze it.
    let fixture = Fixture::new(None, CONFIG);
    describe_tool(
        &fixture,
        "write",
        Some(applied_by(
            "write",
            "candidate-worker",
            "Worker",
            &["read", "write"],
        )),
    );
    let deps = fixture.deps(judge_answers_without_problem()).await;
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: it is in force.
    assert!(!pending_kinds(&reports[0]).contains(&PendingKind::NotApplied));
    assert_eq!(
        reports[0].breakdown.effective_changes.tool_descriptions[0].applied_in,
        ["candidate-1"]
    );
}

#[tokio::test]
async fn a_baseline_that_went_to_another_model_holds_the_judgment_too() {
    // Given: a description change the candidate used, and a baseline whose worker also
    // sent requests to another model.
    let fixture = Fixture::new(None, CONFIG);
    describe_read(&fixture, Some(applied_read()));
    edit_manifest(&fixture, |manifest| {
        let baseline = manifest
            .runs
            .iter_mut()
            .find(|entry| entry.label == BASELINE)
            .unwrap();
        baseline
            .observed
            .as_mut()
            .unwrap()
            .models
            .push(model_use("claude", "claude-sonnet-9"));
    });
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the fallback is named against the baseline replay.
    let held: Vec<&Pending> = reports[0]
        .breakdown
        .validity
        .pending
        .iter()
        .filter(|pending| pending.kind == PendingKind::ModelFallback)
        .collect();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].subject, "baseline-1");
}

#[tokio::test]
async fn replays_never_started_unfinished_or_refused_by_preflight_hold_the_judgment() {
    // Given: an evaluation of two repetitions of which each configuration recorded only the
    // first, the candidate's without an end, and a preflight that refused it.
    let fixture = Fixture::new(None, CONFIG);
    edit_manifest(&fixture, |manifest| {
        manifest.repeat = 2;
        for entry in &mut manifest.runs {
            if entry.label == CANDIDATE {
                entry.status = None;
                entry.finished_at = None;
            }
        }
    });
    std::fs::write(
        fixture.dir().join(runner::preflight::REPORT_FILE),
        r#"{"skipped_fixed_models":false,"checks":[{"label":"candidate","bindings":[],"models":[],"error":"model gpt-9 is unavailable","diagnostics":[]}]}"#,
    )
    .unwrap();
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the missing and unfinished replays and the refusal each hold the judgment.
    let held: Vec<(PendingKind, &str)> = reports[0]
        .breakdown
        .validity
        .pending
        .iter()
        .filter(|pending| {
            matches!(
                pending.kind,
                PendingKind::ReplayMissing | PendingKind::PreflightFailed
            )
        })
        .map(|pending| (pending.kind, pending.subject.as_str()))
        .collect();
    assert!(
        held.contains(&(PendingKind::ReplayMissing, "baseline")),
        "{held:?}"
    );
    assert!(
        held.contains(&(PendingKind::ReplayMissing, "candidate")),
        "{held:?}"
    );
    assert!(
        held.contains(&(PendingKind::ReplayMissing, "candidate-1")),
        "{held:?}"
    );
    assert!(
        held.contains(&(PendingKind::PreflightFailed, "preflight")),
        "{held:?}"
    );
    assert!(!reports[0].breakdown.validity.valid);
}

#[tokio::test]
async fn a_missing_database_makes_every_cost_view_unknown_and_not_zero() {
    // Given: an evaluation whose baseline database was deleted after the replay.
    let fixture = Fixture::new(None, CONFIG);
    std::fs::remove_file(
        fixture
            .dir()
            .join("runs/baseline-1")
            .join(output::DATABASE_FILE),
    )
    .unwrap();
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the run's cost, its configuration's and the simulator's are incomplete, the
    // text and the summary say unknown, and the judge was told the same.
    let report = &reports[0];
    assert!(!report.runs[0].cost.is_complete());
    assert!(!report.cost_of(BASELINE).unwrap().is_complete());
    assert!(!report.overhead.user_simulation.cost.is_complete());
    assert!(usd(&report.runs[0].cost).starts_with("unknown (priced part $0.0000"));
    let value = cost_value(&report.runs[0].cost);
    assert!(value["usd"].is_null(), "{value}");
    assert_eq!(
        value["unrecorded_replays"],
        serde_json::json!(["baseline-1"])
    );
    let markdown =
        std::fs::read_to_string(fixture.dir().join(output::REPORT_MARKDOWN_FILE)).unwrap();
    assert!(
        markdown.contains("unknown (priced part $0.0000)"),
        "{markdown}"
    );
    assert!(!markdown.contains("| $0.0000 |"), "{markdown}");
    assert!(report.warnings.iter().any(|warning| {
        warning.kind == WarningKind::CostIncomplete && warning.message.contains("baseline-1")
    }));
    let requests = fixture.judge_requests();
    assert!(requests[0]["A"]["cost"]["usd"].is_null());
}

#[tokio::test]
async fn a_failed_first_attempt_is_unknown_and_a_later_success_is_kept_apart() {
    // Given: a candidate whose first request failed and whose retry succeeded.
    let fixture = Fixture::new(None, CONFIG);
    let mut failed = usage("c0", "claude", "claude-sonnet-9", "agent", 0);
    failed.status = storage::usage::UsageStatus::Failed;
    failed.at_ns = 1;
    let mut retry = usage("c1", "claude", "claude-sonnet-9", "agent", 777);
    retry.at_ns = 2;
    std::fs::remove_file(
        fixture
            .dir()
            .join("runs/candidate-1")
            .join(output::DATABASE_FILE),
    )
    .unwrap();
    write_run_db(
        &fixture
            .dir()
            .join("runs/candidate-1")
            .join(output::DATABASE_FILE),
        &delegation("candidate", "candidate verdict"),
        vec![failed, retry],
    );
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the first request is unknown, and the retry is a separate figure.
    let first = &reports[0].breakdown.first_request[1];
    assert_eq!(first.run, "candidate-1");
    assert!(first.usage.is_none());
    assert!(first.unknown.as_deref().unwrap().contains("failed"));
    assert_eq!(first.first_success.as_ref().unwrap().input_tokens, 777);
    let markdown =
        std::fs::read_to_string(fixture.dir().join(output::REPORT_MARKDOWN_FILE)).unwrap();
    assert!(markdown.contains("a retry"), "{markdown}");
}

#[tokio::test]
async fn the_machine_checks_reach_the_judge_under_each_position_without_naming_the_sides() {
    // Given: pinned checks that fail in the baseline and cannot be decided in the
    // unfinished candidate.
    let fixture = Fixture::new(None, CONFIG);
    pin_checks(&fixture, vec![shell_errors(0), shell_errors(5)]);
    end_baseline_runs(&fixture);
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    report(fixture.args(false), &deps).await.unwrap();

    // Then: each position carries the checks of the replay shown there, the first
    // comparison with the baseline as A and the second with it as B.
    let requests = fixture.judge_requests();
    let verdict = |request: usize, side: &str| {
        requests[request][side]["checks"]["results"][0]["verdict"].clone()
    };
    assert_eq!(verdict(0, "A"), "fail");
    assert_eq!(verdict(0, "B"), "indeterminate");
    assert_eq!(verdict(1, "A"), "indeterminate");
    assert_eq!(verdict(1, "B"), "fail");
    let baseline = &requests[0]["A"]["checks"];
    assert!(
        baseline["results"][0]["check"]
            .as_str()
            .unwrap()
            .contains("shell")
    );
    assert_eq!(baseline["replay_record_complete"], true);
    assert_eq!(requests[0]["B"]["checks"]["replay_record_complete"], false);
    for request in [0, 1] {
        for side in ["A", "B"] {
            let shown = requests[request][side]["checks"].to_string();
            assert!(
                !shown.contains("baseline") && !shown.contains("candidate"),
                "{shown}"
            );
        }
    }

    // Then: the analysis, which names the sides, gets both.
    assert_eq!(requests[2]["baseline"]["checks"]["quality"], "fail");
    assert_ne!(requests[2]["candidate"]["checks"]["quality"], "pass");
}

#[tokio::test]
async fn a_thread_without_checks_tells_the_judge_so_and_other_checks_are_judged_afresh() {
    // Given: an evaluation judged without any checks.
    let fixture = Fixture::new(None, CONFIG);
    let answers = judge_answers_without_problem();
    report(fixture.args(false), &fixture.deps(answers.clone()).await)
        .await
        .unwrap();
    let first = fixture.judge_requests();
    assert!(first[0]["A"]["checks"].is_null() && first[0]["B"]["checks"].is_null());
    let judges = fixture.judges.lock().unwrap().len();

    // When: checks are pinned to its manifest and it is reported again.
    pin_checks(&fixture, vec![shell_errors(0)]);
    report(fixture.args(false), &fixture.deps(answers).await)
        .await
        .unwrap();

    // Then: none of the judgments made without them is reused.
    assert_eq!(fixture.judges.lock().unwrap().len(), judges + 1);
    let newest = fixture.judges.lock().unwrap().last().unwrap().requests();
    assert_eq!(newest.len(), 3);
}

#[tokio::test]
async fn a_description_of_a_tool_a_child_worker_was_never_offered_is_not_applied() {
    // Given: a changed `handoff_to_lane` description. A worker is the only run that started,
    // and it is a child: its snapshot lists the tool, but its own record of the tools
    // offered to the model leaves it out.
    let fixture = Fixture::new(None, CONFIG);
    describe_tool(
        &fixture,
        "handoff_to_lane",
        Some(applied_by(
            "handoff_to_lane",
            "candidate-child-worker",
            "Worker",
            &["read", "write", "send"],
        )),
    );
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the description is not in force, however permissive the role's static catalog is.
    assert!(pending_kinds(&reports[0]).contains(&PendingKind::NotApplied));
    assert!(
        reports[0].breakdown.effective_changes.tool_descriptions[0]
            .applied_in
            .is_empty()
    );

    // When: the record does not say what was offered at all.
    let fixture = Fixture::new(None, CONFIG);
    let mut unknown = applied_by("read", "candidate-worker", "Worker", &[]);
    unknown.tool_descriptions[0].offered = None;
    describe_read(&fixture, Some(unknown));
    let deps = fixture.deps(judge_answers_without_problem()).await;
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: it is not assumed offered either.
    assert!(pending_kinds(&reports[0]).contains(&PendingKind::NotApplied));
}

#[tokio::test]
async fn a_replay_started_and_not_ended_makes_every_cost_view_unknown_apart_from_one_never_started()
{
    // Given: two repetitions of which the candidate's first was started and never ended,
    // and whose second was never started at all.
    let fixture = Fixture::new(None, CONFIG);
    edit_manifest(&fixture, |manifest| {
        manifest.repeat = 2;
        for entry in &mut manifest.runs {
            if entry.label == CANDIDATE {
                entry.status = None;
                entry.finished_at = None;
            }
        }
    });
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the unfinished replay is named, not summed as nothing, in the candidate's cost,
    // the simulator's and the markdown; the baseline's own cost is still known.
    let report = &reports[0];
    assert_eq!(report.unfinished, ["candidate-1"]);
    let candidate = report.cost_of(CANDIDATE).unwrap();
    assert!(!candidate.is_complete());
    assert_eq!(candidate.unfinished, ["candidate-1"]);
    assert!(candidate.unrecorded.is_empty());
    assert!(report.cost_of(BASELINE).unwrap().is_complete());
    assert!(!report.overhead.user_simulation.cost.is_complete());
    let value = cost_value(candidate);
    assert!(value["usd"].is_null(), "{value}");
    assert_eq!(
        value["unfinished_replays"],
        serde_json::json!(["candidate-1"])
    );
    assert!(report.breakdown.cost.agent[1].cost.usd.is_none());
    assert!(report.breakdown.cost.simulator.usd.is_none());
    assert!(report.breakdown.cost.total.usd.is_none());
    let markdown =
        std::fs::read_to_string(fixture.dir().join(output::REPORT_MARKDOWN_FILE)).unwrap();
    assert!(
        markdown.contains("| candidate-1 | not finished |") && markdown.contains("no recorded end"),
        "{markdown}"
    );
    assert!(report.warnings.iter().any(|warning| {
        warning.kind == WarningKind::CostIncomplete && warning.message.contains("candidate-1")
    }));
    // The replay that was never started is a missing replay, not an unknown cost.
    assert!(
        !candidate
            .unfinished
            .iter()
            .any(|name| name == "candidate-2")
    );
    assert!(
        report
            .breakdown
            .validity
            .pending
            .iter()
            .any(|pending| pending.kind == PendingKind::ReplayMissing
                && pending.subject == "candidate")
    );
}

#[tokio::test]
async fn a_description_the_models_edit_interface_did_not_send_is_not_applied_in_the_report() {
    // Given: a worker's snapshot that lists `write` and `edit`; the replay's observation
    // kept only the tools whose description its model's edit interface sends.
    for (tool, offered, applied) in [
        // apply-patch model: edit and write are narrowed away before the request.
        ("write", vec!["read"], false),
        ("edit", vec!["read"], false),
        // edit model: its own tool arrives.
        ("edit", vec!["read", "edit", "write"], true),
    ] {
        let fixture = Fixture::new(None, CONFIG);
        describe_tool(
            &fixture,
            tool,
            Some(applied_by(tool, "candidate-worker", "Worker", &offered)),
        );
        let deps = fixture.deps(judge_answers_without_problem()).await;

        // When: the directory is reported.
        let reports = report(fixture.args(false), &deps).await.unwrap();

        // Then: the change is in force only when the description could reach the wire.
        assert_eq!(
            !pending_kinds(&reports[0]).contains(&PendingKind::NotApplied),
            applied,
            "{tool} with {offered:?}"
        );
    }
}

/// Both the worker's model `api/a` and the reviewer's `api/b` have a changed `read`.
fn describe_read_for_two_models(fixture: &Fixture, baseline: Observed, candidate: Observed) {
    edit_manifest(fixture, |manifest| {
        manifest.swapped_roles.clear();
        manifest.changes = Changes {
            bindings: Vec::new(),
            tool_descriptions: ["a", "b"]
                .into_iter()
                .map(|model| ToolDescriptionChange {
                    profile: "api".into(),
                    route_model_id: model.into(),
                    tool: "read".into(),
                    baseline_hash: READ_BASELINE.into(),
                    candidate_hash: READ_CANDIDATE.into(),
                })
                .collect(),
            affected_roles: vec!["reviewer".into(), "worker".into()],
        };
        for entry in &mut manifest.runs {
            entry.observed = Some(if entry.label == CANDIDATE {
                candidate.clone()
            } else {
                baseline.clone()
            });
        }
    });
}

fn two_model_replay(prefix: &str, worker_sent_to: &str) -> Observed {
    Observed {
        models: vec![used_by("worker", worker_sent_to), used_by("reviewer", "b")],
        tool_descriptions: vec![
            frozen(&format!("{prefix}-worker"), "worker", "a", &["read"]),
            frozen(&format!("{prefix}-reviewer"), "reviewer", "b", &["read"]),
        ],
    }
}

#[tokio::test]
async fn a_fallback_to_another_described_model_is_told_from_each_roles_own_fixed_model() {
    // Given: the worker is fixed to `a` and the reviewer to `b`, and both models' descriptions
    // changed. In both replays the worker fell back to `b`, which is also described.
    let fixture = Fixture::new(None, CONFIG);
    describe_read_for_two_models(
        &fixture,
        two_model_replay("baseline", "b"),
        two_model_replay("candidate", "b"),
    );
    let deps = fixture.deps(judge_answers_without_problem()).await;

    // When: the directory is reported.
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: the worker's fallback is found in the baseline and in the candidate, and the
    // reviewer, which stayed on its own model, is not accused.
    let held: Vec<(&str, &str)> = reports[0]
        .breakdown
        .validity
        .pending
        .iter()
        .filter(|pending| pending.kind == PendingKind::ModelFallback)
        .map(|pending| (pending.subject.as_str(), pending.reason.as_str()))
        .collect();
    assert_eq!(held.len(), 2, "{held:?}");
    for (subject, reason) in &held {
        assert!(["baseline-1", "candidate-1"].contains(subject), "{subject}");
        assert!(
            reason.contains("worker is fixed to api/a") && reason.contains("api/b"),
            "{reason}"
        );
    }
    assert!(!reports[0].breakdown.validity.valid);

    // When: each role stays on its own model.
    let fixture = Fixture::new(None, CONFIG);
    describe_read_for_two_models(
        &fixture,
        two_model_replay("baseline", "a"),
        two_model_replay("candidate", "a"),
    );
    let deps = fixture.deps(judge_answers_without_problem()).await;
    let reports = report(fixture.args(false), &deps).await.unwrap();

    // Then: nothing is held for a fallback.
    assert!(!pending_kinds(&reports[0]).contains(&PendingKind::ModelFallback));
}

#[tokio::test]
async fn a_role_whose_runs_do_not_fix_one_model_cannot_be_told_from_a_fallback() {
    // Given: two worker runs fixed to different described models, and a worker request that
    // went to one of them; a replay in which the reviewer left no snapshot at all.
    let mut two_workers = two_model_replay("candidate", "a");
    two_workers
        .tool_descriptions
        .push(frozen("candidate-worker-2", "worker", "b", &["read"]));
    let mut unrecorded_reviewer = two_model_replay("candidate", "a");
    unrecorded_reviewer.tool_descriptions.truncate(1);
    for candidate in [two_workers, unrecorded_reviewer] {
        let fixture = Fixture::new(None, CONFIG);
        describe_read_for_two_models(&fixture, two_model_replay("baseline", "a"), candidate);
        let deps = fixture.deps(judge_answers_without_problem()).await;

        // When: the directory is reported.
        let reports = report(fixture.args(false), &deps).await.unwrap();

        // Then: the candidate's judgment is held, since no unique fixed model decides it.
        let held: Vec<&str> = reports[0]
            .breakdown
            .validity
            .pending
            .iter()
            .filter(|pending| pending.kind == PendingKind::ModelFallback)
            .map(|pending| pending.subject.as_str())
            .collect();
        assert_eq!(held, ["candidate-1"]);
    }
}
