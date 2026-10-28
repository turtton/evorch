use std::sync::Mutex;

use providers::ContentBlock;

use super::*;
use crate::source::tests::Recorded;
use crate::test_support::{ScriptedModel, Step, decision, text, tool};

struct Fixture {
    recorded: Recorded,
    user: tempfile::TempDir,
    agents: Arc<Mutex<Vec<Arc<ScriptedModel>>>>,
}

impl Fixture {
    async fn new() -> Self {
        let recorded = Recorded::new();
        let project = recorded.source.root().canonicalize().unwrap();
        recorded.conversation("thread-1", &project).await;
        Self {
            recorded,
            user: tempfile::tempdir().unwrap(),
            agents: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn args(&self, candidate: Option<&str>) -> EvalArgs {
        let candidate = candidate.map(|text| {
            let path = self.user.path().join("candidate.toml");
            std::fs::write(&path, text).unwrap();
            path
        });
        EvalArgs {
            thread_id: "thread-1".into(),
            candidate,
            repeat: Some(1),
            output: Some(self.user.path().join("out")),
            db: Some(self.recorded.config.db_path.clone()),
            user_config_dir: Some(self.user.path().to_path_buf()),
            ..EvalArgs::default()
        }
    }

    fn deps(&self, agent: Vec<Step>, user: Vec<Step>) -> RunnerDeps {
        let agents = Arc::clone(&self.agents);
        let credentials =
            sandbox::credential::FileCredentialStore::open(self.user.path().join("credentials"))
                .unwrap();
        RunnerDeps {
            env: Arc::new(routing::MapEnv::default()),
            isolation: Isolation::DirectUnchecked,
            models: Some(FixedModels {
                agent: Arc::new(move |bus| {
                    let model = ScriptedModel::new(agent.clone());
                    model.attach(bus);
                    agents.lock().unwrap().push(Arc::clone(&model));
                    model
                }),
                simulator: Arc::new(move |bus| {
                    let model = ScriptedModel::new(user.clone());
                    model.attach(bus);
                    model
                }),
            }),
            credentials: Some(Arc::new(credentials)),
            catalog: None,
        }
    }

    fn system_prompts(&self) -> Vec<String> {
        self.agents
            .lock()
            .unwrap()
            .iter()
            .map(|model| {
                let requests = model.requests();
                let system = requests[0]
                    .iter()
                    .find(|message| message.role == providers::Role::System)
                    .expect("system prompt");
                system
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .collect()
    }
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn preflight_config(fixture: &Fixture, base_url: String) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "primary".into(),
        config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAiCompatible,
            api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
            base_url,
            default_model: "gpt-4o".into(),
            models: ["gpt-4o", "missing"]
                .into_iter()
                .map(config::types::provider::ModelEntryConfig::enabled)
                .collect(),
            credential: config::CredentialRefConfig::Env { var: "KEY".into() },
            ..Default::default()
        },
    );
    for role in ["worker", "reviewer"] {
        config.routing.routes.insert(
            role.into(),
            vec![config::RouteCandidateConfig {
                profile: "primary".into(),
                model: Some("gpt-4o".into()),
                auto: false,
            }],
        );
    }
    std::fs::write(
        fixture.user.path().join("config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    config
}

#[tokio::test]
async fn preflight_checks_candidate_child_roles_before_starting_any_baseline() {
    let fixture = Fixture::new().await;
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"data": [{"id": "gpt-4o"}]})),
        )
        .expect(3)
        .mount(&server)
        .await;
    preflight_config(&fixture, server.uri());
    // A post-capture project override must not contaminate the snapshot preflight.
    let project_config = fixture.recorded.source.root().join(".evorch");
    std::fs::create_dir_all(&project_config).unwrap();
    std::fs::write(
        project_config.join("config.toml"),
        "[routing.routes]\nworker = [{ profile = \"primary\", model = \"missing\" }]\n",
    )
    .unwrap();
    let mut deps = fixture.deps(vec![], vec![]);
    deps.models = None;
    deps.env = Arc::new(routing::MapEnv::from_iter([("KEY", "fixture-secret")]));
    let args = fixture.args(Some(
        "[routing.routes]\nreviewer = [{ profile = \"primary\", model = \"missing\" }]\n",
    ));
    let output = args.output.clone().unwrap();
    let error = run(args, deps, |_| panic!("no replay may start"))
        .await
        .unwrap_err();
    assert!(matches!(error, EvalError::Preflight { .. }), "{error}");
    let report = read_json(&output.join(preflight::REPORT_FILE));
    assert!(report["checks"][0]["error"].is_null(), "{report}");
    assert!(
        report["checks"][1]["error"]
            .as_str()
            .unwrap()
            .contains("not advertised"),
        "{report}"
    );
    assert_eq!(report["checks"][1]["label"], CANDIDATE);
    assert!(!output.join("runs").exists());
    let manifest = read_json(&output.join(output::MANIFEST_FILE));
    assert_eq!(manifest["runs"], serde_json::json!([]));
    assert!(manifest["finished_at"].is_string());
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method == "GET")
    );
    let reports = crate::report::report(
        crate::ReportArgs {
            dirs: vec![output.clone()],
            user_config_dir: Some(fixture.user.path().into()),
            ..Default::default()
        },
        &crate::ReportDeps {
            env: Arc::new(routing::MapEnv::default()),
            judge: Some(Arc::new(|_| panic!("no failed preflight may be judged"))),
            credentials: None,
            catalog: None,
        },
    )
    .await
    .unwrap();
    assert!(reports[0].pairs.is_empty());
    assert!(reports[0].warnings.iter().any(|warning| warning.kind
        == crate::report::WarningKind::PreflightFailed
        && warning.message.contains("missing")));
    let markdown = std::fs::read_to_string(output.join(output::REPORT_MARKDOWN_FILE)).unwrap();
    assert!(markdown.contains("no samples compared"), "{markdown}");
}

#[tokio::test]
async fn preflight_checks_user_simulator_before_starting_replays() {
    let fixture = Fixture::new().await;
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"data": [{"id": "gpt-4o"}]})),
        )
        .expect(2)
        .mount(&server)
        .await;
    let mut config = preflight_config(&fixture, server.uri());
    config.routing.routes.insert(
        "simulator".into(),
        vec![config::RouteCandidateConfig {
            profile: "primary".into(),
            model: Some("missing".into()),
            auto: false,
        }],
    );
    config.eval.user_simulator = Some(config::RoleBindingConfig {
        models: vec![config::ChainEntryConfig::route("simulator")],
        ..Default::default()
    });
    std::fs::write(
        fixture.user.path().join("config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    let mut deps = fixture.deps(vec![], vec![]);
    deps.models = None;
    deps.env = Arc::new(routing::MapEnv::from_iter([("KEY", "fixture-secret")]));
    let args = fixture.args(None);
    let output = args.output.clone().unwrap();
    let error = run(args, deps, |_| panic!("no replay may start"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("user_simulator"), "{error}");
    let report = read_json(&output.join(preflight::REPORT_FILE));
    assert!(report["checks"][0]["error"].is_null(), "{report}");
    assert!(report["checks"][1]["error"].is_string(), "{report}");
    assert!(!output.join("runs").exists());
}

#[tokio::test]
async fn baseline_and_candidate_replays_each_leave_their_results() {
    // Given: a recorded thread, an agent that writes a file, and a user who then finishes.
    let fixture = Fixture::new().await;
    let overlay = "[sandbox]\nweb_tools_enabled = false\n";
    let deps = fixture.deps(
        vec![
            tool(
                "write",
                serde_json::json!({"path": "notes.txt", "content": "replayed\n"}),
            ),
            text("wrote the notes"),
        ],
        vec![decision("finish", "")],
    );
    let mut reported = Vec::new();

    // When: the evaluation runs once per configuration.
    let evaluation = run(fixture.args(Some(overlay)), deps, |report| {
        reported.push(format!("{}-{}", report.label, report.index));
    })
    .await
    .unwrap();

    // Then: both replays completed, in order, each with its own files and nonce.
    assert_eq!(reported, ["baseline-1", "candidate-1"]);
    let manifest = read_json(&evaluation.dir.join(output::MANIFEST_FILE));
    assert_eq!(manifest["thread_id"], "thread-1");
    assert_eq!(manifest["candidate_overlay"], overlay);
    assert_eq!(
        manifest["configurations"],
        serde_json::json!([BASELINE, CANDIDATE])
    );
    assert!(
        manifest["simulator"]["source"]
            .as_str()
            .unwrap()
            .contains("unset")
    );
    assert!(manifest["finished_at"].is_string());
    let nonces: Vec<&str> = manifest["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["nonce"].as_str().unwrap())
        .collect();
    assert_eq!(nonces.len(), 2);
    assert_ne!(nonces[0], nonces[1]);
    let prompts = fixture.system_prompts();
    for (prompt, nonce) in prompts.iter().zip(&nonces) {
        assert!(prompt.contains(&format!("Run identifier: {nonce}")));
    }
    for report in &evaluation.runs {
        assert_eq!(
            report.outcome.status,
            Status::Completed,
            "{}",
            report.outcome.reason
        );
        let outcome = read_json(&report.dir.join(output::OUTCOME_FILE));
        assert_eq!(outcome["status"], "completed");
        let conversation = read_json(&report.dir.join(output::CONVERSATION_FILE));
        assert_eq!(
            conversation[0],
            serde_json::json!({"kind": "user", "text": "fix the bug"})
        );
        let diff = std::fs::read_to_string(report.dir.join(output::DIFF_FILE)).unwrap();
        assert!(
            diff.contains("b/notes.txt") && diff.contains("+replayed"),
            "{diff}"
        );
        let database =
            storage::Database::open_read_only(&report.dir.join(output::DATABASE_FILE)).unwrap();
        assert!(database.thread_start_state("thread-1").unwrap().is_some());
        let runs = database.persisted_run_ids().unwrap();
        let [root] = runs.as_slice() else {
            panic!("one replay run: {runs:?}");
        };
        let inspected = database.inspect_run(root, false).unwrap();
        assert!(inspected["context"].is_object(), "{inspected}");
        assert!(
            !inspected["usage_requests"].as_array().unwrap().is_empty(),
            "{inspected}"
        );
    }
}

#[tokio::test]
async fn verification_runs_in_each_final_workspace_and_the_manifest_keeps_the_inputs() {
    // Given: an agent that writes a file, a problem statement, a candidate that changes the
    // reviewer, and verification commands that pass only on the replay's final state.
    let fixture = Fixture::new().await;
    let overlay = "[agents.reviewer.generation]\ntemperature = 0.2\n";
    let steps = || {
        vec![
            tool(
                "write",
                serde_json::json!({"path": "notes.txt", "content": "replayed\n"}),
            ),
            text("wrote the notes"),
        ]
    };
    let mut passing = fixture.args(Some(overlay));
    passing.problem = Some("the notes were missing".into());
    passing.verify = Some("grep -q replayed notes.txt".into());
    let mut failing = fixture.args(None);
    failing.output = Some(fixture.user.path().join("failing"));
    failing.verify = Some("test -f missing.txt".into());

    // When: both evaluations run.
    let passed = run(
        passing,
        fixture.deps(steps(), vec![decision("finish", "")]),
        |_| {},
    )
    .await
    .unwrap();
    let failed = run(
        failing,
        fixture.deps(steps(), vec![decision("finish", "")]),
        |_| {},
    )
    .await
    .unwrap();

    // Then: every replay records its verification, and the manifest keeps what the report needs.
    for report in &passed.runs {
        let verification = report.outcome.verification.as_ref().unwrap();
        assert!(verification.passed, "{verification:?}");
        let outcome = read_json(&report.dir.join(output::OUTCOME_FILE));
        assert_eq!(outcome["verification"]["exit_code"], 0);
    }
    let verification = failed.runs[0].outcome.verification.as_ref().unwrap();
    assert!(!verification.passed);
    assert_eq!(verification.exit_code, Some(1));
    let manifest = read_json(&passed.dir.join(output::MANIFEST_FILE));
    assert_eq!(manifest["problem"], "the notes were missing");
    assert_eq!(manifest["verify"], "grep -q replayed notes.txt");
    assert_eq!(manifest["original_turns"][0]["text"], "fix the bug");
    assert_eq!(manifest["swapped_roles"][0]["role"], "reviewer");
    assert_eq!(manifest["swapped_roles"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn unpreparable_workspace_is_recorded_as_prepare_failed() {
    // Given: a recorded thread whose source repository has since lost its history.
    let fixture = Fixture::new().await;
    std::fs::remove_dir_all(fixture.recorded.source.root().join(".git")).unwrap();

    // When: the baseline is replayed.
    let evaluation = run(
        fixture.args(None),
        fixture.deps(Vec::new(), Vec::new()),
        |_| {},
    )
    .await
    .unwrap();

    // Then: the replay never started and its outcome says why.
    let [report] = evaluation.runs.as_slice() else {
        panic!("one replay");
    };
    assert_eq!(report.outcome.status, Status::PrepareFailed);
    assert_eq!(
        read_json(&report.dir.join(output::OUTCOME_FILE))["status"],
        "prepare_failed"
    );
    assert!(!report.dir.join(output::DATABASE_FILE).exists());
    assert!(fixture.agents.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unusable_overlays_and_used_output_directories_are_refused_before_replaying() {
    // Given: an overlay that changes trusted tools, and an output directory already in use.
    let fixture = Fixture::new().await;
    let tools = fixture.args(Some("[tools.browser]\nenabled = true\n"));
    let used = fixture.args(None);
    let output = used.output.clone().unwrap();
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("previous.txt"), "keep").unwrap();

    // When: each evaluation is started.
    let tools = run(tools, fixture.deps(Vec::new(), Vec::new()), |_| {}).await;
    let used = run(used, fixture.deps(Vec::new(), Vec::new()), |_| {}).await;

    // Then: both are refused and nothing was replayed or overwritten.
    assert!(matches!(tools, Err(EvalError::Overlay(_))), "{tools:?}");
    assert!(matches!(used, Err(EvalError::Io(_))), "{used:?}");
    assert!(!output.join("runs").exists());
    assert_eq!(
        std::fs::read_to_string(output.join("previous.txt")).unwrap(),
        "keep"
    );
    assert!(fixture.agents.lock().unwrap().is_empty());
}

#[test]
fn simulator_uses_its_binding_or_reports_the_worker_fallback() {
    // Given: a configuration without and with an `eval.user_simulator` binding.
    let base = Config::default();
    let mut bound = Config::default();
    bound.eval.user_simulator = Some(config::RoleBindingConfig {
        preset: Some("simulator".into()),
        ..config::RoleBindingConfig::default()
    });

    // When: the simulator configuration is derived.
    let (fallback, fallback_summary) = simulator_config(&base);
    let (config, summary) = simulator_config(&bound);

    // Then: the binding takes the worker slot, or the worker binding is named as the source.
    assert_eq!(fallback.agents.worker.base, base.agents.worker.base);
    assert!(fallback_summary.source.contains("unset"));
    assert_eq!(
        config.agents.worker.base.preset.as_deref(),
        Some("simulator")
    );
    assert_eq!(summary.source, "eval.user_simulator");
}
