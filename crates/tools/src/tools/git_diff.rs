//! git_diff ツールの実装。
//!
//! `git diff` をサブプロセスとして実行し、インデックスと作業ツリーの差分を
//! 返す。v0.1 では差分の算出を git 本体に委ねる。

use std::process::Stdio;
use std::sync::Arc;

use sandbox::process::GroupChild;
use sandbox::{CommandSpec, Sandbox};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool, ToolExecutionMode};

/// Git の差分を取得するツール。
#[derive(Clone)]
pub struct GitDiff {
    sandbox: Arc<dyn Sandbox>,
}

impl GitDiff {
    /// 指定したサンドボックスで git を実行する差分ツールを生成する。
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sandbox }
    }
}

#[async_trait::async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &'static str {
        "git_diff"
    }

    fn description(&self) -> &str {
        "Show unstaged Git changes in a working directory, optionally limited to a path."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cwd": { "type": "string", "default": "." },
                "path": { "type": "string" }
            },
            "required": [],
            "additionalProperties": false
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions {
            fs_read: true,
            fs_write: false,
            process_spawn: true,
            network: false,
        }
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Shared
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        // 引数の型検証は wave 3 の ToolExecutor が担うため、ここでは生の値から
        // 文字列のみを取り出す。欠落や非文字列は既定値 / 未指定として扱う。
        let cwd = args
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".");
        let path = args.get("path").and_then(serde_json::Value::as_str);

        let mut command_args = vec!["diff".to_string()];
        if let Some(path) = path {
            command_args.extend(["--".to_string(), path.to_string()]);
        }
        let wrapped = self
            .sandbox
            .wrap(CommandSpec {
                program: "git".to_string(),
                args: command_args,
                cwd: Some(cwd.into()),
                extra_env: vec![
                    ("GIT_PAGER".to_string(), "cat".to_string()),
                    ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
                ],
            })
            .map_err(|error| ToolError::SandboxUnavailable {
                detail: error.to_string(),
            })?;

        // The launch policy keeps git (and any external diff tool or textconv
        // filter it runs) off evorch's terminal and descriptors. Cancelling the
        // call kills the whole process group.
        let mut command = Command::from(wrapped.to_command());
        command
            // git diff は標準入力を読まない。フック等が読んでも即時 EOF にする。
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let spawn_failed = |error: std::io::Error| ToolError::SpawnFailed {
            command: wrapped.program.clone(),
            detail: error.to_string(),
        };
        let mut child = GroupChild::spawn(&mut command).map_err(spawn_failed)?;
        let (stdout, stderr) = (child.take_stdout(), child.take_stderr());
        // Once git exits, waiting kills helpers that still hold its output, so
        // the reads end and no reaped group id is signalled later.
        let (status, stdout, stderr) =
            tokio::try_join!(child.wait(), read_all(stdout), read_all(stderr))
                .map_err(spawn_failed)?;

        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);

        // リポジトリ外での実行は git のバージョンでメッセージが異なる
        // （古: "fatal: not a git repository" / 新: "warning: Not a git repository.
        // Use --no-index ..."）。大文字小文字を無視して文言で判定する。
        if !status.success() && stderr.to_ascii_lowercase().contains("not a git repository") {
            return Err(ToolError::NotAGitRepository {
                path: cwd.to_string(),
            });
        }
        if !status.success() {
            return Err(ToolError::Io {
                detail: format!("git diff が {status} で失敗しました: {}", stderr.trim()),
            });
        }
        Ok(ToolResult::success(stdout))
    }
}

async fn read_all(pipe: Option<impl AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
}
