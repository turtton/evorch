//! headless run entry の結合テスト (issue #79)。

use std::path::PathBuf;
use std::sync::Arc;

use event_bus::AgentRunPhase;
use evorch::headless::{HeadlessArgs, HeadlessError, SandboxChoice, parse_args, run_headless};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::Role;
use serde_json::json;

const KEY_ENV: &str = "EVORCH_TEST_KEY_HEADLESS_E2E";
const KEY: &str = "headless-e2e-key";
const MODEL: &str = "gpt-4o";
const PROMPT: &str = "HEADLESS-PROMPT";

fn argv(items: &[&str]) -> impl Iterator<Item = String> {
    items.iter().map(|item| (*item).to_string())
}

fn usage_error() -> HeadlessError {
    parse_args(argv(&["run"])).expect_err("引数不足は Usage エラーになる")
}

// Given: run の全フラグ / When: parse_args する / Then: 各フィールドへ写像される
#[test]
fn parse_args_accepts_full_form() {
    let parsed = parse_args(argv(&[
        "run",
        "--project",
        "/tmp/project",
        "--role",
        "worker",
        "--prompt",
        "hello world",
        "--user-config",
        "/tmp/user-config",
        "--web-tools-enabled",
        "false",
        "--run-js-enabled",
        "true",
        "--db",
        "/tmp/headless-events.db",
    ]))
    .expect("妥当な引数はパースできる");

    assert_eq!(parsed.project_dir, PathBuf::from("/tmp/project"));
    assert_eq!(parsed.role, Role::Worker);
    assert_eq!(parsed.prompt, "hello world");
    assert_eq!(parsed.web_tools_enabled, Some(false));
    assert_eq!(parsed.run_js_enabled, Some(true));
    assert_eq!(
        parsed.db_path,
        Some(PathBuf::from("/tmp/headless-events.db"))
    );
    assert_eq!(
        parsed.user_config_dir,
        Some(PathBuf::from("/tmp/user-config"))
    );
}

// Given: 相対パスの --project と空の --project / When: parse_args する
// Then: 相対パスは今のプロセスの作業ディレクトリ基準の絶対パスになり、tool が引数を
//       何度 workspace に結合しても同じ場所を指す。空のパスは Usage エラー
#[test]
fn parse_args_makes_a_relative_project_absolute() {
    let parse = |project: &str| {
        parse_args(argv(&[
            "run",
            "--project",
            project,
            "--role",
            "worker",
            "--prompt",
            "x",
        ]))
    };
    let parsed = parse("relative/project").expect("相対パスはパースできる");
    assert_eq!(
        parsed.project_dir,
        std::env::current_dir().unwrap().join("relative/project")
    );

    let error = parse("").expect_err("空のパスはエラーになる");
    assert_eq!(error.to_string(), usage_error().to_string());
}

// Given: --role の各既知名 / When: parse_args する / Then: runtime Role へ写像される
#[test]
fn parse_args_maps_role_names() {
    for (text, expected) in [
        ("worker", Role::Worker),
        ("orchestrator", Role::Orchestrator),
        ("explorer", Role::Explorer),
        ("reviewer", Role::Reviewer),
        ("web_researcher", Role::WebResearcher),
        ("planner", Role::Planner),
        ("advisor", Role::Advisor),
    ] {
        let parsed = parse_args(argv(&[
            "run",
            "--project",
            "/p",
            "--role",
            text,
            "--prompt",
            "x",
        ]))
        .unwrap_or_else(|error| panic!("role {text} はパースできる: {error}"));
        assert_eq!(parsed.role, expected);
        assert_eq!(parsed.web_tools_enabled, None);
        assert_eq!(parsed.run_js_enabled, None);
        assert_eq!(parsed.db_path, None);
    }
}

// Given: --prompt を欠く引数 / When: parse_args する / Then: Usage エラー
#[test]
fn parse_args_requires_prompt() {
    let error = parse_args(argv(&["run", "--project", "/p", "--role", "worker"]))
        .expect_err("prompt 欠落はエラーになる");

    assert!(matches!(error, HeadlessError::Usage(_)));
    assert_eq!(error.to_string(), usage_error().to_string());
}

// Given: 未知のフラグ / When: parse_args する / Then: Usage エラー
#[test]
fn parse_args_rejects_unknown_flag() {
    let error = parse_args(argv(&[
        "run",
        "--project",
        "/p",
        "--role",
        "worker",
        "--prompt",
        "x",
        "--unknown",
    ]))
    .expect_err("未知フラグはエラーになる");

    assert!(matches!(error, HeadlessError::Usage(_)));
}

#[test]
fn parse_args_rejects_non_boolean_switch_overrides() {
    for flag in ["--web-tools-enabled", "--run-js-enabled"] {
        let error = parse_args(argv(&[
            "run",
            "--project",
            "/p",
            "--role",
            "orchestrator",
            "--prompt",
            "x",
            flag,
            "allowed",
        ]))
        .expect_err("a tool availability switch must be a boolean");
        assert!(matches!(error, HeadlessError::Usage(_)), "{flag}");
    }
}

// Given: 値を欠く --db / When: parse_args する / Then: Usage エラー
#[test]
fn parse_args_requires_a_value_for_db() {
    let error = parse_args(argv(&[
        "run",
        "--project",
        "/p",
        "--role",
        "worker",
        "--prompt",
        "x",
        "--db",
    ]))
    .expect_err("--db without a path is rejected");
    assert!(matches!(error, HeadlessError::Usage(_)));
}

// Given: 未知のロール名 / When: parse_args する / Then: Usage エラー
#[test]
fn parse_args_rejects_unknown_role() {
    let error = parse_args(argv(&[
        "run",
        "--project",
        "/p",
        "--role",
        "chef",
        "--prompt",
        "x",
    ]))
    .expect_err("未知ロールはエラーになる");

    assert!(matches!(error, HeadlessError::Usage(_)));
}

// Given: サブコマンドが run 以外 / When: parse_args する / Then: Usage エラー
#[test]
fn parse_args_requires_run_subcommand() {
    let error = parse_args(argv(&["exec", "--project", "/p"])).expect_err("run 以外はエラーになる");

    assert!(matches!(error, HeadlessError::Usage(_)));
}

/// プロバイダとルーティングは信頼済みのユーザ層 (`<root>/user-config`) に置く。
fn write_user_config(root: &std::path::Path, base_url: &str) {
    std::fs::create_dir_all(root.join("user-config")).expect("config directory");
    std::fs::write(
        user_config_path(root),
        format!(
            r#"[providers.local]
type = "openai-compatible"
base_url = "{base_url}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"

[[routing.routes.worker]]
profile = "local"

[[routing.routes.web_researcher]]
profile = "local"
"#
        ),
    )
    .expect("user-config/config.toml を書ける");
}

fn user_config_path(root: &std::path::Path) -> PathBuf {
    root.join("user-config").join("config.toml")
}

fn headless_args(project_dir: PathBuf, user_config_dir: Option<PathBuf>) -> HeadlessArgs {
    HeadlessArgs {
        project_dir,
        role: Role::Worker,
        prompt: PROMPT.to_string(),
        user_config_dir,
        web_tools_enabled: None,
        run_js_enabled: None,
        db_path: None,
        model_catalog_dir: Some(test_support::model_catalog_dir()),
    }
}

// Given: sugar provider 設定 (localhost モック) と MapEnv credential
// When: DirectUnchecked で worker を headless 実行する
// Then: user AGENTS の全文だけが system に注入され、phase Done、モックは Bearer 認証付き
//       model=gpt-4o の completion リクエストを 1 件だけ受け取る
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_run_injects_user_agents_from_explicit_config_directory() {
    let directory = tempfile::tempdir().expect("project directory");
    let mock = StreamingMockOpenAi::spawn_with_models(
        vec![ScriptedResponse::text_stream("text", MODEL, ["headless ok"]).with_usage(1, 1)],
        mock_openai::WriteMode::default(),
        vec![MODEL.to_owned()],
    );
    write_user_config(directory.path(), &mock.base_url());
    let env = MapEnv::from_iter([(KEY_ENV, KEY)]);
    let user_config = directory.path().join("user-config");
    std::fs::create_dir_all(&user_config).expect("user config directory");
    let body = "---\nalwaysApply: false\n---\n# User instructions\nHEADLESS-USER-AGENTS\n";
    std::fs::write(user_config.join("AGENTS.md"), body).expect("user AGENTS.md");
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "UNAPPROVED-PROJECT-RULE",
    )
    .expect("project AGENTS.md");
    let args = HeadlessArgs {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        ..parse_args(argv(&[
            "run",
            "--project",
            directory.path().to_str().unwrap(),
            "--role",
            "worker",
            "--prompt",
            PROMPT,
            "--user-config",
            user_config.to_str().unwrap(),
        ]))
        .expect("headless CLI args")
    };

    let outcome = run_headless(args, Arc::new(env), SandboxChoice::DirectUnchecked)
        .await
        .expect("headless run が成功する");

    assert_eq!(outcome.phase, AgentRunPhase::Done);
    assert!(
        outcome
            .final_text
            .as_deref()
            .is_some_and(|text| text.contains("headless ok")),
        "final_text にモック応答が含まれる: {:?}",
        outcome.final_text
    );

    let requests = mock.recorded_requests();
    let completion_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .collect();
    assert_eq!(completion_requests.len(), 1);
    assert_eq!(
        completion_requests[0].authorization.as_deref(),
        Some("Bearer headless-e2e-key")
    );
    assert_eq!(completion_requests[0].body["model"], MODEL);
    assert!(completion_requests[0].stream);
    assert_eq!(completion_requests[0].body["stream"], true);
    let messages = completion_requests[0].body["messages"]
        .as_array()
        .expect("messages");
    let system = messages
        .iter()
        .filter(|message| message["role"] == "system")
        .collect::<Vec<_>>();
    assert_eq!(system.len(), 1);
    let content = system[0]["content"].as_str().expect("system content");
    assert!(content.contains(body));
    assert!(!content.contains("UNAPPROVED-PROJECT-RULE"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_web_tools_follow_saved_setting_and_cli_override() {
    for (saved, override_enabled, expected) in [
        (true, None, true),
        (false, None, false),
        (true, Some(false), false),
        (false, Some(true), true),
    ] {
        let directory = tempfile::tempdir().expect("project directory");
        let mock = StreamingMockOpenAi::spawn_with_models(
            vec![ScriptedResponse::text_stream("text", MODEL, ["done"]).with_usage(1, 1)],
            mock_openai::WriteMode::default(),
            vec![MODEL.to_owned()],
        );
        write_user_config(directory.path(), &mock.base_url());
        config::save_sandbox(
            &user_config_path(directory.path()),
            config::SandboxConfig {
                web_tools_enabled: saved,
                ..Default::default()
            },
        )
        .expect("save sandbox");
        let mut args = headless_args(
            directory.path().to_path_buf(),
            Some(directory.path().join("user-config")),
        );
        args.role = Role::WebResearcher;
        args.web_tools_enabled = override_enabled;
        let outcome = run_headless(
            args,
            Arc::new(MapEnv::from_iter([(KEY_ENV, KEY)])),
            SandboxChoice::DirectUnchecked,
        )
        .await
        .expect("headless run succeeds");
        assert_eq!(outcome.phase, AgentRunPhase::Done);
        let requests = mock.recorded_requests();
        let completion = requests
            .iter()
            .find(|request| request.path == "/v1/chat/completions")
            .expect("completion request");
        let tools = completion.body["tools"].as_array().expect("tools");
        for tool_name in ["web_search", "web_fetch"] {
            assert_eq!(
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == tool_name),
                expected,
                "{tool_name}: saved={saved}, override={override_enabled:?}",
            );
        }
        assert!(tools.iter().any(|tool| tool["function"]["name"] == "read"));
    }
}

// Given: 保存設定 `[tools.run_js] enabled` / `scopes` と `--run-js-enabled` の組み合わせ
// When: worker を headless 実行する
// Then: モデルへ送る tools 一覧に run_js が出るのは有効で scopes が worker (カテゴリ無し) を含むときだけ。
//       既定の scopes (worker:deep / worker:high-reasoning) ではカテゴリ無しの worker に出ない
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_run_js_tool_follows_saved_setting_and_cli_override() {
    for (saved, scopes, override_enabled, expected) in [
        (None, None, None, false),
        (None, None, Some(false), false),
        (None, None, Some(true), false),
        (None, Some("[\"worker\"]"), Some(true), true),
        (Some(true), None, None, false),
        (Some(true), Some("[\"explorer\"]"), None, false),
        (Some(true), Some("[\"worker\"]"), None, true),
        (Some(true), Some("[\"worker\"]"), Some(false), false),
        (Some(false), Some("[\"worker\"]"), Some(true), true),
    ] {
        let directory = tempfile::tempdir().expect("project directory");
        let mock = StreamingMockOpenAi::spawn_with_models(
            vec![ScriptedResponse::text_stream("text", MODEL, ["done"]).with_usage(1, 1)],
            mock_openai::WriteMode::default(),
            vec![MODEL.to_owned()],
        );
        write_user_config(directory.path(), &mock.base_url());
        if saved.is_some() || scopes.is_some() {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(user_config_path(directory.path()))
                .expect("open user config");
            write!(file, "\n[tools.run_js]\n").expect("save run_js");
            if let Some(enabled) = saved {
                writeln!(file, "enabled = {enabled}").expect("save run_js");
            }
            if let Some(scopes) = scopes {
                writeln!(file, "scopes = {scopes}").expect("save run_js");
            }
        }
        let mut args = headless_args(
            directory.path().to_path_buf(),
            Some(directory.path().join("user-config")),
        );
        args.run_js_enabled = override_enabled;
        let outcome = run_headless(
            args,
            Arc::new(MapEnv::from_iter([(KEY_ENV, KEY)])),
            SandboxChoice::DirectUnchecked,
        )
        .await
        .expect("headless run succeeds");
        assert_eq!(outcome.phase, AgentRunPhase::Done);
        let requests = mock.recorded_requests();
        let completion = requests
            .iter()
            .find(|request| request.path == "/v1/chat/completions")
            .expect("completion request");
        let tools = completion.body["tools"].as_array().expect("tools");
        assert_eq!(
            tools
                .iter()
                .any(|tool| tool["function"]["name"] == "run_js"),
            expected,
            "saved={saved:?}, scopes={scopes:?}, override={override_enabled:?}",
        );
        assert!(tools.iter().any(|tool| tool["function"]["name"] == "read"));
    }
}

// Given: --db 付きの headless run (ScriptedModel 相当のモック応答 1 回)
// When: run が終了し、永続化先を `evorch inspect --db <path> run <run-id>` で読む
// Then: 終端まで flush 済みで、event_kinds・usage_requests・run の文脈が見える
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_run_persists_events_and_usage_to_the_database() {
    let directory = tempfile::tempdir().expect("project directory");
    let mock = StreamingMockOpenAi::spawn_with_models(
        vec![ScriptedResponse::text_stream("text", MODEL, ["persisted"]).with_usage(7, 3)],
        mock_openai::WriteMode::default(),
        vec![MODEL.to_owned()],
    );
    write_user_config(directory.path(), &mock.base_url());
    let db_path = directory.path().join("observed").join("headless.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).expect("database directory");
    let mut args = headless_args(
        directory.path().to_path_buf(),
        Some(directory.path().join("user-config")),
    );
    args.db_path = Some(db_path.clone());

    let outcome = run_headless(
        args,
        Arc::new(MapEnv::from_iter([(KEY_ENV, KEY)])),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .expect("headless run succeeds");
    assert_eq!(outcome.phase, AgentRunPhase::Done);

    let inspected = evorch::inspect::run(
        evorch::inspect::parse_args(argv(&[
            "inspect",
            "--db",
            db_path.to_str().unwrap(),
            "run",
            &outcome.run_id.to_string(),
        ]))
        .expect("inspect args"),
    )
    .expect("inspect reads the run database");
    let run = &inspected["result"];
    assert_eq!(run["context"]["role"], "Worker", "{run:#}");
    let usage = run["usage_requests"].as_array().expect("usage_requests");
    assert_eq!(usage.len(), 1, "{run:#}");
    assert_eq!(usage[0]["input_tokens"], 7, "{run:#}");
    assert_eq!(usage[0]["output_tokens"], 3, "{run:#}");
    let kinds = run["event_kinds"].as_object().expect("event_kinds");
    assert!(!kinds.is_empty(), "{run:#}");
}

// Given: --db なしの headless run / When: run が終了する / Then: データベースを作らない
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_run_without_db_creates_no_database() {
    let directory = tempfile::tempdir().expect("project directory");
    let mock = StreamingMockOpenAi::spawn_with_models(
        vec![ScriptedResponse::text_stream("text", MODEL, ["quiet"]).with_usage(1, 1)],
        mock_openai::WriteMode::default(),
        vec![MODEL.to_owned()],
    );
    write_user_config(directory.path(), &mock.base_url());
    let args = headless_args(
        directory.path().to_path_buf(),
        Some(directory.path().join("user-config")),
    );
    let outcome = run_headless(
        args,
        Arc::new(MapEnv::from_iter([(KEY_ENV, KEY)])),
        SandboxChoice::DirectUnchecked,
    )
    .await
    .expect("headless run succeeds");
    assert_eq!(outcome.phase, AgentRunPhase::Done);
    let created: Vec<_> = std::fs::read_dir(directory.path())
        .expect("project directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(
        created
            .iter()
            .all(|name| !name.to_string_lossy().ends_with(".db")),
        "{created:?}"
    );
}

// Given: 最終応答の前に blocking の ask_user を呼び、回答待ちで 1 ターンを終える worker
// When: headless 実行する (対話相手がいない)
// Then: 無期限に待たず、質問を示す WaitingForInput で終わり、終了コードは専用値になる
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_run_waiting_for_a_user_answer_stops_with_a_dedicated_error() {
    let directory = tempfile::tempdir().expect("project directory");
    let mock = StreamingMockOpenAi::spawn_with_models(
        vec![
            ScriptedResponse::tool_call(
                "ask",
                MODEL,
                0,
                "call-ask",
                "ask_user",
                [
                    json!({"title": "Include web_fetch in the count?", "blocking": true})
                        .to_string(),
                ],
            ),
            ScriptedResponse::text_stream("question", MODEL, ["Include web_fetch in the count?"]),
        ],
        mock_openai::WriteMode::default(),
        vec![MODEL.to_owned()],
    );
    write_user_config(directory.path(), &mock.base_url());
    let env = MapEnv::from_iter([(KEY_ENV, KEY)]);
    let args = HeadlessArgs {
        db_path: Some(directory.path().join("run.db")),
        ..headless_args(
            directory.path().to_path_buf(),
            Some(directory.path().join("user-config")),
        )
    };

    let error = run_headless(args, Arc::new(env), SandboxChoice::DirectUnchecked)
        .await
        .expect_err("a run that waits for an answer nobody can give does not finish");

    let HeadlessError::WaitingForInput { run_id, questions } = &error else {
        panic!("unexpected error: {error}");
    };
    assert!(run_id.starts_with("run-"), "{run_id}");
    assert_eq!(questions, "Include web_fetch in the count?");
    assert!(error.to_string().contains("non-interactive"), "{error}");
    assert_eq!(
        evorch::headless::exit_code(&error),
        evorch::headless::EXIT_WAITING_FOR_INPUT
    );
    assert_ne!(evorch::headless::EXIT_WAITING_FOR_INPUT, 1);
    assert_eq!(evorch::headless::exit_code(&usage_error()), 1);
    // Both responses were consumed: the run really asked, then ended its turn.
    let completions = mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .count();
    assert_eq!(completions, 2);
}
