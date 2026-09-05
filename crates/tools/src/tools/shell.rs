//! shell ツールの実装。
//!
//! 非対話モードでは [`tokio::process`] で子プロセスを起動し、対話モードでは
//! portable-pty 経由の擬似端末（PTY）上で 1 回限りの実行を行う。

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sandbox::{CommandSpec, Sandbox, WrappedCommand};
use serde::Deserialize;

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};
use crate::tools::shell_contract::{CommandVerdict, ShellCommandContract};

/// コマンドを実行するツール。
#[derive(Clone)]
pub struct Shell {
    sandbox: Arc<dyn Sandbox>,
    contract: ShellCommandContract,
    extra_env: Vec<(String, String)>,
}

impl Shell {
    /// 指定したサンドボックスでコマンドを実行する shell ツールを生成する。
    ///
    /// 契約は [`ShellCommandContract::standard`]（deny-list）が適用される。
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self::with_contract(sandbox, ShellCommandContract::standard())
    }

    /// 実行コマンドの可否を判定する契約を指定して shell ツールを生成する。
    pub fn with_contract(sandbox: Arc<dyn Sandbox>, contract: ShellCommandContract) -> Self {
        Self {
            sandbox,
            contract,
            extra_env: Vec::new(),
        }
    }

    /// 契約と子プロセスへ追加で渡す環境変数を指定して shell ツールを生成する。
    ///
    /// `extra_env` は [`CommandSpec::extra_env`] 経由で sandbox の
    /// 環境統合（PATH/TERM/LANG/LC_ALL への追加）に渡される。
    pub fn with_contract_and_env(
        sandbox: Arc<dyn Sandbox>,
        contract: ShellCommandContract,
        extra_env: Vec<(String, String)>,
    ) -> Self {
        Self {
            sandbox,
            contract,
            extra_env,
        }
    }
}

/// shell ツールの引数。
///
/// スキーマ検証は wave 3 の ToolExecutor が担うため、ここでは JSON からの
/// 復元に失敗した場合のみ [`ToolError::InvalidArgs`] を返す。
#[derive(Debug, Deserialize)]
struct ShellArgs {
    /// 実行するコマンド。
    command: String,
    /// コマンドへ渡す引数。
    #[serde(default)]
    args: Vec<String>,
    /// 擬似端末（PTY）上で実行するかどうか。
    #[serde(default)]
    interactive: bool,
    /// 作業ディレクトリ。
    cwd: Option<String>,
    /// 制限時間（ミリ秒）。
    timeout_ms: Option<u64>,
}

#[async_trait::async_trait]
impl Tool for Shell {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "args": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "interactive": { "type": "boolean", "default": false },
                "cwd": { "type": "string" },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::process()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let args: ShellArgs =
            serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
                detail: error.to_string(),
            })?;
        // 契約判定はサンドボックスの wrap より先に行い、拒否時は子プロセスを
        // 起動しない。拒否は Err ではなく is_error 付きの結果として返し、
        // モデルへツールエラーとして見せる（計画 S9）。
        if let CommandVerdict::Deny { reason } = self.contract.evaluate(&args.command, &args.args) {
            return Ok(ToolResult::error(format!(
                "shell command denied by contract: {reason}"
            )));
        }
        let wrapped = self
            .sandbox
            .wrap(CommandSpec {
                program: args.command.clone(),
                args: args.args.clone(),
                cwd: args.cwd.as_ref().map(PathBuf::from),
                extra_env: self.extra_env.clone(),
            })
            .map_err(|error| ToolError::SandboxUnavailable {
                detail: error.to_string(),
            })?;
        if args.interactive {
            run_interactive(&wrapped, args.timeout_ms).await
        } else {
            run_process(&wrapped, args.timeout_ms).await
        }
    }
}

/// 起動系の失敗を [`ToolError::SpawnFailed`] へ変換する。
fn spawn_failed(command: &str, error: impl std::fmt::Display) -> ToolError {
    ToolError::SpawnFailed {
        command: command.to_string(),
        detail: error.to_string(),
    }
}

/// 入出力の失敗を [`ToolError::Io`] へ変換する。
fn io_failed(error: impl std::fmt::Display) -> ToolError {
    ToolError::Io {
        detail: error.to_string(),
    }
}

/// 非対話モード: tokio::process で子プロセスを実行する。
async fn run_process(
    wrapped: &WrappedCommand,
    timeout_ms: Option<u64>,
) -> Result<ToolResult, ToolError> {
    let mut command = tokio::process::Command::new(&wrapped.program);
    command
        .args(&wrapped.args)
        .env_clear()
        .envs(wrapped.env.iter().cloned())
        .kill_on_drop(true);
    if let Some(cwd) = &wrapped.cwd {
        command.current_dir(cwd);
    }

    let spawned = match timeout_ms {
        Some(timeout_ms) => {
            tokio::time::timeout(Duration::from_millis(timeout_ms), command.output())
                .await
                .map_err(|_| ToolError::Timeout { timeout_ms })?
        }
        None => command.output().await,
    };
    let output = spawned.map_err(|error| spawn_failed(&wrapped.program, error))?;

    let content = format!(
        "exit_code: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(if output.status.success() {
        ToolResult::success(content)
    } else {
        ToolResult::error(content)
    })
}

/// 対話モード: portable-pty の擬似端末上で 1 回限り実行する。
///
/// wezterm の whoami.rs 例と同じ手順を踏む。スレーブとライターを即座に捨てて
/// 子プロセス側に EOF を見せないと、リーダーの読み取りが終端せずデッドロックする。
async fn run_interactive(
    wrapped: &WrappedCommand,
    timeout_ms: Option<u64>,
) -> Result<ToolResult, ToolError> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| spawn_failed(&wrapped.program, error))?;
    let portable_pty::PtyPair { master, slave } = pair;

    let mut command = portable_pty::CommandBuilder::new(&wrapped.program);
    command.args(&wrapped.args);
    command.env_clear();
    for (key, value) in &wrapped.env {
        command.env(key, value);
    }
    if let Some(cwd) = &wrapped.cwd {
        command.cwd(cwd);
    } else {
        command.cwd(std::env::current_dir().map_err(io_failed)?);
    }
    let mut child = slave
        .spawn_command(command)
        .map_err(|error| spawn_failed(&wrapped.program, error))?;
    drop(slave);

    let mut reader = master.try_clone_reader().map_err(io_failed)?;
    // ライターを捨てて子プロセスの標準入力に EOF を見せ、読み取りを終端させる。
    let writer = master.take_writer().map_err(io_failed)?;
    drop(writer);

    // wait でブロックする blocking タスクとは独立に殺せるよう、先に killer を複製する。
    let mut killer = child.clone_killer();

    let mut blocking = tokio::task::spawn_blocking(move || -> Result<(u32, Vec<u8>), ToolError> {
        let reader_thread = std::thread::spawn(move || -> Result<Vec<u8>, ToolError> {
            let mut output = Vec::new();
            reader.read_to_end(&mut output).map_err(io_failed)?;
            Ok(output)
        });
        let status = child.wait().map_err(io_failed)?;
        // 親の master を捨ててリーダー側の EOF（Linux では EIO）を確実にする。
        drop(master);
        let output = reader_thread.join().map_err(|_| ToolError::Io {
            detail: "PTY リーダースレッドがパニックしました".to_string(),
        })??;
        Ok((status.exit_code(), output))
    });

    let waited = match timeout_ms {
        Some(timeout_ms) => {
            match tokio::time::timeout(Duration::from_millis(timeout_ms), &mut blocking).await {
                Ok(joined) => joined.map_err(io_failed)?,
                Err(_elapsed) => {
                    let _ = killer.kill();
                    // 後片付け（wait とリーダー読み取りの完了）を待ってから返す。
                    let _ = blocking.await;
                    return Err(ToolError::Timeout { timeout_ms });
                }
            }
        }
        None => blocking.await.map_err(io_failed)?,
    };

    let (exit_code, output) = waited?;
    let content = format!(
        "exit_code: {exit_code}\n--- output ---\n{}",
        String::from_utf8_lossy(&output)
    );
    Ok(if exit_code == 0 {
        ToolResult::success(content)
    } else {
        ToolResult::error(content)
    })
}
