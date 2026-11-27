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
            cancel: None,
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
    let mut config = preflight_config(&fixture, server.uri());
    // A post-capture project override must not contaminate the snapshot preflight.
    let mut late = config.role_profile_config(None).unwrap();
    late.routing.routes.get_mut("worker").unwrap()[0].model = Some("missing".into());
    config.role_profiles.insert("post_capture".into(), late);
    std::fs::write(
        fixture.user.path().join("config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    let project_config = fixture.recorded.source.root().join(".evorch");
    std::fs::create_dir_all(&project_config).unwrap();
    std::fs::write(
        project_config.join("config.toml"),
        "role_profile = 'post_capture'\n",
    )
    .unwrap();
    let live = load_config(
        Some(fixture.user.path()),
        Some(&fixture.recorded.source.root()),
        None,
    )
    .unwrap();
    assert_eq!(roles::role_models(&live, "worker")[0].model, "missing");
    let mut deps = fixture.deps(vec![], vec![]);
    deps.models = None;
    deps.env = Arc::new(routing::MapEnv::from_iter([("KEY", "fixture-secret")]));
    let args = fixture.args(Some(
        "[routing.routes]\nreviewer = [{ profile = \"primary\", model = \"missing\" }]\n\
         [role_profiles.post_capture.routing.routes]\nreviewer = [{ profile = \"primary\", model = \"missing-live\" }]\n",
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
    assert!(!output.join("preflight-workspace").exists());
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

fn conversation_only_config(fixture: &Fixture, base_url: String) -> Config {
    let mut config = preflight_config(fixture, base_url);
    let chat = config.routing.routes.remove("worker").unwrap();
    config.routing.routes.insert("chat".into(), chat);
    config.agents.worker.categories.insert(
        CategoryId::Conversation.to_string(),
        config::CategoryBindingConfig {
            models: vec![config::ChainEntryConfig::route("chat")],
            ..Default::default()
        },
    );
    config
}

#[tokio::test]
async fn preflight_simulator_base_cannot_be_masked_by_conversation_category() {
    let fixture = Fixture::new().await;
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"data": [{"id": "gpt-4o"}]})),
        )
        .mount(&server)
        .await;
    let config = conversation_only_config(&fixture, server.uri());
    std::fs::write(
        fixture.user.path().join("config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    let mut deps = fixture.deps(vec![], vec![]);
    deps.models = None;
    deps.env = Arc::new(routing::MapEnv::from_iter([("KEY", "fixture-secret")]));
    let args = fixture.args(None);
    let dir = args.output.clone().unwrap();

    let error = run(args, deps, |_| panic!("no replay may start"))
        .await
        .unwrap_err();
    assert!(matches!(error, EvalError::Preflight { .. }), "{error}");
    assert!(error.to_string().contains("user_simulator"), "{error}");
    let evidence = read_json(&dir.join(preflight::REPORT_FILE));
    assert!(evidence["checks"][0]["error"].is_null(), "{evidence}");
    assert_eq!(
        evidence["checks"][0]["bindings"],
        serde_json::json!([{"role": "worker", "category": "conversation"}])
    );
    assert_eq!(evidence["checks"][1]["label"], "user_simulator");
    assert_eq!(
        evidence["checks"][1]["bindings"],
        serde_json::json!([{"role": "worker", "category": null}])
    );
    assert!(
        evidence["checks"][1]["error"]
            .as_str()
            .unwrap()
            .contains("worker"),
        "{evidence}"
    );
    assert_eq!(
        read_json(&dir.join(output::MANIFEST_FILE))["runs"],
        serde_json::json!([])
    );
    assert!(!dir.join("runs").exists());
    let requests = server.received_requests().await.unwrap();
    assert!(
        !requests.is_empty(),
        "the conversation binding was verified"
    );
    assert!(requests.iter().all(|request| request.method == "GET"));
}

#[tokio::test]
async fn preflight_accepts_explicit_simulator_with_conversation_only_worker() {
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
    let mut config = conversation_only_config(&fixture, server.uri());
    config.eval.user_simulator = Some(config::RoleBindingConfig {
        models: vec![config::ChainEntryConfig::route("chat")],
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
    let database = storage::Database::open_read_only(&fixture.recorded.config.db_path).unwrap();
    let thread = OriginalThread::load(&database, "thread-1").unwrap();
    let context = Context {
        identity: CommitIdentity::from_host(&thread.source_toplevel),
        thread,
        user_config_dir: Some(fixture.user.path().into()),
        budget: config.eval.budget.clone(),
        verify: None,
        verify_timeout: verify::DEFAULT_TIMEOUT,
        simulator: simulator_config(&config).0,
        inputs_guard: pins::Guard {
            user_config_dir: Some(fixture.user.path().into()),
            project_root: fixture.user.path().into(),
            common: None,
            candidate: None,
            pinned: pins::collect(
                Some(fixture.user.path()),
                fixture.user.path(),
                None,
                None,
                None,
            )
            .unwrap(),
        },
        trust: ProjectTrust::Unapproved,
        credentials: Arc::clone(deps.credentials.as_ref().unwrap()),
        deps,
    };
    let evidence = preflight::check(
        &context,
        &[(BASELINE, None)],
        &fixture.user.path().join("out"),
    )
    .await;
    assert!(evidence.failure().is_none(), "{:?}", evidence.failure());
    let evidence = serde_json::to_value(evidence).unwrap();
    assert_eq!(evidence["checks"][1]["label"], "user_simulator");
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[tokio::test]
async fn preflight_changed_child_base_cannot_be_masked_by_a_category() {
    let fixture = Fixture::new().await;
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"data": [{"id": "gpt-4o"}]})),
        )
        .mount(&server)
        .await;
    let mut config = preflight_config(&fixture, server.uri());
    config.routing.routes.remove("reviewer");
    config.agents.reviewer.categories.insert(
        "tool-execution".into(),
        config::CategoryBindingConfig {
            models: vec![config::ChainEntryConfig::route("worker")],
            ..Default::default()
        },
    );
    std::fs::write(
        fixture.user.path().join("config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    let mut deps = fixture.deps(vec![], vec![]);
    deps.models = None;
    deps.env = Arc::new(routing::MapEnv::from_iter([("KEY", "fixture-secret")]));
    let args = fixture.args(Some("[agents.reviewer.generation]\ntemperature = 0.5\n"));
    let dir = args.output.clone().unwrap();

    let error = run(args, deps, |_| panic!("no replay may start"))
        .await
        .unwrap_err();
    assert!(matches!(error, EvalError::Preflight { .. }), "{error}");
    let evidence = read_json(&dir.join(preflight::REPORT_FILE));
    for check in &evidence["checks"].as_array().unwrap()[..2] {
        assert!(
            check["error"].as_str().unwrap().contains("reviewer"),
            "{evidence}"
        );
    }
    assert_eq!(
        read_json(&dir.join(output::MANIFEST_FILE))["runs"],
        serde_json::json!([])
    );
    assert!(!dir.join("runs").exists());
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method == "GET")
    );
}

async fn rejects_undefined_route_before_replay(overlay: &str, role: &str) {
    let fixture = Fixture::new().await;
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/models"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"data": [{"id": "gpt-4o"}]})),
        )
        .mount(&server)
        .await;
    preflight_config(&fixture, server.uri());
    let mut deps = fixture.deps(vec![], vec![]);
    deps.models = None;
    deps.env = Arc::new(routing::MapEnv::from_iter([("KEY", "fixture-secret")]));
    let args = fixture.args(Some(overlay));
    let output = args.output.clone().unwrap();

    let error = run(args, deps, |_| {
        panic!("no baseline or candidate replay may start")
    })
    .await
    .unwrap_err();
    assert!(matches!(error, EvalError::Preflight { .. }), "{error}");
    let report = read_json(&output.join(preflight::REPORT_FILE));
    assert!(report["checks"][0]["error"].is_null(), "{report}");
    let reason = report["checks"][1]["error"].as_str().unwrap();
    assert!(reason.contains("typo") && reason.contains(role), "{report}");
    assert_eq!(report["checks"][1]["label"], CANDIDATE);
    assert_eq!(
        read_json(&output.join(output::MANIFEST_FILE))["runs"],
        serde_json::json!([])
    );
    assert!(!output.join("runs").exists());
    let requests = server.received_requests().await.unwrap();
    assert!(
        !requests.is_empty(),
        "the valid baseline catalog was checked"
    );
    assert!(
        requests.iter().all(|request| request.method == "GET"),
        "no inference requests: {requests:?}"
    );
}

#[tokio::test]
async fn preflight_rejects_undefined_child_head_before_any_replay() {
    rejects_undefined_route_before_replay(
        "[agents.reviewer]\nmodels = [{ route = 'typo' }]\n",
        "reviewer",
    )
    .await;
}

#[tokio::test]
async fn preflight_rejects_undefined_child_fallback_before_any_replay() {
    rejects_undefined_route_before_replay(
        "[agents.reviewer]\nmodels = [{ route = 'reviewer' }, { route = 'typo' }]\n",
        "reviewer",
    )
    .await;
}

#[tokio::test]
async fn preflight_rejects_undefined_category_fallback_before_any_replay() {
    rejects_undefined_route_before_replay(
        "[agents.worker.categories.quick]\nmodels = [{ route = 'worker' }, { route = 'typo' }]\n",
        "worker",
    )
    .await;
}

#[tokio::test]
async fn baseline_and_candidate_replays_each_leave_their_results() {
    // Given: a recorded thread, an agent that writes a file, and a user who then finishes.
    let fixture = Fixture::new().await;
    let overlay = "[agents.worker.generation]\ntemperature = 0.2\n";
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
    assert!(!evaluation.dir.join("preflight-workspace").exists());
    for report in &evaluation.runs {
        assert!(!report.dir.join("workspace").exists());
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
async fn a_cancel_during_the_first_replay_leaves_it_incomplete_and_starts_no_other() {
    // Given: a baseline and a candidate, and an agent whose first request never answers.
    let fixture = Fixture::new().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let mut deps = fixture.deps(
        vec![Step::Gated {
            entered: Arc::clone(&entered),
            release: Arc::new(tokio::sync::Notify::new()),
            then: Box::new(Step::Hang),
        }],
        Vec::new(),
    );
    let (cancel, signal) = tokio::sync::watch::channel(false);
    deps.cancel = Some(signal);
    let overlay = "[agents.worker.generation]\ntemperature = 0.2\n";

    // When: a stop is asked while that request is in flight.
    let (result, ()) = tokio::join!(run(fixture.args(Some(overlay)), deps, |_| {}), async {
        entered.notified().await;
        cancel.send_replace(true);
    });

    // Then: the evaluation returns with the baseline incomplete, and the candidate never ran.
    let evaluation = result.unwrap();
    let [report] = evaluation.runs.as_slice() else {
        panic!("only the baseline ran: {:?}", evaluation.runs.len());
    };
    assert_eq!(report.outcome.status, Status::Incomplete);
    assert!(
        report.outcome.reason.contains("cancelled"),
        "{}",
        report.outcome.reason
    );
    assert_eq!(fixture.agents.lock().unwrap().len(), 1);
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
    assert!(!evaluation.dir.join("preflight-workspace").exists());
    assert!(!report.dir.join("workspace").exists());
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

#[tokio::test]
async fn live_preflight_and_replay_cleanup_preserve_report_inputs_or_keep_clones() {
    use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
    for keep_workspace in [false, true] {
        let fixture = Fixture::new().await;
        let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
            vec![
                ScriptedResponse::text_stream("agent", "gpt-4o", ["done"]),
                ScriptedResponse::text_stream(
                    "simulator",
                    "gpt-4o",
                    [r#"{"action":"finish","text":"","reason":"done"}"#],
                ),
            ],
            vec!["gpt-4o".into()],
        );
        preflight_config(&fixture, mock.base_url());
        let mut deps = fixture.deps(vec![], vec![]);
        deps.models = None;
        deps.env = Arc::new(routing::MapEnv::from_iter([("KEY", "fixture-secret")]));
        let mut args = fixture.args(None);
        args.keep_workspace = keep_workspace;
        let evaluation = run(args, deps, |report| {
            assert!(
                report.dir.join("workspace").is_dir(),
                "still in use until all callbacks finish"
            );
        })
        .await
        .unwrap();
        assert_eq!(
            evaluation.runs[0].outcome.status,
            Status::Completed,
            "{:?}",
            evaluation.runs[0].outcome
        );
        assert_eq!(
            evaluation.dir.join("preflight-workspace").is_dir(),
            keep_workspace
        );
        assert_eq!(
            evaluation.runs[0].dir.join("workspace").is_dir(),
            keep_workspace
        );
        let reports = crate::report(
            crate::ReportArgs {
                dirs: vec![evaluation.dir.clone()],
                user_config_dir: Some(fixture.user.path().into()),
                ..Default::default()
            },
            &crate::ReportDeps {
                env: Arc::new(routing::MapEnv::default()),
                judge: Some(Arc::new(|_| panic!("baseline only: no comparison"))),
                credentials: None,
                catalog: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(reports.len(), 1);
        for name in [
            output::MANIFEST_FILE,
            preflight::REPORT_FILE,
            output::REPORT_JSON_FILE,
            output::REPORT_MARKDOWN_FILE,
        ] {
            assert!(evaluation.dir.join(name).is_file(), "{name}");
        }
        for name in [
            output::DATABASE_FILE,
            output::DIFF_FILE,
            output::CONVERSATION_FILE,
            output::OUTCOME_FILE,
        ] {
            assert!(evaluation.runs[0].dir.join(name).is_file(), "{name}");
        }
    }
}

#[tokio::test]
async fn write_failure_also_reaches_cleanup_without_hiding_the_error() {
    let fixture = Fixture::new().await;
    let args = fixture.args(None);
    let dir = args.output.clone().unwrap();
    let error = run(
        args,
        fixture.deps(vec![text("done")], vec![decision("finish", "")]),
        |_| {
            // Force the final manifest write to fail after a real replay.
            std::fs::remove_file(dir.join(output::MANIFEST_FILE)).unwrap();
            std::fs::create_dir(dir.join(output::MANIFEST_FILE)).unwrap();
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, EvalError::Io(_)), "{error}");
    assert!(!dir.join("runs/baseline-1/workspace").exists());
    assert!(dir.join("runs/baseline-1/outcome.json").is_file());
}

#[tokio::test]
async fn cleanup_failure_does_not_change_success_and_keep_applies_to_failure() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new().await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("sentinel"), "safe").unwrap();
    let mut cleanup_failed = false;
    let evaluation = run_with_cleanup(
        fixture.args(None),
        fixture.deps(vec![text("done")], vec![decision("finish", "")]),
        |report| {
            // Swap after cleanup opens the descriptor: removal must fail rather
            // than unlink the replacement, without changing evaluation success.
            let target = outside.path().to_owned();
            crate::cleanup::on_workspace_opened(move |workspace| {
                std::fs::rename(workspace, workspace.with_file_name("moved")).unwrap();
                symlink(target, workspace).unwrap();
            });
            let busy = crate::cleanup::clean(&[report.dir.parent().unwrap().parent().unwrap().to_owned()], None, Default::default());
            assert!(matches!(&busy.entries[0].action, crate::cleanup::Action::Skip(reason) if reason == "busy"));
        },
        |_, report| cleanup_failed = report.has_failures(),
    )
    .await
    .unwrap();
    assert!(cleanup_failed);
    assert_eq!(evaluation.runs[0].outcome.status, Status::Completed);
    assert_eq!(
        std::fs::read_to_string(outside.path().join("sentinel")).unwrap(),
        "safe"
    );
    // A replay failure after prepare still keeps a real clone when requested.
    for keep_workspace in [false, true] {
        let mut args = fixture.args(None);
        args.output = Some(fixture.user.path().join(if keep_workspace {
            "kept-failure"
        } else {
            "cleaned-failure"
        }));
        args.keep_workspace = keep_workspace;
        // All structured retries receive invalid simulator JSON.
        let deps = fixture.deps(vec![text("done")], vec![text("invalid"); 3]);
        let evaluation = run(args, deps, |_| {}).await.unwrap();
        assert_eq!(evaluation.runs[0].outcome.status, Status::Failed);
        assert_eq!(
            evaluation.runs[0].dir.join("workspace").is_dir(),
            keep_workspace
        );
    }
}

fn write_check() -> storage::eval_sample::Check {
    storage::eval_sample::Check::SuccessfulCalls {
        tool: "write".into(),
        min: Some(1),
        max: None,
        scope: storage::eval_sample::CheckScope::Tree,
        calls: storage::eval_sample::CallRange::Direct,
    }
}

fn pinned_sample(tags: &[&str]) -> storage::eval_sample::EvalSample {
    storage::eval_sample::EvalSample {
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        checks: vec![write_check()],
        ..storage::eval_sample::EvalSample::new(
            "thread-1".into(),
            "the notes were missing".into(),
            Vec::new(),
            1,
        )
    }
}

fn writes_notes() -> Vec<Step> {
    vec![
        tool(
            "write",
            serde_json::json!({"path": "notes.txt", "content": "replayed\n"}),
        ),
        text("wrote the notes"),
    ]
}

#[tokio::test]
async fn the_manifest_pins_the_sample_as_it_was_at_the_start() {
    // Given: a thread with a sample that declares a check.
    let fixture = Fixture::new().await;
    let handle = fixture.recorded.storage.handle();
    let at_start = pinned_sample(&["first"]);
    handle.save_eval_sample(at_start.clone()).unwrap();

    // When: an evaluation runs and the sample is edited afterwards.
    let evaluation = run(
        fixture.args(None),
        fixture.deps(writes_notes(), vec![decision("finish", "")]),
        |_| {},
    )
    .await
    .unwrap();
    handle.save_eval_sample(pinned_sample(&["second"])).unwrap();

    // Then: the manifest still holds the sample, its checks and its hash from the start.
    let manifest = read_json(&evaluation.dir.join(output::MANIFEST_FILE));
    assert_eq!(manifest["sample"]["hash"], at_start.content_hash());
    assert_eq!(
        manifest["sample"]["sample"]["tags"],
        serde_json::json!(["first"])
    );
    assert_eq!(manifest["sample"]["sample"]["checks"][0]["tool"], "write");
    assert_ne!(
        manifest["sample"]["hash"],
        pinned_sample(&["second"]).content_hash()
    );
}

#[tokio::test]
async fn an_evaluation_of_a_thread_without_a_sample_pins_none() {
    let fixture = Fixture::new().await;

    let evaluation = run(
        fixture.args(None),
        fixture.deps(writes_notes(), vec![decision("finish", "")]),
        |_| {},
    )
    .await
    .unwrap();

    let manifest = read_json(&evaluation.dir.join(output::MANIFEST_FILE));
    assert!(manifest.get("sample").is_none(), "{manifest}");
}

#[tokio::test]
async fn checks_only_regrades_recorded_runs_without_asking_any_model() {
    // Given: a finished evaluation that pinned a sample whose check the replay satisfies.
    let fixture = Fixture::new().await;
    fixture
        .recorded
        .storage
        .handle()
        .save_eval_sample(pinned_sample(&[]))
        .unwrap();
    let evaluation = run(
        fixture.args(None),
        fixture.deps(writes_notes(), vec![decision("finish", "")]),
        |_| {},
    )
    .await
    .unwrap();
    let models_before = fixture.agents.lock().unwrap().len();
    let manifest_before = std::fs::read(evaluation.dir.join(output::MANIFEST_FILE)).unwrap();

    // When: the directory is graded again from its evidence.
    let reports = crate::report::checks_only_pinned(
        vec![output::PinnedDir::open_path(&evaluation.dir).unwrap()],
        None,
        |_| {},
    )
    .await
    .unwrap();

    // Then: the replay passes its check on a complete record, the result has its own
    // files, and neither the manifest nor a report.json was written or changed.
    let [report] = reports.as_slice() else {
        panic!("one report: {reports:?}");
    };
    assert_eq!(report.basis, crate::report::Basis::Pinned);
    assert_eq!(
        report.runs[0].grading.quality(),
        crate::checks::Quality::Pass
    );
    assert!(evaluation.dir.join(report.file_name("json")).is_file());
    assert!(evaluation.dir.join(report.file_name("md")).is_file());
    assert!(!evaluation.dir.join(output::REPORT_JSON_FILE).exists());
    assert_eq!(
        std::fs::read(evaluation.dir.join(output::MANIFEST_FILE)).unwrap(),
        manifest_before
    );
    assert_eq!(fixture.agents.lock().unwrap().len(), models_before);
}

#[tokio::test]
async fn an_evaluation_without_a_pinned_sample_is_graded_post_hoc_and_kept_apart() {
    // Given: an evaluation started before its thread had a sample, and a sample made afterwards.
    let fixture = Fixture::new().await;
    let evaluation = run(
        fixture.args(None),
        fixture.deps(writes_notes(), vec![decision("finish", "")]),
        |_| {},
    )
    .await
    .unwrap();
    let handle = fixture.recorded.storage.handle();
    let grade = |database: Option<PathBuf>| {
        let dir = output::PinnedDir::open_path(&evaluation.dir).unwrap();
        async move { crate::report::checks_only_pinned(vec![dir], database.as_deref(), |_| {}).await }
    };

    // When: it is graded without a database to read the sample from, and with one.
    let without = grade(None).await.unwrap_err();
    handle.save_eval_sample(pinned_sample(&["late"])).unwrap();
    let first = grade(Some(fixture.recorded.config.db_path.clone()))
        .await
        .unwrap()
        .remove(0);
    handle.save_eval_sample(pinned_sample(&["later"])).unwrap();
    let second = grade(Some(fixture.recorded.config.db_path.clone()))
        .await
        .unwrap()
        .remove(0);

    // Then: it is refused without a sample, marked post-hoc with one, and a changed sample
    // leaves a separate result next to the earlier one.
    assert!(
        without.to_string().contains("no checks to grade"),
        "{without}"
    );
    assert_eq!(first.basis, crate::report::Basis::CurrentSample);
    assert!(first.note.contains("Post-hoc"), "{}", first.note);
    assert_ne!(first.sample_hash, second.sample_hash);
    assert!(evaluation.dir.join(first.file_name("json")).is_file());
    assert!(evaluation.dir.join(second.file_name("json")).is_file());
    let manifest = read_json(&evaluation.dir.join(output::MANIFEST_FILE));
    assert!(manifest.get("sample").is_none());
}

#[tokio::test]
async fn regrading_the_same_sample_keeps_the_earlier_result_and_writes_a_new_one() {
    // Given: a finished evaluation that pinned a sample, graded once.
    let fixture = Fixture::new().await;
    fixture
        .recorded
        .storage
        .handle()
        .save_eval_sample(pinned_sample(&[]))
        .unwrap();
    let evaluation = run(
        fixture.args(None),
        fixture.deps(writes_notes(), vec![decision("finish", "")]),
        |_| {},
    )
    .await
    .unwrap();
    let grade = || async {
        let dir = output::PinnedDir::open_path(&evaluation.dir).unwrap();
        crate::report::checks_only_pinned(vec![dir], None, |_| {})
            .await
            .unwrap()
            .remove(0)
    };
    let first = grade().await;
    let first_json = std::fs::read(evaluation.dir.join(first.file_name("json"))).unwrap();
    let first_md = std::fs::read(evaluation.dir.join(first.file_name("md"))).unwrap();

    // When: the same sample is graded again from the same evidence.
    let second = grade().await;

    // Then: the second analysis has its own numbered files, and the first result's files
    // are exactly as they were.
    assert_eq!(first.sample_hash, second.sample_hash);
    assert_eq!((first.analysis, second.analysis), (1, 2));
    assert_ne!(first.file_name("json"), second.file_name("json"));
    assert!(evaluation.dir.join(second.file_name("json")).is_file());
    assert!(evaluation.dir.join(second.file_name("md")).is_file());
    assert_eq!(
        std::fs::read(evaluation.dir.join(first.file_name("json"))).unwrap(),
        first_json
    );
    assert_eq!(
        std::fs::read(evaluation.dir.join(first.file_name("md"))).unwrap(),
        first_md
    );
}

fn with_common(fixture: &Fixture, mut args: EvalArgs, common: &str) -> EvalArgs {
    let path = fixture.user.path().join("common.toml");
    std::fs::write(&path, common).unwrap();
    args.common = Some(path);
    args
}

const WORKER_TEMPERATURE: &str = "[agents.worker.generation]\ntemperature = 0.2\n";

#[tokio::test]
async fn the_common_overlay_is_pinned_on_both_sides_and_the_manifest_records_what_changed() {
    // Given: a common overlay and a candidate that changes the worker's generation.
    let fixture = Fixture::new().await;
    let common = "[agents.reviewer.generation]\nmax_tokens = 4000\n";
    let args = with_common(&fixture, fixture.args(Some(WORKER_TEMPERATURE)), common);
    let deps = fixture.deps(vec![text("done")], vec![decision("finish", "")]);

    // When: the evaluation runs.
    let evaluation = run(args, deps, |_| {}).await.unwrap();

    // Then: the manifest keeps the common overlay, records the change as a binding difference
    // with the roles it reaches, and pins hashes of both sides, where the baseline already
    // carries the common overlay.
    let manifest = read_json(&evaluation.dir.join(output::MANIFEST_FILE));
    assert_eq!(manifest["common_overlay"], common);
    assert_eq!(
        manifest["changes"]["bindings"],
        serde_json::json!(["worker"])
    );
    assert_eq!(
        manifest["changes"]["affected_roles"],
        serde_json::json!(["worker"])
    );
    assert_eq!(
        manifest["changes"]["tool_descriptions"],
        serde_json::json!([])
    );
    let user = Some(fixture.user.path());
    let project = fixture.recorded.source.root().canonicalize().unwrap();
    let candidate = Overlay {
        text: WORKER_TEMPERATURE.into(),
        value: toml::from_str(WORKER_TEMPERATURE).unwrap(),
    };
    let common = Overlay {
        text: common.into(),
        value: toml::from_str(common).unwrap(),
    };
    let sample = manifest["pins"]["inputs"]["sample_hash"]
        .as_str()
        .map(str::to_owned);
    let with = pins::collect(
        user,
        &project,
        sample.clone(),
        Some(&common),
        Some(&candidate),
    )
    .unwrap();
    let without = pins::collect(user, &project, sample, None, Some(&candidate)).unwrap();
    assert_eq!(manifest["pins"]["digest"], with.digest());
    assert_eq!(
        manifest["pins"]["inputs"]["baseline_config_hash"],
        with.baseline_config_hash
    );
    assert_ne!(with.baseline_config_hash, without.baseline_config_hash);
    assert_eq!(manifest["pins"]["build"]["package"], "eval");
    assert!(manifest["pins"]["configured_models"]["baseline"].is_array());
    assert!(manifest["pins"]["configured_models"]["candidate"].is_array());
    assert!(
        manifest["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|run| run["observed"].is_object()),
        "{manifest}"
    );
}

#[tokio::test]
async fn a_common_overlay_cannot_change_anything_but_models_and_roles() {
    // Given: a common overlay that tries to change the sandbox.
    let fixture = Fixture::new().await;
    let args = with_common(
        &fixture,
        fixture.args(Some(WORKER_TEMPERATURE)),
        "[sandbox]\nweb_tools_enabled = false\n",
    );
    let deps = fixture.deps(vec![], vec![]);

    // When: the evaluation is started.
    let error = run(args, deps, |_| panic!("no replay may start"))
        .await
        .unwrap_err();

    // Then: it is refused as a common overlay, before anything is written.
    assert!(matches!(error, EvalError::Common(_)), "{error}");
    assert!(
        error
            .to_string()
            .contains("cannot be set by a common overlay"),
        "{error}"
    );
    assert!(!fixture.user.path().join("out").exists());
}

#[tokio::test]
async fn candidates_that_change_nothing_or_a_tool_description_are_refused_before_any_replay() {
    // Given: a candidate equal to the baseline, and one that rewrites a tool description.
    let fixture = Fixture::new().await;
    for (candidate, reason) in [
        ("[agents.worker]\n", "changes nothing"),
        (
            "[model_tool_descriptions.primary.gpt-4o]\nread = \"Read.\"\n",
            "judge is not yet told",
        ),
    ] {
        let deps = fixture.deps(vec![], vec![]);

        // When: the evaluation is started.
        let error = run(fixture.args(Some(candidate)), deps, |_| {
            panic!("no replay may start")
        })
        .await
        .unwrap_err();

        // Then: the comparison is refused with its reason, with no output and no model built.
        assert!(
            matches!(&error, EvalError::Comparison(text) if text.contains(reason))
                || matches!(&error, EvalError::Overlay(_) | EvalError::Config(_)),
            "{error}"
        );
        assert!(!fixture.user.path().join("out").exists());
        assert!(fixture.agents.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn an_evaluation_approved_on_other_inputs_runs_nothing() {
    // Given: an approval digest of inputs that no longer match.
    let fixture = Fixture::new().await;
    let mut args = fixture.args(Some(WORKER_TEMPERATURE));
    args.approved_digest = Some("0".repeat(64));
    let deps = fixture.deps(vec![], vec![]);

    // When: the evaluation is started.
    let error = run(args, deps, |_| panic!("no replay may start"))
        .await
        .unwrap_err();

    // Then: it is refused, and nothing was written.
    assert!(
        matches!(&error, EvalError::Usage(text) if text.contains("after the evaluation was approved")),
        "{error}"
    );
    assert!(!fixture.user.path().join("out").exists());
    assert!(fixture.agents.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_preset_edited_after_the_baseline_stops_the_evaluation_before_the_candidate() {
    // Given: an evaluation of one baseline and one candidate replay.
    let fixture = Fixture::new().await;
    let deps = fixture.deps(vec![text("done")], vec![decision("finish", "")]);
    let presets = fixture.user.path().join("presets");

    // When: a prompt preset is edited as the baseline ends.
    let result = run(fixture.args(Some(WORKER_TEMPERATURE)), deps, |report| {
        if report.label == BASELINE {
            std::fs::create_dir_all(&presets).unwrap();
            std::fs::write(presets.join("role-worker.md"), "edited between the replays").unwrap();
        }
    })
    .await;

    // Then: the candidate never starts, and the manifest says the evaluation ended.
    let error = result.unwrap_err();
    assert!(
        matches!(&error, EvalError::Usage(text) if text.contains("prompt presets")),
        "{error}"
    );
    let manifest = read_json(&fixture.user.path().join("out").join(output::MANIFEST_FILE));
    assert_eq!(manifest["runs"].as_array().unwrap().len(), 1);
    assert!(manifest["finished_at"].is_string());
}
