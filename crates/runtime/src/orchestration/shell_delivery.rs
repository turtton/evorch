//! 承認済み shell ツール経由で配信操作を実行する [`ShellDeliveryAdapter`] (AC7)。
//!
//! [`DeliveryPort`] を [`tools::Shell`] ツール越しに実装する。配信系コマンドは
//! [`ShellCommandContract::delivery`] の allowlist に、merge は
//! [`ShellCommandContract::merge_only`] に制限され、契約が拒否したコマンドは
//! [`OrchestratorEvent::ShellCommandDenied`] としてバスへ記録された上で
//! [`DeliveryError::Command`] になる。ツール実行は [`ToolExecutor`] を経由するため、
//! `ToolStarted` / `ToolCompleted` イベントには run コンテキストの run_id が
//! stamp されてバスへ流れる。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use event_bus::{CiState, CloseoutStep, Event, EventBus, GateEvidence, OrchestratorEvent};
use sandbox::Sandbox;
use serde::Deserialize;
use tools::{CommandVerdict, Shell, ShellCommandContract, ToolExecutionContext, ToolExecutor};

use super::delivery::{DeliveryError, DeliveryPort};
use super::types::ApprovedMerge;

/// delivery 系実行 (`git` / `gh` / `intent-cli`) の run コンテキスト識別子。
const DELIVERY_RUN_ID: &str = "delivery";

/// merge 専用実行 (`gh pr merge`) の run コンテキスト識別子。
const MERGE_RUN_ID: &str = "merge";

/// 親環境から子プロセスへ転送すべき認証系環境変数 (計画 Clarification A)。
const CREDENTIAL_KEYS: [&str; 4] = ["GH_TOKEN", "GITHUB_TOKEN", "GIT_ASKPASS", "GH_CONFIG_DIR"];

/// 親環境に存在する認証系環境変数のみを収集する。
fn credential_env() -> Vec<(String, String)> {
    CREDENTIAL_KEYS
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect()
}

/// shell ツール結果本文から stdout セクションを抽出する。
///
/// `Shell::execute` は `exit_code: N\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}`
/// 形式で返す。形式が契約どおりでない場合は全文を返す。
fn stdout_section(content: &str) -> &str {
    const STDOUT_MARK: &str = "--- stdout ---";
    const STDERR_MARK: &str = "--- stderr ---";
    let Some((_, rest)) = content.split_once(STDOUT_MARK) else {
        return content;
    };
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    match rest.split_once(STDERR_MARK) {
        Some((stdout, _)) => stdout.trim_end_matches('\n'),
        None => rest,
    }
}

/// JSON 出力を解析する。契約した形状に反した場合は [`DeliveryError::Protocol`]。
fn parse_json<T: for<'de> Deserialize<'de>>(
    output: &str,
    context: &str,
) -> Result<T, DeliveryError> {
    serde_json::from_str(output.trim())
        .map_err(|error| DeliveryError::Protocol(format!("{context}: {error}")))
}

/// PR の `gh --json` 応答 (`number,url,headRefOid,baseRefName`)。
#[derive(Deserialize)]
struct PrFields {
    number: u64,
    url: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
}

/// CI 観測用の PR 探索応答 (`number,headRefOid`)。
#[derive(Deserialize)]
struct PrHead {
    number: u64,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
}

/// `gh pr view --json statusCheckRollup` 応答。
#[derive(Deserialize)]
struct Rollup {
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Vec<RollupEntry>,
}

/// statusCheckRollup の 1 エントリ (CheckRun / StatusContext の共用形状)。
#[derive(Deserialize)]
struct RollupEntry {
    name: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
}

/// `intent-cli worker result-summary` 応答。
#[derive(Deserialize)]
struct ResultSummary {
    artifact_ref: Option<String>,
}

enum CheckOutcome {
    Passed,
    Pending,
    Failed,
}

fn check_outcome(entry: &RollupEntry) -> CheckOutcome {
    if let Some(status) = entry.status.as_deref() {
        if status != "COMPLETED" {
            return CheckOutcome::Pending;
        }
        return match entry.conclusion.as_deref() {
            Some("SUCCESS" | "SKIPPED" | "NEUTRAL") => CheckOutcome::Passed,
            Some("FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "CANCELLED") => CheckOutcome::Failed,
            _ => CheckOutcome::Pending,
        };
    }
    if let Some(state) = entry.state.as_deref() {
        return match state {
            "SUCCESS" => CheckOutcome::Passed,
            "FAILURE" | "ERROR" => CheckOutcome::Failed,
            _ => CheckOutcome::Pending,
        };
    }
    CheckOutcome::Pending
}

/// statusCheckRollup を [`CiState`] へ写像する。
///
/// 失敗があれば概要に検査名を並べ、1 件も確定していない (rollup が空を含む)
/// 場合は [`CiState::Pending`]、すべて確定かつ成功なら [`CiState::Green`]。
fn ci_state(entries: &[RollupEntry]) -> CiState {
    let mut failing: Vec<&str> = Vec::new();
    let mut pending = false;
    for entry in entries {
        match check_outcome(entry) {
            CheckOutcome::Passed => {}
            CheckOutcome::Pending => pending = true,
            CheckOutcome::Failed => {
                failing.push(entry.name.as_deref().unwrap_or("unnamed check"));
            }
        }
    }
    if !failing.is_empty() {
        return CiState::Failing {
            summary: failing.join(", "),
        };
    }
    if pending || entries.is_empty() {
        return CiState::Pending;
    }
    CiState::Green
}

fn pull_request_evidence(repo: &str, entry: PrFields) -> GateEvidence {
    GateEvidence::PullRequest {
        repo: repo.to_owned(),
        number: entry.number,
        url: entry.url,
        base_ref: entry.base_ref_name,
        head_sha: entry.head_ref_oid,
    }
}

/// 承認済み shell ツール経由の [`DeliveryPort`] 実装。
///
/// 構築時に親環境から認証系環境変数を収集し、delivery 用 executor へのみ
/// 転送する。production の bwrap サンドボックス (credential ro-bind 構成) は
/// T3.1 の composition root で組み立てるため、本アダプタは注入された
/// [`Sandbox`] をそのまま使う。
pub struct ShellDeliveryAdapter {
    bus: Arc<EventBus>,
    delivery_executor: ToolExecutor,
    merge_executor: ToolExecutor,
    delivery_contract: ShellCommandContract,
    merge_contract: ShellCommandContract,
    repo_root: PathBuf,
    repo: String,
    base_ref: String,
    delivery_ctx: ToolExecutionContext,
    merge_ctx: ToolExecutionContext,
    next_call: AtomicU64,
}

impl ShellDeliveryAdapter {
    /// 配信アダプタを生成する。
    ///
    /// delivery 用 (`git` / `gh` / `intent-cli`) と merge 用 (`gh pr merge`) の
    /// 2 つの [`ToolExecutor`] を構築する。merge 用は認証変数を転送しない
    /// (`gh pr merge` はホスト側 `gh` の認証設定に依存しないため)。
    pub fn new(
        bus: Arc<EventBus>,
        sandbox: Arc<dyn Sandbox>,
        repo_root: PathBuf,
        repo: String,
        base_ref: String,
    ) -> Self {
        let delivery_shell = Shell::with_contract_and_env(
            Arc::clone(&sandbox),
            ShellCommandContract::delivery(),
            credential_env(),
        );
        let mut delivery_executor = ToolExecutor::new(Arc::clone(&bus));
        delivery_executor
            .register(Arc::new(delivery_shell))
            // SAFE-EXPECT: shell ツールのスキーマは all_standard_tool_schemas_compile で
            // コンパイル可能を検証済み。
            .expect("shell ツールのスキーマはコンパイル可能を検証済み");
        let merge_shell = Shell::with_contract(sandbox, ShellCommandContract::merge_only());
        let mut merge_executor = ToolExecutor::new(Arc::clone(&bus));
        merge_executor
            .register(Arc::new(merge_shell))
            // SAFE-EXPECT: 同上。
            .expect("shell ツールのスキーマはコンパイル可能を検証済み");
        Self {
            bus,
            delivery_executor,
            merge_executor,
            delivery_contract: ShellCommandContract::delivery(),
            merge_contract: ShellCommandContract::merge_only(),
            repo_root,
            repo,
            base_ref,
            delivery_ctx: ToolExecutionContext {
                run_id: DELIVERY_RUN_ID.to_owned(),
            },
            merge_ctx: ToolExecutionContext {
                run_id: MERGE_RUN_ID.to_owned(),
            },
            next_call: AtomicU64::new(0),
        }
    }

    fn deny(
        &self,
        ctx: &ToolExecutionContext,
        program: &str,
        args: &[String],
        reason: &str,
    ) -> DeliveryError {
        self.bus
            .emit(Event::new(OrchestratorEvent::ShellCommandDenied {
                goal_id: None,
                run_id: Some(ctx.run_id.clone()),
                program: program.to_owned(),
                args: args.to_vec(),
                reason: reason.to_owned(),
            }));
        DeliveryError::Command(reason.to_owned())
    }

    async fn run(
        &self,
        executor: &ToolExecutor,
        ctx: &ToolExecutionContext,
        contract: &ShellCommandContract,
        program: &str,
        args: &[&str],
    ) -> Result<String, DeliveryError> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        if let CommandVerdict::Deny { reason } = contract.evaluate(program, &args) {
            return Err(self.deny(ctx, program, &args, &reason));
        }
        let call_id = format!(
            "{}-{}",
            ctx.run_id,
            self.next_call.fetch_add(1, Ordering::Relaxed)
        );
        let payload = serde_json::json!({
            "command": program,
            "args": args,
            "cwd": self.repo_root.display().to_string(),
        });
        let result = executor
            .execute(ctx, "shell", &call_id, payload)
            .await
            .map_err(|error| DeliveryError::Command(error.to_string()))?;
        if result.is_error {
            return Err(DeliveryError::Command(result.content));
        }
        Ok(stdout_section(&result.content).to_owned())
    }

    async fn run_delivery(&self, program: &str, args: &[&str]) -> Result<String, DeliveryError> {
        self.run(
            &self.delivery_executor,
            &self.delivery_ctx,
            &self.delivery_contract,
            program,
            args,
        )
        .await
    }

    async fn run_merge(&self, program: &str, args: &[&str]) -> Result<String, DeliveryError> {
        self.run(
            &self.merge_executor,
            &self.merge_ctx,
            &self.merge_contract,
            program,
            args,
        )
        .await
    }
}

#[async_trait]
impl DeliveryPort for ShellDeliveryAdapter {
    async fn push_branch(&self, branch: &str) -> Result<(), DeliveryError> {
        self.run_delivery("git", &["push", "-u", "origin", branch])
            .await
            .map(|_| ())
    }

    async fn find_or_create_pr(
        &self,
        branch: &str,
        base_ref: &str,
        title: &str,
        body: &str,
    ) -> Result<GateEvidence, DeliveryError> {
        let list_args = [
            "pr",
            "list",
            "--repo",
            &self.repo,
            "--head",
            branch,
            "--base",
            &self.base_ref,
            "--json",
            "number,url,headRefOid,baseRefName",
        ];
        let list_json = self.run_delivery("gh", &list_args).await?;
        if let Some(entry) = parse_json::<Vec<PrFields>>(&list_json, "gh pr list output")?
            .into_iter()
            .next()
        {
            return Ok(pull_request_evidence(&self.repo, entry));
        }
        self.run_delivery(
            "gh",
            &[
                "pr", "create", "--repo", &self.repo, "--head", branch, "--base", base_ref,
                "--title", title, "--body", body,
            ],
        )
        .await?;
        let view_json = self
            .run_delivery(
                "gh",
                &[
                    "pr",
                    "view",
                    branch,
                    "--repo",
                    &self.repo,
                    "--json",
                    "number,url,headRefOid,baseRefName",
                ],
            )
            .await?;
        let entry: PrFields = parse_json(&view_json, "gh pr view output")?;
        Ok(pull_request_evidence(&self.repo, entry))
    }

    async fn pr_status(&self, repo: &str, number: u64) -> Result<GateEvidence, DeliveryError> {
        let number = number.to_string();
        let view_json = self
            .run_delivery(
                "gh",
                &[
                    "pr",
                    "view",
                    &number,
                    "--repo",
                    repo,
                    "--json",
                    "number,url,headRefOid,baseRefName",
                ],
            )
            .await?;
        let entry: PrFields = parse_json(&view_json, "gh pr view output")?;
        Ok(pull_request_evidence(repo, entry))
    }

    async fn ci_status(&self, repo: &str, head_sha: &str) -> Result<GateEvidence, DeliveryError> {
        let list_json = self
            .run_delivery(
                "gh",
                &["pr", "list", "--repo", repo, "--json", "number,headRefOid"],
            )
            .await?;
        let number = parse_json::<Vec<PrHead>>(&list_json, "gh pr list output")?
            .into_iter()
            .find(|entry| entry.head_ref_oid == head_sha)
            .map(|entry| entry.number.to_string())
            .ok_or_else(|| {
                DeliveryError::Protocol(format!("no pull request with head {head_sha}"))
            })?;
        let view_json = self
            .run_delivery(
                "gh",
                &[
                    "pr",
                    "view",
                    &number,
                    "--repo",
                    repo,
                    "--json",
                    "statusCheckRollup",
                ],
            )
            .await?;
        let rollup: Rollup = parse_json(&view_json, "gh pr view output")?;
        Ok(GateEvidence::Ci {
            head_sha: head_sha.to_owned(),
            state: ci_state(&rollup.status_check_rollup),
        })
    }

    async fn merge_pr(&self, approved: &ApprovedMerge) -> Result<String, DeliveryError> {
        let binding = &approved.binding;
        let number = binding.pr_number.to_string();
        self.run_merge(
            "gh",
            &[
                "pr",
                "merge",
                &number,
                "--repo",
                &binding.repo,
                "--squash",
                "--match-head-commit",
                &binding.head_sha,
            ],
        )
        .await
    }

    async fn closeout_step(
        &self,
        goal_id: &str,
        step: CloseoutStep,
    ) -> Result<Option<String>, DeliveryError> {
        let subcommand = match step {
            CloseoutStep::WorkerClaim => "claim",
            CloseoutStep::ResultSummary => "result-summary",
            CloseoutStep::WorkerComplete => "complete",
        };
        let output = self
            .run_delivery("intent-cli", &["worker", subcommand, "--goal", goal_id])
            .await?;
        match step {
            CloseoutStep::ResultSummary => {
                let artifact: ResultSummary =
                    parse_json(&output, "intent-cli worker result-summary output")?;
                Ok(artifact.artifact_ref)
            }
            CloseoutStep::WorkerClaim | CloseoutStep::WorkerComplete => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{EventKind, GateSnapshot, MergeBinding};
    use sandbox::DirectSandbox;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    use tokio::time::timeout;

    /// 環境変数を差し替えるテストを直列化するロック。
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// 差し替えた環境変数をスコープ終了で復元するガード。
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: String) -> Self {
            let saved = vec![(key, std::env::var(key).ok())];
            // SAFETY: ENV_LOCK で直列化された区間でのみ呼ばれ、他のテストは
            // 環境変数を並行して読み書きしない。
            unsafe { std::env::set_var(key, value) };
            Self { saved }
        }

        fn restore(&mut self) {
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

    /// 受信 argv を 1 行ずつログへ残し、定形応答を返す fake gh。
    const GH_SCRIPT: &str = r#"DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$*" >> "$DIR/gh_args.log"
echo "squashed and merged"
"#;

    fn write_gh_script(dir: &TempDir) {
        let path = dir.path().join("gh");
        fs::write(&path, format!("#!/bin/sh\n{GH_SCRIPT}")).expect("スクリプトを書き込める");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("実行権を付けられる");
    }

    fn argv_log(dir: &TempDir) -> Vec<Vec<String>> {
        fs::read_to_string(dir.path().join("gh_args.log"))
            .expect("argv ログが存在するはずです")
            .lines()
            .map(|line| line.split_whitespace().map(str::to_owned).collect())
            .collect()
    }

    /// 承認済みバインディングを構築する (crate 内部テストのみ許可)。
    fn approved_merge(head_sha: &str) -> ApprovedMerge {
        ApprovedMerge {
            binding: MergeBinding {
                token_id: "token-1".to_owned(),
                repo: "turtton/evorch".to_owned(),
                pr_number: 101,
                head_sha: head_sha.to_owned(),
                snapshot: GateSnapshot {
                    repo: "turtton/evorch".to_owned(),
                    pr_number: 101,
                    base_ref: "main".to_owned(),
                    head_sha: head_sha.to_owned(),
                    ci: CiState::Green,
                    criteria_round: 1,
                    review_round: 1,
                    reviewer_run_id: "review-1".to_owned(),
                },
            },
        }
    }

    // Given: 40-hex の head を持つ承認済みバインディング / When: merge_pr / Then:
    // merge_only 契約の argv 形状 (--match-head-commit を含む) で gh が呼ばれる
    #[tokio::test]
    async fn merge_pr_passes_match_head_commit_from_approved_binding() {
        let _env = ENV_LOCK.lock().await;
        let head_sha = "a".repeat(40);
        let fake = TempDir::new().expect("fake bin ディレクトリを作成できる");
        write_gh_script(&fake);
        let mut guard = EnvGuard::set(
            "PATH",
            format!(
                "{}:{}",
                fake.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );

        let bus = Arc::new(EventBus::new(64));
        let repo_root = tempfile::tempdir().expect("リポジトリ用ディレクトリを作成できる");
        let adapter = ShellDeliveryAdapter::new(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
            repo_root.path().to_path_buf(),
            "turtton/evorch".to_owned(),
            "main".to_owned(),
        );

        let detail = adapter
            .merge_pr(&approved_merge(&head_sha))
            .await
            .expect("merge は成功するはずです");

        assert_eq!(detail, "squashed and merged");
        guard.restore();
        assert_eq!(
            argv_log(&fake),
            vec![vec![
                "pr".to_owned(),
                "merge".to_owned(),
                "101".to_owned(),
                "--repo".to_owned(),
                "turtton/evorch".to_owned(),
                "--squash".to_owned(),
                "--match-head-commit".to_owned(),
                head_sha,
            ]]
        );
    }

    // Given: head が 40-hex でない承認済みバインディング / When: merge_pr / Then:
    // 契約が拒否し、DeliveryError と ShellCommandDenied オーケストレータイベントが記録される
    #[tokio::test]
    async fn denied_command_yields_delivery_error_and_shell_command_denied_event() {
        let _env = ENV_LOCK.lock().await;
        let bus = Arc::new(EventBus::new(64));
        let mut receiver = bus.subscribe();
        let repo_root = tempfile::tempdir().expect("リポジトリ用ディレクトリを作成できる");
        let adapter = ShellDeliveryAdapter::new(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
            repo_root.path().to_path_buf(),
            "turtton/evorch".to_owned(),
            "main".to_owned(),
        );

        let error = adapter
            .merge_pr(&approved_merge("not-a-40-hex-sha"))
            .await
            .expect_err("契約拒否はエラーになるはずです");

        assert!(matches!(error, DeliveryError::Command(_)));
        let event = timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("イベントがタイムアウトしません")
            .expect("受信者は生きているはずです");
        match event.kind {
            EventKind::Orchestrator(OrchestratorEvent::ShellCommandDenied {
                goal_id,
                run_id,
                program,
                args,
                reason,
            }) => {
                assert_eq!(goal_id, None);
                assert!(run_id.is_some_and(|run_id| !run_id.is_empty()));
                assert_eq!(program, "gh");
                assert!(args.contains(&"--match-head-commit".to_owned()));
                assert!(!reason.is_empty());
            }
            other => panic!("ShellCommandDenied 以外のイベントです: {other:?}"),
        }
    }
}
