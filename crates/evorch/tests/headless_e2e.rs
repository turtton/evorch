//! headless run entry の結合テスト (issue #79)。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use event_bus::AgentRunPhase;
use evorch::headless::{HeadlessArgs, HeadlessError, SandboxChoice, parse_args, run_headless};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::Role;

const KEY_ENV: &str = "EVORCH_TEST_KEY_HEADLESS_E2E";
const KEY: &str = "headless-e2e-key";
const MODEL: &str = "local-model";
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
    ]))
    .expect("妥当な引数はパースできる");

    assert_eq!(parsed.project_dir, PathBuf::from("/tmp/project"));
    assert_eq!(parsed.role, Role::Worker);
    assert_eq!(parsed.prompt, "hello world");
    assert_eq!(
        parsed.user_config_dir,
        Some(PathBuf::from("/tmp/user-config"))
    );
}

// Given: --role の各既知名 / When: parse_args する / Then: runtime Role へ写像される
#[test]
fn parse_args_maps_role_names() {
    for (text, expected) in [
        ("worker", Role::Worker),
        ("orchestrator", Role::Orchestrator),
        ("explorer", Role::Explorer),
        ("reviewer", Role::Reviewer),
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

fn write_project_config(root: &std::path::Path, base_url: &str) {
    std::fs::write(
        root.join("evorch.toml"),
        format!(
            r#"[providers.local]
type = "openai-compatible"
base_url = "{base_url}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"
"#
        ),
    )
    .expect("evorch.toml を書ける");
}

fn headless_args(project_dir: PathBuf, user_config_dir: Option<PathBuf>) -> HeadlessArgs {
    HeadlessArgs {
        project_dir,
        role: Role::Worker,
        prompt: PROMPT.to_string(),
        user_config_dir,
    }
}

// Given: sugar provider 設定 (localhost モック) と MapEnv credential
// When: DirectUnchecked で worker を headless 実行する
// Then: phase Done、final_text にモック応答が含まれ、モックは Bearer 認証付き
//       model=local-model の 1 リクエストだけを受け取る
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_run_completes_with_single_mock_response() {
    let directory = tempfile::tempdir().expect("project directory");
    let mock = StreamingMockOpenAi::spawn(vec![
        ScriptedResponse::text_stream("text", MODEL, ["headless ok"]).with_usage(1, 1),
    ]);
    write_project_config(directory.path(), &mock.base_url());
    let env = MapEnv::from_iter([(KEY_ENV, KEY)]);

    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        run_headless(
            headless_args(
                directory.path().to_path_buf(),
                Some(directory.path().join("user-config")),
            ),
            Arc::new(env),
            SandboxChoice::DirectUnchecked,
        ),
    )
    .await
    .expect("headless run がタイムアウトしない")
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
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer headless-e2e-key")
    );
    assert_eq!(requests[0].body["model"], MODEL);
    assert!(requests[0].stream);
    assert_eq!(requests[0].body["stream"], true);
}
