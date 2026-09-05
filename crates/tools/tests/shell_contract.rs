//! shell コマンド契約（S9）の統合テスト。
//!
//! standard 契約はモデル起因の shell 実行から `gh pr merge` と intent-cli の
//! queue/automation/issue/packet/publish/run 系を拒否し、delivery / merge_only
//! 契約は supervisor 配信アダプタ専用の allowlist を提供する。拒否は ToolError
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

// Given: standard 契約 / When: gh pr merge を直接と /usr/bin/gh と sh -c 経由で評価 / Then: すべて拒否される
#[test]
fn standard_denies_gh_pr_merge_direct_and_via_sh_dash_c() {
    let contract = ShellCommandContract::standard();

    assert_denied(contract.evaluate("gh", &argv(&["pr", "merge", "123", "--repo", "o/r"])));
    assert_denied(contract.evaluate("/usr/bin/gh", &argv(&["pr", "merge", "123"])));
    assert_denied(contract.evaluate("sh", &argv(&["-c", "gh pr merge 123 --repo o/r"])));
    assert_denied(contract.evaluate("bash", &argv(&["-c", "cd repo && gh pr merge 123"])));
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

// Given: delivery 契約 / When: closeout と PR 操作の許可リスト内コマンドを評価 / Then: すべて許可される
#[test]
fn delivery_allows_exact_closeout_and_pr_commands() {
    let contract = ShellCommandContract::delivery();
    let allowed_commands: &[(&str, &[&str])] = &[
        ("git", &["push", "origin", "main"]),
        ("git", &["rev-parse", "HEAD"]),
        ("git", &["status", "--porcelain"]),
        ("git", &["log", "-1", "--format=%H"]),
        ("git", &["fetch", "origin"]),
        ("git", &["ls-remote", "origin"]),
        ("gh", &["auth", "status"]),
        ("gh", &["pr", "list"]),
        ("gh", &["pr", "view", "101", "--json", "headRefOid"]),
        ("gh", &["pr", "checks", "101"]),
        ("gh", &["pr", "create", "--title", "t"]),
        ("gh", &["pr", "edit", "101", "--title", "t"]),
        ("gh", &["pr", "comment", "101", "--body", "b"]),
        ("intent-cli", &["--help"]),
        ("intent-cli", &["worker", "--help"]),
        ("intent-cli", &["worker", "claim", "u1"]),
        ("intent-cli", &["worker", "result-summary", "u1"]),
        ("intent-cli", &["worker", "complete", "u1"]),
    ];

    for (program, args) in allowed_commands {
        assert_allowed(contract.evaluate(program, &argv(args)));
    }
}

// Given: delivery 契約 / When: ワークツリーを書き換える git コマンドやインタプリタを評価 / Then: すべて拒否される
#[test]
fn delivery_denies_worktree_mutation_commands() {
    let contract = ShellCommandContract::delivery();

    assert_denied(contract.evaluate("git", &argv(&["add", "-A"])));
    assert_denied(contract.evaluate("git", &argv(&["commit", "-m", "msg"])));
    assert_denied(contract.evaluate("git", &argv(&["checkout", "-b", "branch"])));
    assert_denied(contract.evaluate("git", &argv(&["reset", "--hard"])));
    assert_denied(contract.evaluate("sh", &argv(&["-c", "git add -A"])));
    assert_denied(contract.evaluate("sh", &argv(&["-c", "echo hi"])));
}

// Given: delivery 契約 / When: intent-cli の queue 系や gh pr merge を評価 / Then: すべて拒否される
#[test]
fn delivery_denies_intent_cli_queue_family() {
    let contract = ShellCommandContract::delivery();

    for sub in ["queue", "automation", "issue", "packet", "publish", "run"] {
        assert_denied(contract.evaluate("intent-cli", &argv(&[sub, "list"])));
    }
    assert_denied(contract.evaluate("intent-cli", &argv(&["queue", "seed"])));
    assert_denied(contract.evaluate(
        "intent-cli",
        &argv(&["automation", "queue-seed-from-packet"]),
    ));
    assert_denied(contract.evaluate("gh", &argv(&["pr", "merge", "123", "--repo", "o/r"])));
    assert_denied(contract.evaluate("gh", &argv(&["pr", "merge", "123"])));
}

// Given: merge_only 契約 / When: 正しい match-head-commit 形状と崩れた形状を評価 / Then: 正しい形状のみ許可される
#[test]
fn merge_only_allows_only_match_head_commit_shape() {
    let contract = ShellCommandContract::merge_only();
    let sha = "a".repeat(40);

    assert_allowed(contract.evaluate(
        "gh",
        &argv(&[
            "pr",
            "merge",
            "123",
            "--repo",
            "owner/repo",
            "--squash",
            "--match-head-commit",
            &sha,
        ]),
    ));

    // --match-head-commit 無し
    assert_denied(contract.evaluate(
        "gh",
        &argv(&["pr", "merge", "123", "--repo", "owner/repo", "--squash"]),
    ));
    // --squash と --match-head-commit の順序違い
    assert_denied(contract.evaluate(
        "gh",
        &argv(&[
            "pr",
            "merge",
            "123",
            "--repo",
            "owner/repo",
            "--match-head-commit",
            &sha,
            "--squash",
        ]),
    ));
    // PR 番号が非数字
    assert_denied(contract.evaluate(
        "gh",
        &argv(&[
            "pr",
            "merge",
            "abc",
            "--repo",
            "owner/repo",
            "--squash",
            "--match-head-commit",
            &sha,
        ]),
    ));
    // --repo が owner/repo 形状でない
    assert_denied(contract.evaluate(
        "gh",
        &argv(&[
            "pr",
            "merge",
            "123",
            "--repo",
            "owner",
            "--squash",
            "--match-head-commit",
            &sha,
        ]),
    ));
    // コミットハッシュが 39 文字
    assert_denied(contract.evaluate(
        "gh",
        &argv(&[
            "pr",
            "merge",
            "123",
            "--repo",
            "owner/repo",
            "--squash",
            "--match-head-commit",
            &"a".repeat(39),
        ]),
    ));
    // 余計な引数が付いている
    assert_denied(contract.evaluate(
        "gh",
        &argv(&[
            "pr",
            "merge",
            "123",
            "--repo",
            "owner/repo",
            "--squash",
            "--match-head-commit",
            &sha,
            "--admin",
        ]),
    ));
    // プログラムが gh 以外
    assert_denied(contract.evaluate("git", &argv(&["push", "origin", "main"])));
    assert_denied(contract.evaluate("sh", &argv(&["-c", "gh pr merge 123"])));
}

// Given: standard 契約を Shell へ注入 / When: gh pr merge を実行 / Then: ToolError ではなく is_error 付き ToolResult が返る
#[tokio::test]
async fn shell_tool_returns_tool_error_not_err_when_denied() {
    let shell = Shell::with_contract(
        Arc::new(DirectSandbox::new_unchecked()),
        ShellCommandContract::standard(),
    );

    let result = shell
        .execute(json!({
            "command": "gh",
            "args": ["pr", "merge", "123", "--repo", "o/r", "--squash"]
        }))
        .await
        .expect("拒否は ToolError ではなく ToolResult として返るはずです");

    assert!(result.is_error);
    assert!(result.content.contains("shell command denied by contract"));
}

// Given: Shell::new / When: gh pr merge を実行 / Then: 既定で standard 契約が適用され拒否される
#[tokio::test]
async fn shell_new_applies_standard_contract_by_default() {
    let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));

    let result = shell
        .execute(json!({
            "command": "sh",
            "args": ["-c", "gh pr merge 123 --repo o/r"]
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
            "command": "sh",
            "args": ["-c", "printf %s \"$CONTRACT_ENV_PROBE\""]
        }))
        .await
        .expect("実行に成功するはずです");

    assert!(!result.is_error);
    assert!(result.content.contains("forwarded"));
}
