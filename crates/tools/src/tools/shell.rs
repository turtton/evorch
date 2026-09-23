//! shell ツールの実装。
//!
//! 非対話モードでは [`tokio::process`] で子プロセスを起動し、対話モードでは
//! portable-pty 経由の擬似端末（PTY）を使う。yield_ms の明示指定時には
//! owner-scoped job として制御を返し、poll/stdin/stop で継続する。

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use sandbox::{CommandSpec, Sandbox, WrappedCommand};
use serde::Deserialize;

use crate::error::ToolError;
use crate::executor::ToolExecutionContext;
use crate::output::Capture;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool, ToolExecutionMode};
use crate::tools::shell_contract::{CommandVerdict, ShellCommandContract};
use crate::tools::shell_escalation::{
    EscalationDecision, ShellAccess, ShellEscalation, ShellEscalationGate,
};

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
    default_cwd: Arc<Mutex<Option<PathBuf>>>,
    escalation: Arc<RwLock<Option<ShellEscalation>>>,
    jobs: Arc<jobs::JobRegistry>,
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
            default_cwd: Arc::new(Mutex::new(None)),
            escalation: Arc::new(RwLock::new(None)),
            jobs: Arc::new(jobs::JobRegistry::default()),
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
    require_escalated: bool,
    #[serde(default)]
    require_network: bool,
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
        "Run a POSIX shell command. Without yield_ms, wait for completion. With yield_ms (0..60000), return a run-owned job ID and cursor; use action poll/stdin/stop to continue it. Jobs retain their sandbox/cwd, stop when the run ends, and cannot resume after restart. Live output contains complete lines; long output has a bounded temporary artifact on completion. For dependency downloads or git pull when sandbox DNS/network access fails, request require_network with justification to keep filesystem isolation; require_escalated removes all isolation."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type":"string", "enum":["start", "poll", "stdin", "stop"], "default":"start"},
                "command": {"type":"string", "minLength":1, "description":"POSIX shell command. Required for start."},
                "args": {"type":"array", "items":{"type":"string"}, "deprecated":true, "description":"Deprecated shell fragments; put all shell syntax in command."},
                "interactive": {"type":"boolean", "default":false, "description":"Use a PTY. With yield_ms, retain stdin for later input."},
                "require_escalated": {"type":"boolean", "default":false, "description":"Run without filesystem or network sandbox after review."},
                "require_network": {"type":"boolean", "default":false, "description":"Allow host networking for this command after review; retain sandbox filesystem mounts."},
                "justification": {"type":"string"},
                "cwd": {"type":"string", "description":"Start directory; cannot change an existing job's cwd."},
                "timeout_ms": {"type":"integer", "minimum":1, "description":"Total command lifetime. Async jobs default to 1 hour."},
                "yield_ms": {"type":"integer", "minimum":0, "maximum":60000, "description":"Start async job or wait for new output/completion, at most this many milliseconds."},
                "job_id": {"type":"string", "minLength":1, "description":"ID returned by this run's asynchronous shell start."},
                "cursor": {"type":"integer", "minimum":0, "default":0, "description":"Returned output cursor, in redacted UTF-8 bytes. Older live output may expire."},
                "input": {"type":"string", "maxLength":16384, "description":"stdin only: bytes to write, at most 16 KiB. Timeout may mean a partial write: inspect output before retrying."},
                "close_stdin": {"type":"boolean", "default":false, "description":"stdin only: close pipe stdin or send PTY EOF after writing."}
            },
            "additionalProperties": false,
            "allOf": [{
                "if": {"properties":{"action":{"enum":["poll", "stdin", "stop"]}}, "required":["action"]},
                "then": {"required":["job_id"], "not":{"anyOf":[{"required":["command"]},{"required":["args"]},{"required":["interactive"]},{"required":["require_escalated"]},{"required":["require_network"]},{"required":["justification"]},{"required":["timeout_ms"]}]}},
                "else": {"required":["command"], "not":{"anyOf":[{"required":["job_id"]},{"required":["cursor"]},{"required":["input"]},{"required":["close_stdin"]}]}}
            }]
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::process()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }

    fn set_default_cwd(&self, cwd: PathBuf) {
        *self
            .default_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cwd);
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

    fn cancel_shell_jobs(&self, run_id: &str) {
        self.jobs.cancel(run_id);
    }
    async fn drain_shell_jobs(&self, run_id: &str) -> Result<(), ToolError> {
        self.jobs.drain(run_id).await
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

    fn retain_shell_call_guard(&self, run_id: &str, call_id: &str, guard: Box<dyn Send + Sync>) {
        self.jobs.retain_call_guard(run_id, call_id, guard);
    }

    fn retain_shell_job_guard(&self, run_id: &str, job_id: &str, guard: Box<dyn Send + Sync>) {
        self.jobs.retain_guard(run_id, job_id, guard);
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        self.execute_with_context(
            &ToolExecutionContext {
                run_id: String::new(),
                thread_id: None,
                call_id: None,
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
        if args.yield_ms.is_some_and(|ms| ms > 60_000)
            || args.timeout_ms == Some(0)
            || args.timeout_ms.is_some_and(|ms| {
                tokio::time::Instant::now()
                    .checked_add(Duration::from_millis(ms))
                    .is_none()
            })
        {
            return Err(ToolError::InvalidArgs {
                detail: "yield_ms must be 0..60000 and timeout_ms must be positive".into(),
            });
        }
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
        if args.require_escalated && args.require_network {
            return Ok(ToolResult::error(
                "shell access denied: require_escalated and require_network are mutually exclusive",
            ));
        }
        let mut escalated_input = None;
        let sandbox = if args.require_escalated || args.require_network {
            if args.justification.trim().is_empty() {
                return Ok(ToolResult::error(
                    "shell escalation denied: justification is required",
                ));
            }
            // Build the network-only variant from the existing sandbox so its
            // filesystem mounts and private HOME remain identical.
            let access = if args.require_network {
                ShellAccess::Network
            } else {
                ShellAccess::Host
            };
            let network_sandbox = if args.require_network {
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
                EscalationDecision::Deny { reason } => return Ok(ToolResult::error(reason)),
            }
        } else {
            Arc::clone(&self.sandbox)
        };
        let wrapped = sandbox
            .wrap(CommandSpec {
                program: "sh".to_string(),
                args: shell_args,
                cwd,
                extra_env: self.extra_env.clone(),
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
        if let Some(yield_ms) = args.yield_ms {
            return self
                .jobs
                .start(
                    ctx,
                    jobs::JobLaunch {
                        wrapped,
                        interactive: args.interactive,
                        timeout_ms: args.timeout_ms,
                        yield_ms,
                        escalated: escalated_input,
                        sandbox,
                    },
                )
                .await;
        }
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

    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| spawn_failed(&wrapped.program, error))?;
    let group = ProcessGroup(
        child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(id as i32)),
    );
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io_failed("missing stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io_failed("missing stderr"))?;
    let mut out = Capture::default();
    let mut err = Capture::default();
    let completion = async {
        tokio::join!(
            child.wait(),
            drain(&mut stdout, &mut out),
            drain(&mut stderr, &mut err)
        )
    };
    let waited = match timeout_ms {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), completion).await,
        None => Ok(completion.await),
    };
    let (exit_code, timed_out) = match waited {
        Ok((status, stdout, stderr)) => {
            stdout?;
            stderr?;
            (status.map_err(io_failed)?.code().unwrap_or(-1), false)
        }
        Err(_) => {
            group.kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            (-1, true)
        }
    };
    drop(group);
    out.append(&err);
    Ok(shell_result(
        out,
        exit_code,
        if timed_out { timeout_ms } else { None },
    ))
}

struct ProcessGroup(Option<rustix::process::Pid>);
impl ProcessGroup {
    fn kill(&self) {
        if let Some(pid) = self.0 {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn drain(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    capture: &mut Capture,
) -> Result<(), ToolError> {
    use tokio::io::AsyncReadExt;
    let mut buffer = [0u8; 8192];
    loop {
        let n = reader.read(&mut buffer).await.map_err(io_failed)?;
        if n == 0 {
            return Ok(());
        }
        capture.push(&buffer[..n]);
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
    result
}

// Cancellation also tears down a PTY child; dropping a blocking task's handle
// alone does not stop the process or its reader thread.
struct PtyKiller(Box<dyn portable_pty::ChildKiller + Send + Sync>);
impl Drop for PtyKiller {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
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
    let mut killer = PtyKiller(child.clone_killer());

    let captured = Arc::new(Mutex::new(Capture::default()));
    let read_capture = Arc::clone(&captured);
    let mut blocking = tokio::task::spawn_blocking(move || -> Result<u32, ToolError> {
        let reader_thread = std::thread::spawn(move || -> Result<(), ToolError> {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(n) => read_capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(&buffer[..n]),
                    Err(error) if error.raw_os_error() == Some(5) => return Ok(()), // Linux PTY EOF
                    Err(error) => return Err(io_failed(error)),
                }
            }
        });
        let status = child.wait().map_err(io_failed)?;
        drop(master);
        reader_thread
            .join()
            .map_err(|_| io_failed("PTY reader panicked"))??;
        Ok(status.exit_code())
    });
    let waited = match timeout_ms {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), &mut blocking).await,
        None => Ok((&mut blocking).await),
    };
    let (exit_code, timed_out) = match waited {
        Ok(joined) => (joined.map_err(io_failed)?? as i32, None),
        Err(_) => {
            let _ = killer.0.kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), blocking).await;
            (-1, timeout_ms)
        }
    };
    let output = std::mem::take(
        &mut *captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    Ok(shell_result(output, exit_code, timed_out))
}
