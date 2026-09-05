//! T2.3 `ShellDeliveryAdapter` の統合テスト (AC7)。
//!
//! `DirectSandbox::new_unchecked()` と PATH 上の fake `gh` / `intent-cli` スクリプト
//! を使い、git push はローカル bare リモートに対して実行する。fake スクリプトは
//! 隣接する canned JSON ファイルを応答し、受信した argv をログへ残す。
//!
//! 契約上 `ApprovedMerge` は crate 外で構築できないため、merge_pr の argv 契約と
//! contract 拒否経路は `shell_delivery.rs` 内の crate 内部単体テストで検証する。

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use event_bus::{CiState, CloseoutStep, EventKind, GateEvidence, ToolEvent};
use runtime::orchestration::DeliveryPort;
use runtime::orchestration::shell_delivery::ShellDeliveryAdapter;
use sandbox::DirectSandbox;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};

/// 環境変数を変更するテストを直列化するロック。
///
/// 子プロセスの PATH 解決は親プロセス環境の `PATH` に依存するため、fake
/// スクリプト発見のためにテストプロセスの `PATH` を差し替える。並行テストと
/// の競合を避けるため、環境変数に触る区間は本ロックで直列化する。
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// 差し替えた環境変数をスコープ終了で復元するガード。
struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    /// 指定キーを現在値から退避した上で新値へ差し替える。
    fn set(vars: &[(&'static str, String)]) -> Self {
        let keys: Vec<&'static str> = vars.iter().map(|(key, _)| *key).collect();
        let guard = Self::remove(&keys);
        for (key, value) in vars {
            // SAFETY: ENV_LOCK で直列化された区間でのみ呼ばれ、このテストバイナリの
            // 他テストは環境変数を並行して読み書きしない。
            unsafe { std::env::set_var(key, value) };
        }
        guard
    }

    /// 指定キーの現在値を退避した上で削除する。
    fn remove(keys: &[&'static str]) -> Self {
        let saved = keys
            .iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect();
        for key in keys {
            // SAFETY: ENV_LOCK で直列化された区間でのみ呼ばれ、このテストバイナリの
            // 他テストは環境変数を並行して読み書きしない。
            unsafe { std::env::remove_var(key) };
        }
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            // SAFETY: ENV_LOCK で直列化された区間でのみ呼ばれる。
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// PATH に fake bin ディレクトリを先頭に追加するガードを返す。
fn path_guard(bin: &Path) -> EnvGuard {
    let current = std::env::var("PATH").unwrap_or_default();
    EnvGuard::set(&[("PATH", format!("{}:{current}", bin.display()))])
}

/// 実行可能な fake スクリプトを書き込む。
fn write_script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).expect("スクリプトを書き込める");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("実行権を付けられる");
}

fn write_file(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).expect("canned ファイルを書き込める");
}

/// 受信 argv をログへ残し、canned ファイルで応答する fake gh。
const GH_SCRIPT: &str = r#"DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$*" >> "$DIR/gh_args.log"
case "$1 $2" in
  "pr list") cat "$DIR/pr_list.json" ;;
  "pr create") echo "created" ;;
  "pr view") cat "$DIR/pr_view.json" ;;
  "pr merge") echo "squashed and merged" ;;
  *) echo "unexpected gh invocation: $*" >&2; exit 1 ;;
esac
"#;

/// 受信 argv をログへ残し、canned ファイルで応答する fake intent-cli。
const INTENT_CLI_SCRIPT: &str = r#"DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$*" >> "$DIR/intent_cli_args.log"
case "$1 $2" in
  "worker claim") echo '{"ok":true}' ;;
  "worker result-summary") cat "$DIR/result_summary.json" ;;
  "worker complete") echo '{"ok":true}' ;;
  *) echo "unexpected intent-cli invocation: $*" >&2; exit 1 ;;
esac
"#;

/// argv ログを読み込む (1 行 = 1 呼び出しの argv トークン列)。
fn read_args_log(dir: &Path, name: &str) -> Vec<Vec<String>> {
    fs::read_to_string(dir.join(name))
        .expect("argv ログが存在するはずです")
        .lines()
        .map(|line| line.split_whitespace().map(str::to_owned).collect())
        .collect()
}

/// ローカル bare リモートを作る。
fn bare_remote() -> TempDir {
    let bare = tempfile::tempdir().expect("bare リモート用ディレクトリを作成できる");
    assert!(
        support::git(bare.path(), &["init", "--bare", "."])
            .status
            .success()
    );
    bare
}

/// コマンド出力の stdout 部分を取り出す。
fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// アダプタとそのイベントバス。
struct Harness {
    adapter: ShellDeliveryAdapter,
    bus: Arc<event_bus::EventBus>,
}

/// fake bin を PATH へ載せた上でアダプタを生成する。
///
/// 返るガードはアダプタの全呼び出しが終わるまで生かすこと。
fn harness(repo_root: &Path, fake_bin: &Path) -> (EnvGuard, Harness) {
    let path = path_guard(fake_bin);
    let bus = Arc::new(event_bus::EventBus::new(256));
    let adapter = ShellDeliveryAdapter::new(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
        repo_root.to_path_buf(),
        "turtton/evorch".to_string(),
        "main".to_string(),
    );
    (path, Harness { adapter, bus })
}

/// 指定件数のツールイベントを受信する。
///
/// 受信者はアダプタ呼び出しの前に `bus.subscribe()` しておくこと (broadcast は
/// 過去イベントを再生しない)。
async fn recv_tool_events(receiver: &mut event_bus::EventReceiver, count: usize) -> Vec<ToolEvent> {
    let mut events = Vec::new();
    while events.len() < count {
        let event = timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("イベントがタイムアウトしません")
            .expect("受信者は生きているはずです");
        if let EventKind::Tool(tool_event) = event.kind {
            events.push(tool_event);
        }
    }
    events
}

// Given: デリバラブルブランチとローカル bare リモート / When: push して find_or_create_pr / Then:
// push がリモートへ届き、PR 証跡が返り、全ツールイベントに run_id が stamped される
#[tokio::test]
async fn push_then_find_or_create_pr_records_tool_events_with_run_id() {
    let _env = ENV_LOCK.lock().await;
    let (_temp, repo) = support::init_git_repo();
    let remote = bare_remote();
    assert!(
        support::git(
            &repo,
            &["remote", "add", "origin", remote.path().to_str().unwrap()]
        )
        .status
        .success()
    );
    assert!(
        support::git(&repo, &["checkout", "-b", "evorch/goal-1"])
            .status
            .success()
    );
    fs::write(repo.join("goal.txt"), "goal work\n").expect("ブランチの変更を書き込める");
    assert!(support::git(&repo, &["add", "goal.txt"]).status.success());
    assert!(
        support::git(&repo, &["commit", "-m", "goal work"])
            .status
            .success()
    );
    let head = stdout_of(&support::git(&repo, &["rev-parse", "HEAD"]));

    let fake = TempDir::new().expect("fake bin ディレクトリを作成できる");
    write_script(fake.path(), "gh", GH_SCRIPT);
    write_file(fake.path(), "pr_list.json", "[]");
    write_file(
        fake.path(),
        "pr_view.json",
        &format!(
            "{{\"number\":101,\"url\":\"https://github.com/turtton/evorch/pull/101\",\
             \"headRefOid\":\"{head}\",\"baseRefName\":\"main\"}}"
        ),
    );
    let (_path, harness) = harness(&repo, fake.path());
    let mut receiver = harness.bus.subscribe();

    harness
        .adapter
        .push_branch("evorch/goal-1")
        .await
        .expect("push は成功するはずです");
    let evidence = harness
        .adapter
        .find_or_create_pr("evorch/goal-1", "main", "goal: goal-1", "body")
        .await
        .expect("PR 作成は成功するはずです");

    assert_eq!(
        evidence,
        GateEvidence::PullRequest {
            repo: "turtton/evorch".to_string(),
            number: 101,
            url: "https://github.com/turtton/evorch/pull/101".to_string(),
            base_ref: "main".to_string(),
            head_sha: head,
        }
    );

    let ls_remote = stdout_of(&support::git(
        &repo,
        &["ls-remote", "--heads", remote.path().to_str().unwrap()],
    ));
    assert!(
        ls_remote.contains("refs/heads/evorch/goal-1"),
        "push が bare リモートへ届いているはずです: {ls_remote}"
    );

    // push 1 回 + list / create / view の 3 回 = 4 コマンドぶんのツールイベント
    let events = recv_tool_events(&mut receiver, 8).await;
    let started = events
        .iter()
        .filter(|event| matches!(event, ToolEvent::ToolStarted { .. }))
        .count();
    assert_eq!(started, 4, "ToolStarted は 4 回: {events:?}");
    for event in &events {
        let run_id = match event {
            ToolEvent::ToolStarted { run_id, .. } | ToolEvent::ToolCompleted { run_id, .. } => {
                run_id
            }
            _ => continue,
        };
        assert!(
            run_id.as_deref().is_some_and(|run_id| !run_id.is_empty()),
            "ツールイベントには run_id が stamp されているはずです: {event:?}"
        );
    }
}

// Given: statusCheckRollup の canned 応答 / When: ci_status / Then: rollup が CiState へ写像される
#[tokio::test]
async fn ci_status_maps_rollup_to_ci_state() {
    let _env = ENV_LOCK.lock().await;
    let (_temp, repo) = support::init_git_repo();
    let head = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

    let fake = TempDir::new().expect("fake bin ディレクトリを作成できる");
    write_script(fake.path(), "gh", GH_SCRIPT);
    write_file(
        fake.path(),
        "pr_list.json",
        &format!("[{{\"number\":101,\"headRefOid\":\"{head}\"}}]"),
    );
    let (_path, harness) = harness(&repo, fake.path());

    // すべて成功 → Green
    write_file(
        fake.path(),
        "pr_view.json",
        r#"{"statusCheckRollup":[
            {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"StatusContext","name":"lint","state":"SUCCESS"}]}"#,
    );
    assert_eq!(
        harness.adapter.ci_status("turtton/evorch", head).await,
        Ok(GateEvidence::Ci {
            head_sha: head.to_string(),
            state: CiState::Green,
        })
    );

    // 失敗 1 件 → Failing (概要に検査名)
    write_file(
        fake.path(),
        "pr_view.json",
        r#"{"statusCheckRollup":[
            {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"FAILURE"},
            {"__typename":"StatusContext","name":"lint","state":"PENDING"}]}"#,
    );
    assert_eq!(
        harness.adapter.ci_status("turtton/evorch", head).await,
        Ok(GateEvidence::Ci {
            head_sha: head.to_string(),
            state: CiState::Failing {
                summary: "test".to_string(),
            },
        })
    );

    // 未完了のみ → Pending
    write_file(
        fake.path(),
        "pr_view.json",
        r#"{"statusCheckRollup":[
            {"__typename":"CheckRun","name":"build","status":"IN_PROGRESS"}]}"#,
    );
    assert_eq!(
        harness.adapter.ci_status("turtton/evorch", head).await,
        Ok(GateEvidence::Ci {
            head_sha: head.to_string(),
            state: CiState::Pending,
        })
    );

    // rollup が空 → まだ何も報告されていないので Pending
    write_file(fake.path(), "pr_view.json", r#"{"statusCheckRollup":[]}"#);
    assert_eq!(
        harness.adapter.ci_status("turtton/evorch", head).await,
        Ok(GateEvidence::Ci {
            head_sha: head.to_string(),
            state: CiState::Pending,
        })
    );
}

// Given: fake intent-cli / When: closeout 3 ステップを実行 / Then:
// worker の claim / result-summary / complete のみが呼ばれ、queue 系は一切呼ばれない
#[tokio::test]
async fn closeout_step_invokes_intent_cli_worker_subcommands_only() {
    let _env = ENV_LOCK.lock().await;
    let (_temp, repo) = support::init_git_repo();

    let fake = TempDir::new().expect("fake bin ディレクトリを作成できる");
    write_script(fake.path(), "intent-cli", INTENT_CLI_SCRIPT);
    write_file(
        fake.path(),
        "result_summary.json",
        r#"{"artifact_ref":"runs/2026-09-05/T-G1"}"#,
    );
    let (_path, harness) = harness(&repo, fake.path());

    let claimed = harness
        .adapter
        .closeout_step("T-G1", CloseoutStep::WorkerClaim)
        .await
        .expect("claim は成功するはずです");
    let summarized = harness
        .adapter
        .closeout_step("T-G1", CloseoutStep::ResultSummary)
        .await
        .expect("result-summary は成功するはずです");
    let completed = harness
        .adapter
        .closeout_step("T-G1", CloseoutStep::WorkerComplete)
        .await
        .expect("complete は成功するはずです");

    assert_eq!(claimed, None);
    assert_eq!(summarized, Some("runs/2026-09-05/T-G1".to_string()));
    assert_eq!(completed, None);

    let log = read_args_log(fake.path(), "intent_cli_args.log");
    let expected: &[&[&str]] = &[
        &["worker", "claim", "--goal", "T-G1"],
        &["worker", "result-summary", "--goal", "T-G1"],
        &["worker", "complete", "--goal", "T-G1"],
    ];
    let expected: Vec<Vec<String>> = expected
        .iter()
        .map(|argv| argv.iter().map(|token| token.to_string()).collect())
        .collect();
    assert_eq!(log, expected);
    for token in ["queue", "automation", "issue", "packet", "publish", "run"] {
        assert!(
            !log.iter().flatten().any(|arg| arg == token),
            "closeout は {token} 系サブコマンドを呼んではいけません: {log:?}"
        );
    }
}

// Given: 親環境の GH_TOKEN / GH_CONFIG_DIR / When: アダプタを構築して gh を実行 / Then:
// 設定済みの変数のみが子プロセスへ転送され、未設定なら転送されない (Clarification A)
#[tokio::test]
async fn delivery_adapter_forwards_gh_token_and_config_dir_only_when_present() {
    let _env = ENV_LOCK.lock().await;
    let (_temp, repo) = support::init_git_repo();
    let head = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

    // fake gh は全呼び出しで環境プローブをログへ残す。
    let fake = TempDir::new().expect("fake bin ディレクトリを作成できる");
    write_script(
        fake.path(),
        "gh",
        r#"DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf 'token=[%s] config=[%s]\n' "$GH_TOKEN" "$GH_CONFIG_DIR" >> "$DIR/env_probe.log"
case "$1 $2" in
  "pr list") cat "$DIR/pr_list.json" ;;
  "pr view") cat "$DIR/pr_view.json" ;;
  *) echo "unexpected gh invocation: $*" >&2; exit 1 ;;
esac
"#,
    );
    write_file(
        fake.path(),
        "pr_list.json",
        &format!("[{{\"number\":101,\"headRefOid\":\"{head}\"}}]"),
    );
    write_file(fake.path(), "pr_view.json", r#"{"statusCheckRollup":[]}"#);

    // ケース 1: 認証変数をクリーンにした上で GH_TOKEN / GH_CONFIG_DIR を設定 → 転送される
    let clean = EnvGuard::remove(&["GH_TOKEN", "GITHUB_TOKEN", "GIT_ASKPASS", "GH_CONFIG_DIR"]);
    let present = EnvGuard::set(&[
        ("GH_TOKEN", "token-123".to_string()),
        ("GH_CONFIG_DIR", "/tmp/fake-gh-config".to_string()),
    ]);
    let (_path, delivery) = harness(&repo, fake.path());
    delivery
        .adapter
        .ci_status("turtton/evorch", head)
        .await
        .expect("ci_status は成功するはずです");
    let probe = fs::read_to_string(fake.path().join("env_probe.log"))
        .expect("環境プローブが記録されているはずです");
    assert!(
        probe.contains("token=[token-123] config=[/tmp/fake-gh-config]"),
        "設定済みの認証変数のみが子プロセスへ転送されるはずです: {probe}"
    );
    drop(present);
    drop(clean);

    // ケース 2: 認証変数が全く無い状態で構築 → 何も転送されない
    let clean = EnvGuard::remove(&["GH_TOKEN", "GITHUB_TOKEN", "GIT_ASKPASS", "GH_CONFIG_DIR"]);
    let (_path, delivery) = harness(&repo, fake.path());
    delivery
        .adapter
        .ci_status("turtton/evorch", head)
        .await
        .expect("ci_status は成功するはずです");
    let probe = fs::read_to_string(fake.path().join("env_probe.log"))
        .expect("環境プローブが記録されているはずです");
    assert!(
        probe.contains("token=[] config=[]"),
        "未設定の認証変数は子プロセスへ転送されないはずです: {probe}"
    );
    drop(clean);
}
