//! shell ツールの実装。
//!
//! 非対話モードでは [`tokio::process`] で子プロセスを起動し、対話モードでは
//! portable-pty 経由の擬似端末（PTY）を使う。全 start は owner-scoped job で監督し、
//! モデルの同期呼出しは 10 秒またはホスト入力で継続 handle を返す。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use sandbox::{CommandSpec, Sandbox};
use serde::Deserialize;

use crate::error::ToolError;
use crate::executor::ToolExecutionContext;
use crate::output::Capture;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool, ToolExecutionMode};
use crate::tools::shell_escalation::{
    EscalationDecision, ShellAccess, ShellEscalation, ShellEscalationGate,
};

mod guidance;
#[cfg(test)]
mod job_tests;
mod jobs;

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
        let result =
            execute(json!({"command": "echo out; echo err >&2", "interactive": true})).await;
        let normalized = result.content.replace("\r\n", "\n");
        let output = normalized
            .strip_prefix("exit_code: 0\n")
            .expect("exit code");
        // Dropping portable-pty's UnixMasterWriter sends a newline and VEOF;
        // terminal input echo can therefore insert an empty line between streams.
        let lines: Vec<_> = output.lines().filter(|line| !line.is_empty()).collect();
        assert_eq!(lines, ["out", "err"]);
    }

    // Given: a command that formerly matched the hardcoded deny-list / When: execute
    // Then: it runs like any other command, so the shell has no per-program deny-list.
    #[tokio::test]
    async fn issue_and_intent_cli_words_are_not_denied_by_name() {
        let result =
            execute(json!({"command": "echo gh issue close 1; echo intent-cli queue list"})).await;
        assert_eq!(
            result.content,
            "exit_code: 0\ngh issue close 1\nintent-cli queue list\n"
        );
    }
}

/// コマンドを実行するツール。
#[derive(Clone)]
pub struct Shell {
    sandbox: Arc<dyn Sandbox>,
    extra_env: Arc<RwLock<Vec<(String, String)>>>,
    default_cwd: Arc<Mutex<Option<PathBuf>>>,
    escalation: Arc<RwLock<Option<ShellEscalation>>>,
    jobs: Arc<jobs::JobRegistry>,
}

impl Shell {
    /// 指定したサンドボックスでコマンドを実行する shell ツールを生成する。
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self::with_env(sandbox, Vec::new())
    }

    /// 子プロセスへ追加で渡す環境変数を指定して shell ツールを生成する。
    ///
    /// `extra_env` は [`CommandSpec::extra_env`] 経由で sandbox の
    /// 環境統合に渡される。隔離時は最小環境へ追加し、審査済みホスト実行では
    /// 継承したプロセス環境を上書きする。
    pub fn with_env(sandbox: Arc<dyn Sandbox>, extra_env: Vec<(String, String)>) -> Self {
        Self {
            sandbox,
            extra_env: Arc::new(RwLock::new(extra_env)),
            default_cwd: Arc::new(Mutex::new(None)),
            escalation: Arc::new(RwLock::new(None)),
            jobs: Arc::new(jobs::JobRegistry::default()),
        }
    }

    pub fn with_default_cwd(self, cwd: PathBuf) -> Self {
        *self
            .default_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cwd);
        self
    }

    /// Wait for an owned job to finish without cancelling it, observing its
    /// result, or consuming its completion notification. The handle must match
    /// both the run and thread in `ctx`, just as shell control calls do.
    pub async fn wait_for_job(
        &self,
        ctx: &ToolExecutionContext,
        job_id: &str,
    ) -> Result<(), ToolError> {
        self.jobs.wait(ctx, job_id).await
    }
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SandboxAccess {
    #[default]
    Isolated,
    Network,
    Unsandboxed,
}

/// shell ツールの引数。
///
/// スキーマ検証は wave 3 の ToolExecutor が担うため、ここでは JSON からの
/// 復元に失敗した場合のみ [`ToolError::InvalidArgs`] を返す。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    #[serde(default)]
    action: Option<String>,
    yield_ms: Option<u64>,
    /// 実行する POSIX シェルコマンド（pipe、redirect、&&、glob をサポート）。
    command: String,
    /// [deprecated] command に空白区切りで追記するシェル構文。command 内への記述を推奨。
    #[serde(default)]
    args: Vec<String>,
    /// 擬似端末（PTY）上で実行するかどうか。
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    sandbox_access: SandboxAccess,
    #[serde(default)]
    justification: String,
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

    fn description(&self) -> &str {
        guidance::DESCRIPTION
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type":"string", "enum":["start", "poll", "stdin", "stop"], "default":"start"},
                "command": {"type":"string", "minLength":1, "description":"POSIX shell command. Required for start."},
                "args": {"type":"array", "items":{"type":"string"}, "deprecated":true, "description":"Deprecated shell fragments; put all shell syntax in command."},
                "interactive": {"type":"boolean", "default":false, "description":"Use a PTY. With yield_ms, retain stdin for later input."},
                "sandbox_access": {"type":"string", "enum":["isolated", "network", "unsandboxed"], "default":"isolated", "description":guidance::ACCESS_DESCRIPTION},
                "justification": {"type":"string", "description":"Why this command needs network or unsandboxed access. Required and nonempty for either mode."},
                "cwd": {"type":"string", "description":"Start directory; cannot change an existing job's cwd."},
                "timeout_ms": {"type":"integer", "minimum":1, "description":"Total command lifetime. Defaults to 1 hour; no cap."},
                "return_on": {"type":"string", "enum":["completion", "output"], "default":"completion", "description":"Poll only: completion waits for terminal teardown or new input, ignoring output. Select output explicitly to also return on new output (for interactive prompts). Stdin always returns on output or completion."},
                "yield_ms": {"type":"integer", "minimum":0, "maximum":jobs::MAX_WAIT_YIELD_MS, "description":"Start, stdin and stop only. Start: 0..1800000 (30 minutes) returns a job handle after completion or this deadline, ignoring output. Stdin/stop: defaults to 0, max 60000; stdin returns on output or completion, stop waits for teardown or the deadline. Zero returns immediately. Waiting never restarts the command or extends timeout_ms."},
                "job_id": {"type":"string", "minLength":1, "description":"Handle returned by this run's asynchronous shell start (for example, job-1)."},
                "cursor": {"type":"integer", "minimum":0, "description":"Output cursor, in redacted UTF-8 bytes. Omit it to continue after the output already returned; pass a returned cursor (or 0) to re-read from there. When input ends a poll of a running job, omitted cursor favors bounded tail output and flags skipped bytes; explicit cursors keep ordered paging. Older live output may expire."},
                "input": {"type":"string", "maxLength":16384, "description":"stdin only: bytes to write, at most 16 KiB. Timeout may mean a partial write: inspect output before retrying."},
                "close_stdin": {"type":"boolean", "default":false, "description":"stdin only: close pipe stdin or send PTY EOF after writing."}
            },
            "additionalProperties": false,
            "allOf": [{
                "if": {"properties":{"action":{"enum":["poll", "stdin", "stop"]}}, "required":["action"]},
                "then": {"required":["job_id"], "not":{"anyOf":[{"required":["command"]},{"required":["args"]},{"required":["interactive"]},{"required":["sandbox_access"]},{"required":["justification"]},{"required":["timeout_ms"]}]}},
                "else": {"required":["command"], "not":{"anyOf":[{"required":["job_id"]},{"required":["cursor"]},{"required":["input"]},{"required":["close_stdin"]},{"required":["return_on"]}]}}
            }, {
                "if": {"properties":{"action":{"enum":["stdin", "stop"]}}, "required":["action"]},
                "then": {"properties":{"yield_ms":{"maximum":jobs::MAX_CONTROL_YIELD_MS}}}
            }, {
                "if": {"properties":{"action":{"const":"poll"}}, "required":["action"]},
                "else": {"not":{"required":["return_on"]}}
            }]
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::process()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }

    fn reports_progress(&self, args: &serde_json::Value) -> bool {
        args.get("action").and_then(serde_json::Value::as_str) == Some("poll")
    }

    fn set_default_cwd(&self, cwd: PathBuf) {
        *self
            .default_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cwd);
    }

    fn set_event_bus(&self, bus: Arc<event_bus::EventBus>) {
        self.jobs.set_event_bus(bus);
    }

    fn set_shell_environment(&self, env: Vec<(String, String)>) {
        *self
            .extra_env
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = env;
    }

    fn set_shell_escalation(
        &self,
        gate: Arc<dyn ShellEscalationGate>,
        unsandboxed: Arc<dyn Sandbox>,
    ) {
        *self
            .escalation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(ShellEscalation { gate, unsandboxed });
    }

    fn reserve_shell_job_handles(&self, next: u64) -> Result<(), ToolError> {
        self.jobs.reserve_handles(next)
    }

    fn cancel_shell_jobs(&self, run_id: &str) {
        self.jobs.cancel(run_id);
    }
    async fn drain_shell_jobs(&self, run_id: &str) -> Result<(), ToolError> {
        self.jobs.drain(run_id).await
    }
    async fn drain_child_shell_jobs(
        &self,
        run_id: &str,
        parent_call_id: &str,
    ) -> Result<(), ToolError> {
        self.jobs.drain_children(run_id, parent_call_id).await
    }
    fn release_shell_jobs(&self, run_id: &str) -> Result<(), ToolError> {
        self.jobs.release(run_id)
    }
    fn has_running_shell_jobs(&self, run_id: &str) -> bool {
        self.jobs.running(run_id)
    }
    fn has_unobserved_shell_jobs(&self, run_id: &str) -> bool {
        self.jobs.unobserved(run_id)
    }

    fn take_shell_job_notifications(&self, run_id: &str) -> Vec<String> {
        self.jobs.take_notifications(run_id)
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        self.execute_with_context(
            &ToolExecutionContext {
                run_id: String::new(),
                thread_id: None,
                call_id: None,
                ..Default::default()
            },
            args,
        )
        .await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolExecutionContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        if args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|action| action != "start")
        {
            let args = serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
                detail: error.to_string(),
            })?;
            return self.jobs.control(ctx, args).await;
        }
        let args: ShellArgs =
            serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
                detail: error.to_string(),
            })?;
        if args
            .action
            .as_deref()
            .is_some_and(|action| action != "start")
            || args.command.trim().is_empty()
        {
            return Err(ToolError::InvalidArgs {
                detail: "start requires a nonempty command".into(),
            });
        }
        if let Some(ms) = args.yield_ms {
            jobs::validate_yield(ms, "start")?;
        }
        if args.timeout_ms == Some(0)
            || args.timeout_ms.is_some_and(|ms| {
                tokio::time::Instant::now()
                    .checked_add(Duration::from_millis(ms))
                    .is_none()
            })
        {
            return Err(ToolError::InvalidArgs {
                detail: "timeout_ms must be positive".into(),
            });
        }
        let command = build_shell_command(&args.command, &args.args);
        let shell_args = vec!["-c".to_string(), command];
        let root = self
            .default_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let cwd = args
            .cwd
            .as_ref()
            .map(|cwd| {
                let path = PathBuf::from(cwd);
                if path.is_absolute() {
                    path
                } else {
                    root.as_ref().map_or(path.clone(), |root| root.join(path))
                }
            })
            .or(root);
        let access = match args.sandbox_access {
            SandboxAccess::Isolated => None,
            SandboxAccess::Network => Some(ShellAccess::Network),
            SandboxAccess::Unsandboxed => Some(ShellAccess::Host),
        };
        let mut escalated_input = None;
        let sandbox = if let Some(access) = access {
            if args.justification.trim().is_empty() {
                return Ok(ToolResult::error(
                    "shell escalation denied: justification is required",
                ));
            }
            // Build the network-only variant from the existing sandbox so its
            // filesystem mounts and private HOME remain identical.
            let network_sandbox = if access == ShellAccess::Network {
                match self.sandbox.with_network_access() {
                    Ok(sandbox) => Some(sandbox),
                    Err(error) => return Ok(ToolResult::error(error.to_string())),
                }
            } else {
                None
            };
            // 審査中にロックを保持せず、gate と実行経路を同じスナップショットから使う。
            let escalation = self.escalation.read().ok().and_then(|slot| slot.clone());
            let Some(escalation) = escalation else {
                return Ok(ToolResult::error(
                    "shell escalation denied: no escalation gate configured",
                ));
            };
            match escalation
                .gate
                .decide_scoped_with_cwd(
                    ctx,
                    &shell_args[1],
                    &args.justification,
                    cwd.as_deref(),
                    access,
                )
                .await
            {
                EscalationDecision::Approve => {
                    escalated_input = Some(jobs::EscalatedInput {
                        gate: escalation.gate,
                        command: shell_args[1].clone(),
                        justification: args.justification.clone(),
                        cwd: cwd.clone(),
                        access,
                    });
                    network_sandbox.unwrap_or(escalation.unsandboxed)
                }
                decision => {
                    let (reason, guidance) = decision.denial().expect("non-approval");
                    return Ok(ToolResult::error(format!("{reason}\n{guidance}")));
                }
            }
        } else {
            Arc::clone(&self.sandbox)
        };
        let wrapped = sandbox
            .wrap(CommandSpec {
                program: "sh".to_string(),
                args: shell_args,
                cwd,
                extra_env: self
                    .extra_env
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            })
            .map_err(|error| ToolError::SandboxUnavailable {
                detail: error.to_string(),
            })?;
        wrapped
            .preflight()
            .map_err(|error| ToolError::SpawnFailed {
                command: wrapped.program.clone(),
                detail: error.to_string(),
            })?;
        self.jobs
            .start(
                ctx,
                jobs::JobLaunch {
                    wrapped,
                    interactive: args.interactive,
                    timeout_ms: args.timeout_ms,
                    yield_ms: args.yield_ms,
                    escalated: escalated_input,
                    sandbox,
                },
            )
            .await
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

fn shell_result(capture: Capture, exit_code: i32, timed_out: Option<u64>) -> ToolResult {
    let mut result = capture.finish();
    result.is_error = timed_out.is_some() || exit_code != 0;
    let status = timed_out.map_or_else(
        || format!("exit_code: {exit_code}"),
        |ms| format!("exit_code: {exit_code}\ntimed out after {ms} ms; partial output follows"),
    );
    result.content = format!("{status}\n{}", result.content);
    result.full_content = result
        .full_content
        .map(|text| crate::result::FullContent::new(format!("{status}\n{}", text.as_str())));
    result
}
