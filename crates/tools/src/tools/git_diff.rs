//! git_diff ツールの実装。
//!
//! `git diff` をサブプロセスとして実行し、インデックスと作業ツリーの差分を
//! 返す。v0.1 では差分の算出を git 本体に委ねる。

use std::process::Stdio;

use tokio::process::Command;

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};

/// Git の差分を取得するツール。
#[derive(Debug, Clone, Copy)]
pub struct GitDiff;

#[async_trait::async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &'static str {
        "git_diff"
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
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        // 引数の型検証は wave 3 の ToolExecutor が担うため、ここでは生の値から
        // 文字列のみを取り出す。欠落や非文字列は既定値 / 未指定として扱う。
        let cwd = args
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".");
        let path = args.get("path").and_then(serde_json::Value::as_str);

        let mut command = Command::new("git");
        command
            .arg("diff")
            .current_dir(cwd)
            // ユーザーの pager / 認証プロンプト設定が差分取得を妨げないようにする。
            .env("GIT_PAGER", "cat")
            .env("GIT_TERMINAL_PROMPT", "0")
            // git diff は標準入力を読まない。フック等が読んでも即時 EOF にする。
            .stdin(Stdio::null());
        if let Some(path) = path {
            command.arg("--").arg(path);
        }

        let output = command
            .output()
            .await
            .map_err(|error| ToolError::SpawnFailed {
                command: "git".to_string(),
                detail: error.to_string(),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let status = output.status;

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
