//! ツールの抽象と権限モデルを定義します。

use crate::error::ToolError;
use crate::executor::ToolExecutionContext;
use crate::result::ToolResult;

/// ツールが要求する権限の集合。
///
/// 各フラグはツールがその種類のリソースへアクセスし得ることを示し、実行環境は
/// この宣言に基づいて許可判定を行う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// ファイルシステムの読み取り。
    pub fs_read: bool,
    /// ファイルシステムの書き込み。
    pub fs_write: bool,
    /// プロセスの起動。
    pub process_spawn: bool,
    /// ネットワークアクセス。
    pub network: bool,
}

impl Permissions {
    /// 読み取り専用の権限（`fs_read` のみ `true`）。
    pub const fn read_only() -> Self {
        Self {
            fs_read: true,
            fs_write: false,
            process_spawn: false,
            network: false,
        }
    }

    /// 読み書きの権限（`fs_read` と `fs_write` が `true`）。
    pub const fn read_write() -> Self {
        Self {
            fs_read: true,
            fs_write: true,
            process_spawn: false,
            network: false,
        }
    }

    /// プロセス起動を含む全権限（ローカルリソースの 3 フラグすべて `true`）。
    pub const fn process() -> Self {
        Self {
            fs_read: true,
            fs_write: true,
            process_spawn: true,
            network: false,
        }
    }

    /// ネットワークアクセスのみの権限（`network` のみ `true`）。
    pub const fn network() -> Self {
        Self {
            fs_read: false,
            fs_write: false,
            process_spawn: false,
            network: true,
        }
    }
}

/// A tool name reserved for one implementation in this crate.
///
/// Models are trained on the exact behavior of these tools, so
/// [`crate::ToolExecutor::register`] refuses any other tool that claims the
/// name, such as an MCP or host tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinTool {
    /// `apply_patch`, implemented by [`crate::tools::ApplyPatch`].
    ApplyPatch,
}

impl BuiltinTool {
    /// The built-in that owns `name`, if the name is reserved.
    pub fn reserving(name: &str) -> Option<Self> {
        (name == apply_patch::tool_contract::NAME).then_some(Self::ApplyPatch)
    }
}

/// ツールを並行実行するときの排他性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    /// 他の Shared ツールと並行実行できる。
    Shared,
    /// 単独で実行する必要がある。
    Exclusive,
}

/// 標準ツールの抽象。
///
/// ツールの実行は必ず ToolExecutor（wave 3 で追加）経由で行うこと。ToolExecutor
/// が引数のスキーマ検証と結果の正規化を担うため、`execute` を直接呼び出した場合の
/// 戻り値は検証前の生の内容になる。
#[async_trait::async_trait]
pub trait Tool: Send + Sync + std::any::Any {
    /// ツールの一意な名前。
    fn name(&self) -> &str;

    /// モデルに提示する説明。独自ツールは名前を既定の説明として使う。
    fn description(&self) -> &str {
        self.name()
    }

    /// Advisory identity for callers inspecting tool metadata. Registration
    /// verifies the concrete type instead; external implementations can spoof
    /// this method and must never gain reserved-name privileges from it.
    fn builtin_identity(&self) -> Option<BuiltinTool> {
        None
    }

    /// MCP communication must pass the runtime scope gate before execution.
    fn requires_scope_gate(&self) -> bool {
        false
    }

    /// true なら、型が `null` を許さない任意の欄の `null` を、検証の前に省略として
    /// 取り除く（使わない欄を `null` で埋めるモデル向け）。スキーマと `null` の意味を
    /// 外部のサーバーが決めるツール（MCP）は、値を消すなどの意図を黙って落とさない
    /// よう false にし、検証で拒否させる。
    fn reads_null_as_omitted(&self) -> bool {
        true
    }

    /// true なら、この呼び出しは結果に [`ToolResult::progress`] を添える。runtime は
    /// 同一入力の反復を実行前ではなく結果の進捗で判定する。待機を伴う状態確認向け。
    fn reports_progress(&self, _args: &serde_json::Value) -> bool {
        false
    }

    /// 引数の JSON Schema。
    fn schema(&self) -> serde_json::Value;

    /// ツールが要求する権限。
    fn permissions(&self) -> Permissions;

    /// ツールの実行モード。未分類のツールは安全側に倒して排他実行する。
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }

    /// ツールを実行する。
    ///
    /// `args` は [`Tool::schema`] に適合する JSON オブジェクトを想定する。
    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError>;

    /// 実行文脈を受け取ってツールを実行する。既存ツールは引数のみの実行へ委譲する。
    async fn execute_with_context(
        &self,
        _ctx: &ToolExecutionContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        self.execute(args).await
    }

    /// Per-call hosted search capability, supplied only after the executor's gates.
    /// Other tools retain their ordinary execution context and implementation.
    async fn execute_with_search_provider(
        &self,
        ctx: &ToolExecutionContext,
        args: serde_json::Value,
        _search_provider: Option<&dyn crate::search::SearchProvider>,
    ) -> Result<ToolResult, ToolError> {
        self.execute_with_context(ctx, args).await
    }

    /// Cancel processes still owned by a terminal/cancelled run. No replay occurs.
    fn cancel_shell_jobs(&self, _run_id: &str) {}

    /// Release per-run resources such as browser pages once the run is terminal.
    fn end_run(&self, _run_id: &str) {}

    /// Stop and await process teardown before deleting its workspace. Failure
    /// means the workspace must be retained.
    async fn drain_shell_jobs(&self, _run_id: &str) -> Result<(), ToolError> {
        Ok(())
    }

    /// Stop the shell jobs that the `run_js` call `parent_call_id` started and
    /// await their process teardown, leaving the run's other jobs running. An
    /// error means a process may still be alive.
    async fn drain_child_shell_jobs(
        &self,
        _run_id: &str,
        _parent_call_id: &str,
    ) -> Result<(), ToolError> {
        Ok(())
    }

    /// Acknowledge the finished jobs of the `run_js` call `parent_call_id` whose child call
    /// numbers are `seqs`, once its result, which names them, has reached the model. Jobs
    /// still running, or not named, stay unobserved.
    fn observe_child_shell_jobs(&self, _run_id: &str, _parent_call_id: &str, _seqs: &[u32]) {}

    /// One line per running or unread job of the run, for a reminder that names them.
    fn unobserved_shell_job_summaries(&self, _run_id: &str) -> Vec<String> {
        Vec::new()
    }

    /// Reserve historical shell handles before restoring model-visible history.
    /// The default also covers custom shells which may later be replaced.
    fn reserve_shell_job_handles(&self, next: u64) -> Result<(), ToolError> {
        crate::shell_handles::HandleAllocator::default().reserve(next)
    }

    /// Forget reaped handles only after the terminal snapshot recorded uncertain
    /// effects. This never acknowledges results and refuses to forget live jobs.
    fn release_shell_jobs(&self, _run_id: &str) -> Result<(), ToolError> {
        Ok(())
    }

    /// Whether this run still owns a live shell process.
    fn has_running_shell_jobs(&self, _run_id: &str) -> bool {
        false
    }

    /// Running or terminal jobs whose final result has not been delivered by a
    /// tool response. Cancellation alone never acknowledges partial effects.
    fn has_unobserved_shell_jobs(&self, _run_id: &str) -> bool {
        false
    }

    /// Take one-shot completion notices without acknowledging output or effects.
    fn take_shell_job_notifications(&self, _run_id: &str) -> Vec<String> {
        Vec::new()
    }

    /// ツールが cwd を持つ場合、既定の作業ディレクトリを更新する。
    fn set_default_cwd(&self, _cwd: std::path::PathBuf) {}

    /// Receive the executor's bus for progress the tool reports outside its
    /// result, such as live shell job output. Other tools ignore it.
    fn set_event_bus(&self, _bus: std::sync::Arc<event_bus::EventBus>) {}

    /// Replace the variables a shell adds to every command it starts. Other tools ignore it.
    fn set_shell_environment(&self, _env: Vec<(String, String)>) {}

    /// shell の呼び出し単位の隔離解除を構成する。他のツールでは何もしない。
    fn set_shell_escalation(
        &self,
        _gate: std::sync::Arc<dyn crate::tools::shell_escalation::ShellEscalationGate>,
        _unsandboxed: std::sync::Arc<dyn sandbox::Sandbox>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 3 つのコンストラクタ / When: 権限を生成 / Then: フラグの組が契約どおりで network はすべて false
    #[test]
    fn permissions_const_constructors() {
        assert_eq!(
            Permissions::read_only(),
            Permissions {
                fs_read: true,
                fs_write: false,
                process_spawn: false,
                network: false,
            }
        );
        assert_eq!(
            Permissions::read_write(),
            Permissions {
                fs_read: true,
                fs_write: true,
                process_spawn: false,
                network: false,
            }
        );
        assert_eq!(
            Permissions::process(),
            Permissions {
                fs_read: true,
                fs_write: true,
                process_spawn: true,
                network: false,
            }
        );
    }

    // Given: network コンストラクタ / When: 権限を生成 / Then: network のみ true で他は false
    #[test]
    fn permissions_network_constructor_sets_only_network() {
        assert_eq!(
            Permissions::network(),
            Permissions {
                fs_read: false,
                fs_write: false,
                process_spawn: false,
                network: true,
            }
        );
    }
}
