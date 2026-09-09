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

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox::DirectSandbox;
    use serde_json::json;

    async fn execute(args: serde_json::Value) -> ToolResult {
        Shell::new(Arc::new(DirectSandbox::new_unchecked()))
            .execute(args)
            .await
            .expect("shell execution returns a result")
    }

    // Given: pipe and redirect / When: execute in cwd / Then: transformed file is readable.
    #[tokio::test]
    async fn sh_c_executes_pipe_and_redirect() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        let result = execute(
            json!({"command": "echo hello | tr a-z A-Z > output; cat output", "cwd": dir.path()}),
        )
        .await;
        assert_eq!(result.content, "exit_code: 0\nHELLO\n");
    }

    // Given: chained commands / When: execute / Then: both commands run.
    #[tokio::test]
    async fn sh_c_executes_command_chaining() {
        let result = execute(json!({"command": "echo a && echo b"})).await;
        assert_eq!(result.content, "exit_code: 0\na\nb\n");
    }

    // Given: matching and nonmatching files / When: expand glob / Then: only Rust files appear.
    #[tokio::test]
    async fn sh_c_executes_wildcard() {
        let dir = tempfile::tempdir().expect("temporary workspace");
        std::fs::write(dir.path().join("one.rs"), "").expect("Rust fixture");
        std::fs::write(dir.path().join("other.txt"), "").expect("other fixture");
        let result = execute(json!({"command": "printf '%s\\n' *.rs", "cwd": dir.path()})).await;
        assert_eq!(result.content, "exit_code: 0\none.rs\n");
    }

    // Given: deprecated args / When: execute / Then: args are appended.
    #[tokio::test]
    async fn args_field_appends_to_command() {
        let result = execute(json!({"command": "echo", "args": ["hello"]})).await;
        assert_eq!(result.content, "exit_code: 0\nhello\n");
    }

    // Given: shell syntax in args / When: execute / Then: the pipe is interpreted.
    #[tokio::test]
    async fn args_field_with_shell_syntax_is_interpreted() {
        let result = execute(json!({"command": "echo hello", "args": ["|", "cat"]})).await;
        assert_eq!(result.content, "exit_code: 0\nhello\n");
    }

    // Given: unterminated stdout and stderr / When: execute / Then: one newline separates them.
    #[tokio::test]
    async fn stdout_and_stderr_are_combined() {
        let result = execute(json!({"command": "printf out; printf err >&2"})).await;
        assert_eq!(result.content, "exit_code: 0\nout\nerr");
    }

    // Given: failed command / When: execute / Then: error result preserves exit code and output.
    #[tokio::test]
    async fn command_failing_shows_exit_code_in_output() {
        let result = execute(json!({"command": "echo failed >&2; exit 7"})).await;
        assert!(result.is_error);
        assert_eq!(result.content, "exit_code: 7\nfailed\n");
    }

    // Given: PTY stdout and stderr / When: execute / Then: output has no section headers.
    #[tokio::test]
    async fn interactive_output_is_combined() {
        let result = execute(
            json!({"command": "echo out; echo err >&2", "interactive": true, "timeout_ms": 1000}),
        )
        .await;
        let normalized = result.content.replace("\r\n", "\n");
        let output = normalized
            .strip_prefix("exit_code: 0\n")
            .expect("exit code");
        assert_eq!(output.trim(), "out\nerr");
    }

    // Given: denied command split over command and args / When: execute / Then: contract rejects it.
    #[tokio::test]
    async fn contract_checks_combined_command() {
        let result = execute(json!({"command": "gh pr", "args": ["merge", "123"]})).await;
        assert!(result.is_error);
        assert!(
            result
                .content
                .starts_with("shell command denied by contract:")
        );
    }

    // Given: command-only issue mutation / When: execute / Then: the existing denial is retained.
    #[tokio::test]
    async fn contract_denies_command_only_issue_mutation() {
        let result = execute(json!({"command": "gh issue create --help"})).await;
        assert!(
            result
                .content
                .starts_with("shell command denied by contract:")
        );
    }
}

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
    /// 実行する POSIX シェルコマンド（pipe、redirect、&&、glob をサポート）。
    command: String,
    /// [deprecated] command に空白区切りで追記するシェル構文。command 内への記述を推奨。
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
                "command": { "type": "string", "description": "POSIX shell command supporting pipes, redirects, && and glob expansion." },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "deprecated": true,
                    "description": "Deprecated: put arguments in command instead. Joined with spaces without quoting and interpreted as shell syntax."
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
        let command = build_shell_command(&args.command, &args.args);
        for segment in command.split([';', '|', '&', '\n']) {
            let mut tokens = segment.split_whitespace();
            if let Some(program) = tokens.next() {
                let tokens: Vec<String> = tokens.map(str::to_string).collect();
                if let CommandVerdict::Deny { reason } = self.contract.evaluate(program, &tokens) {
                    return Ok(ToolResult::error(format!(
                        "shell command denied by contract: {reason}"
                    )));
                }
            }
        }
        let shell_args = vec!["-c".to_string(), command];
        for verdict in [
            self.contract.evaluate(&args.command, &args.args),
            self.contract.evaluate("sh", &shell_args),
        ] {
            if let CommandVerdict::Deny { reason } = verdict {
                return Ok(ToolResult::error(format!(
                    "shell command denied by contract: {reason}"
                )));
            }
        }
        let wrapped = self
            .sandbox
            .wrap(CommandSpec {
                program: "sh".to_string(),
                args: shell_args,
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

fn build_shell_command(command: &str, args: &[String]) -> String {
    let mut combined = command.to_string();
    for arg in args {
        combined.push(' ');
        combined.push_str(arg);
    }
    combined
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

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    let content = format!(
        "exit_code: {}\n{combined}",
        output.status.code().unwrap_or(-1)
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
        "exit_code: {exit_code}\n{}",
        String::from_utf8_lossy(&output)
    );
    Ok(if exit_code == 0 {
        ToolResult::success(content)
    } else {
        ToolResult::error(content)
    })
}
