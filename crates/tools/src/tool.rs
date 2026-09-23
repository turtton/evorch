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
pub trait Tool: Send + Sync {
    /// ツールの一意な名前。
    fn name(&self) -> &str;

    /// モデルに提示する説明。独自ツールは名前を既定の説明として使う。
    fn description(&self) -> &str {
        self.name()
    }

    /// MCP communication must pass the runtime scope gate before execution.
    fn requires_scope_gate(&self) -> bool {
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

    /// Cancel processes still owned by a terminal/cancelled run. No replay occurs.
    fn cancel_shell_jobs(&self, _run_id: &str) {}

    /// Stop and await process teardown before deleting its workspace. Failure
    /// means the workspace must be retained; mutation guards remain owned.
    async fn drain_shell_jobs(&self, _run_id: &str) -> Result<(), ToolError> {
        Ok(())
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

    /// Retain a starting call's lease even when cancellation prevents delivery
    /// of its job ID. Completed or absent jobs release the lease immediately.
    fn retain_shell_call_guard(&self, _run_id: &str, _call_id: &str, _guard: Box<dyn Send + Sync>) {
    }

    /// Move the runtime's snapshot mutation lease to a yielded process. The
    /// lease is released after termination, or immediately if already complete.
    fn retain_shell_job_guard(&self, _run_id: &str, _job_id: &str, _guard: Box<dyn Send + Sync>) {}

    /// ツールが cwd を持つ場合、既定の作業ディレクトリを更新する。
    fn set_default_cwd(&self, _cwd: std::path::PathBuf) {}

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
