//! shell コマンド契約（S9）の統合テスト。
//!
//! standard 契約はモデル起因の shell 実行から `gh issue create|edit|close` と intent-cli の
//! queue/automation/issue/packet/publish/run 系を拒否することを検証する。拒否は ToolError
//! ではなく `is_error: true` の ToolResult として返ることも検証する。

use std::sync::Arc;

use sandbox::DirectSandbox;
use serde_json::json;
use tools::{CommandVerdict, Shell, ShellCommandContract, Tool};

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

// Given: 許可されるべき評価結果 / When: verdict を検査 / Then: Allow であれば通過し Deny なら失敗する
fn assert_allowed(verdict: CommandVerdict) {
    match verdict {
        CommandVerdict::Allow => {}
        CommandVerdict::Deny { reason } => {
            panic!("許可されるべきコマンドが拒否されました: {reason}")
        }
    }
}

// Given: 拒否されるべき評価結果 / When: verdict を検査 / Then: 空でない reason 付きの Deny である
fn assert_denied(verdict: CommandVerdict) {
    match verdict {
        CommandVerdict::Allow => panic!("拒否されるべきコマンドが許可されました"),
        CommandVerdict::Deny { reason } => assert!(!reason.is_empty()),
    }
}

// Given: standard 契約 / When: gh pr merge を直接とインタプリタ経由で評価 / Then: 通常の sandbox 経路へ進める
#[test]
fn standard_allows_gh_pr_merge_direct_and_via_interpreters() {
    let contract = ShellCommandContract::standard();

    assert_allowed(contract.evaluate("gh", &argv(&["pr", "merge", "123", "--repo", "o/r"])));
    assert_allowed(contract.evaluate("/usr/bin/gh", &argv(&["pr", "merge", "123"])));
    for interpreter in ["sh", "bash", "zsh", "dash"] {
        assert_allowed(
            contract.evaluate(interpreter, &argv(&["-c", "gh pr merge 123 --repo o/r"])),
        );
        assert_allowed(
            contract.evaluate(interpreter, &argv(&["-c", "cd repo && gh pr merge 123"])),
        );
    }
}

// Given: standard 契約 / When: intent-cli の queue 系サブコマンドを直接と sh -c 経由で評価 / Then: すべて拒否される
#[test]
fn standard_denies_intent_cli_queue_seed_publish_issue_automation_packet() {
    let contract = ShellCommandContract::standard();

    for sub in ["queue", "automation", "issue", "packet", "publish", "run"] {
        assert_denied(contract.evaluate("intent-cli", &argv(&[sub, "list"])));
    }
    // seed 系は queue / automation 経由の呼び出しとしても拒否される
    assert_denied(contract.evaluate("intent-cli", &argv(&["queue", "seed"])));
    assert_denied(contract.evaluate(
        "intent-cli",
        &argv(&[
            "automation",
            "queue-seed-from-packet",
            "--execution-unit",
            "u",
        ]),
    ));
    assert_denied(contract.evaluate(
        "sh",
        &argv(&["-c", "intent-cli automation queue-seed-from-packet"]),
    ));
}

// Given: standard 契約 / When: git commit / cargo / gh issue list を評価 / Then: すべて許可される
#[test]
fn standard_allows_git_commit_and_cargo() {
    let contract = ShellCommandContract::standard();

    assert_allowed(contract.evaluate("git", &argv(&["add", "-A"])));
    assert_allowed(contract.evaluate("git", &argv(&["commit", "-m", "msg"])));
    assert_allowed(contract.evaluate("cargo", &argv(&["test", "-p", "tools"])));
    assert_allowed(contract.evaluate("gh", &argv(&["issue", "list"])));
    assert_allowed(contract.evaluate("sh", &argv(&["-c", "git commit -m msg && cargo test"])));
}

// Given: standard 契約を Shell へ注入 / When: issue mutation を実行 / Then: ToolError ではなく is_error 付き ToolResult が返る
#[tokio::test]
async fn shell_tool_returns_tool_error_not_err_when_denied() {
    let shell = Shell::with_contract(
        Arc::new(DirectSandbox::new_unchecked()),
        ShellCommandContract::standard(),
    );

    let result = shell
        .execute(json!({
            "command": "gh",
            "args": ["issue", "close", "123", "--repo", "o/r"]
        }))
        .await
        .expect("拒否は ToolError ではなく ToolResult として返るはずです");

    assert!(result.is_error);
    assert!(result.content.contains("shell command denied by contract"));
}

// Given: Shell::new / When: intent-cli queue を実行 / Then: 既定で standard 契約が適用され拒否される
#[tokio::test]
async fn shell_new_applies_standard_contract_by_default() {
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));

    let result = shell
        .execute(json!({
            "command": "sh",
            "args": ["-c", "intent-cli queue list"]
        }))
        .await
        .expect("拒否は ToolError ではなく ToolResult として返るはずです");

    assert!(result.is_error);
    assert!(result.content.contains("shell command denied by contract"));
}

// Given: with_contract_and_env で追加環境を注入 / When: 子プロセスで環境変数を読む / Then: 値が子へ渡る
#[tokio::test]
async fn with_contract_and_env_forwards_extra_env_to_child() {
    let shell = Shell::with_contract_and_env(
        Arc::new(DirectSandbox::new_unchecked()),
        ShellCommandContract::standard(),
        vec![("CONTRACT_ENV_PROBE".to_string(), "forwarded".to_string())],
    );

    let result = shell
        .execute(json!({
            "command": "printf %s \"$CONTRACT_ENV_PROBE\""
        }))
        .await
        .expect("実行に成功するはずです");

    assert!(!result.is_error);
    assert!(result.content.contains("forwarded"));
}
