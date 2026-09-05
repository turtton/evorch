//! headless run entry の結合テスト (issue #79)。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use event_bus::AgentRunPhase;
use evorch::headless::{HeadlessArgs, HeadlessError, SandboxChoice, parse_args, run_headless};
use routing::MapEnv;
use runtime::Role;
use serde_json::{Value, json};

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

#[derive(Clone, Debug)]
struct RecordedRequest {
    authorization: Option<String>,
    body: Value,
}

// crates/runtime/tests/support/mock_openai.rs と同型の localhost モック。
struct RecordingMockOpenAi {
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl RecordingMockOpenAi {
    fn spawn(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("モックサーバを bind できる");
        let addr = listener.local_addr().expect("モックアドレスを取得できる");
        let script = Mutex::new(VecDeque::from(responses));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded_requests = Arc::clone(&requests);

        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let Some(response) = script
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front()
                else {
                    continue;
                };
                if let Some(request) = read_request(&mut stream) {
                    recorded_requests
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(request);
                }
                let http = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                stream
                    .write_all(http.as_bytes())
                    .expect("モック応答を書き込める");
            }
        });

        Self {
            base_url: format!("http://{addr}/v1"),
            requests,
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            if buf.len() >= header_end + 4 + content_length {
                let authorization = headers.lines().find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("authorization")
                            .then(|| value.trim().to_string())
                    })
                });
                let body_start = header_end + 4;
                let body = serde_json::from_slice(&buf[body_start..body_start + content_length])
                    .unwrap_or(Value::Null);
                return Some(RecordedRequest {
                    authorization,
                    body,
                });
            }
        }
        let read = stream.read(&mut chunk).expect("モックリクエストを読める");
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

fn openai_text_response(text: &str) -> String {
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string()
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
    let mock = RecordingMockOpenAi::spawn(vec![openai_text_response("headless ok")]);
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

    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer headless-e2e-key")
    );
    assert_eq!(requests[0].body["model"], MODEL);
}
