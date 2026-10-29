//! Scheduling, deadlines and cancellation of `run_js` against a `sh` worker that
//! speaks the protocol (no evorch binary, no bubblewrap).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sandbox::DirectSandbox;
use serde_json::{Value, json};
use tokio::sync::Barrier;

use super::{ChildCall, ChildOutcome, RunJs, RunJsHost, WorkerLauncher, serve};
use crate::executor::ToolExecutionContext;
use crate::result::ToolResult;
use crate::tool::Tool;

/// A worker that starts running `script` without reading anything first.
fn launcher_raw(script: &str) -> WorkerLauncher {
    WorkerLauncher::injected(
        Arc::new(DirectSandbox::new_unchecked()),
        PathBuf::from("sh"),
        vec!["-c".into(), script.to_owned()],
    )
}

/// A worker that has read the `run` message when `script` starts.
fn launcher(script: &str) -> WorkerLauncher {
    launcher_raw(&format!("read -r run\n{script}"))
}

fn call_line(id: u64, name: &str, args: &Value) -> String {
    let message = json!({"type": "tool_call", "id": id, "name": name, "args": args});
    format!("printf '%s\\n' '{message}'\n")
}

const DONE: &str = "printf '%s\\n' '{\"type\":\"done\",\"value\":null}'\n";

fn ok_outcome(paths: &[&str], written: &[&str]) -> ChildOutcome {
    ChildOutcome {
        result: Ok(json!({"ok": true})),
        is_error: false,
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        written: written.iter().map(|path| (*path).to_owned()).collect(),
        failure: None,
        warning: None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    /// The first two calls wait for each other, so they must overlap.
    PairedStart,
    /// Every call checks that exclusivity holds, then finishes.
    Exclusive,
    /// Every call waits forever.
    Hang,
}

struct ScriptHost {
    behavior: Behavior,
    settings: config::RunJsConfig,
    barrier: Barrier,
    shared_active: AtomicUsize,
    exclusive_active: AtomicUsize,
    finished: std::sync::Mutex<Vec<u32>>,
    started: AtomicUsize,
    /// Bytes of text in every reply, to fill the pipe to a worker that does not read.
    reply_bytes: usize,
    drain_error: Option<String>,
    dropped_when_drained: AtomicUsize,
    /// A FIFO the first call writes to once it runs, releasing a worker waiting on it.
    signal: std::sync::Mutex<Option<PathBuf>>,
    dropped: Arc<AtomicUsize>,
    /// Ends the wait for the worker once this call number was answered.
    expire_after: Option<(u32, Arc<tokio::sync::Notify>)>,
    /// What the host was told could not be confirmed stopped.
    unconfirmed: std::sync::Mutex<Vec<String>>,
    /// Every `read` answers with a `next` for the page 300 lines further on.
    paged_reads: bool,
    /// `(seq, continuation)` of every call the host was given.
    seen: std::sync::Mutex<Vec<(u32, bool)>>,
}

impl std::fmt::Debug for ScriptHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ScriptHost")
    }
}

impl ScriptHost {
    fn new(behavior: Behavior) -> Arc<Self> {
        Arc::new(Self {
            behavior,
            settings: config::RunJsConfig {
                enabled: true,
                ..config::RunJsConfig::default()
            },
            barrier: Barrier::new(2),
            shared_active: AtomicUsize::new(0),
            exclusive_active: AtomicUsize::new(0),
            finished: std::sync::Mutex::new(Vec::new()),
            started: AtomicUsize::new(0),
            reply_bytes: 0,
            drain_error: None,
            dropped_when_drained: AtomicUsize::new(usize::MAX),
            signal: std::sync::Mutex::new(None),
            dropped: Arc::new(AtomicUsize::new(0)),
            expire_after: None,
            unconfirmed: std::sync::Mutex::new(Vec::new()),
            paged_reads: false,
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn finished(&self) -> Vec<u32> {
        self.finished.lock().unwrap().clone()
    }

    fn configured(behavior: Behavior, adjust: impl FnOnce(&mut Self)) -> Arc<Self> {
        let mut host = Arc::try_unwrap(Self::new(behavior)).ok().unwrap();
        adjust(&mut host);
        Arc::new(host)
    }
}

struct CountDrop(Arc<AtomicUsize>);

impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl RunJsHost for ScriptHost {
    fn tool_names(&self) -> Vec<String> {
        ["read", "grep", "write", "edit", "shell"]
            .map(String::from)
            .into()
    }

    fn settings(&self) -> config::RunJsConfig {
        self.settings.clone()
    }

    fn shares_wave(&self, name: &str) -> bool {
        matches!(name, "read" | "grep")
    }

    fn report_unconfirmed(&self, detail: &str) {
        self.unconfirmed.lock().unwrap().push(detail.to_owned());
    }

    async fn drain_children(&self, _parent_call_id: &str) -> Result<(), String> {
        self.dropped_when_drained
            .store(self.dropped.load(Ordering::SeqCst), Ordering::SeqCst);
        self.drain_error.clone().map_or(Ok(()), Err)
    }

    async fn call(&self, call: ChildCall) -> ChildOutcome {
        let _guard = CountDrop(Arc::clone(&self.dropped));
        self.started.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap()
            .push((call.seq, call.continuation));
        let signal = self.signal.lock().unwrap().take();
        if let Some(fifo) = signal {
            tokio::task::spawn_blocking(move || std::fs::write(fifo, "go\n"))
                .await
                .unwrap()
                .unwrap();
        }
        let shares = self.shares_wave(&call.name);
        match self.behavior {
            Behavior::Hang => std::future::pending::<()>().await,
            Behavior::PairedStart => {
                if call.seq <= 2 {
                    self.barrier.wait().await;
                }
            }
            Behavior::Exclusive => {}
        }
        if shares {
            assert_eq!(
                self.exclusive_active.load(Ordering::SeqCst),
                0,
                "a shared call started beside an exclusive one"
            );
            self.shared_active.fetch_add(1, Ordering::SeqCst);
        } else {
            assert_eq!(
                self.shared_active.load(Ordering::SeqCst)
                    + self.exclusive_active.fetch_add(1, Ordering::SeqCst),
                0,
                "an exclusive call started beside another call"
            );
        }
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        if shares {
            self.shared_active.fetch_sub(1, Ordering::SeqCst);
        } else {
            self.exclusive_active.fetch_sub(1, Ordering::SeqCst);
        }
        self.finished.lock().unwrap().push(call.seq);
        if let Some((seq, expire)) = &self.expire_after
            && call.seq == *seq
        {
            expire.notify_one();
        }
        let mut outcome = ok_outcome(&[], &[]);
        if self.paged_reads
            && call.name == "read"
            && let Some(path) = call.args.get("path").and_then(Value::as_str)
        {
            let offset = call.args.get("offset").and_then(Value::as_u64).unwrap_or(1);
            outcome.result = Ok(json!({
                "ok": true,
                "next": {"tool": "read", "path": path, "offset": offset + 300, "byte_offset": 0},
            }));
        }
        if self.reply_bytes > 0 {
            outcome.result = Ok(json!({"ok": true, "text": "x".repeat(self.reply_bytes)}));
        }
        outcome
    }
}

async fn run(script: &str, host: Arc<ScriptHost>, code: &str, timeout_ms: u64) -> ToolResult {
    run_with(
        RunJs::with_launcher(launcher(script)),
        host,
        code,
        timeout_ms,
    )
    .await
}

async fn run_with(tool: RunJs, host: Arc<ScriptHost>, code: &str, timeout_ms: u64) -> ToolResult {
    let ctx = ToolExecutionContext {
        run_id: "run".into(),
        call_id: Some("parent".into()),
        run_js_host: Some(host),
        ..Default::default()
    };
    tool.execute_with_context(&ctx, json!({"code": code, "timeout_ms": timeout_ms}))
        .await
        .unwrap()
}

fn assert_gone(pid_file: &std::path::Path) {
    let pid: i32 = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_err(),
        "worker {pid} survived"
    );
}

#[tokio::test]
async fn shared_calls_overlap_and_a_writer_runs_alone_in_program_order() {
    let mut script = String::new();
    script.push_str(&call_line(1, "read", &json!({})));
    script.push_str(&call_line(2, "grep", &json!({})));
    script.push_str(&call_line(3, "write", &json!({})));
    script.push_str(&call_line(4, "read", &json!({})));
    script.push_str("read -r a; read -r b; read -r c; read -r d\n");
    script.push_str(DONE);
    // Calls 1 and 2 wait for each other at a barrier: serial execution would
    // never get past it. The host's own assertions check exclusivity.
    let host = ScriptHost::new(Behavior::PairedStart);

    let result = run(&script, host.clone(), "x", 60_000).await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.ends_with("[run_js: 4 calls, 0 failed]"));
    let finished = host.finished();
    assert_eq!(&finished[2..], [3, 4], "writer, then the read behind it");
    assert!(finished[..2].contains(&1) && finished[..2].contains(&2));
}

#[tokio::test]
async fn consecutive_writers_never_overlap_each_other_or_readers() {
    let mut script = String::new();
    for (id, name) in [
        (1, "write"),
        (2, "edit"),
        (3, "read"),
        (4, "shell"),
        (5, "write"),
    ] {
        script.push_str(&call_line(id, name, &json!({})));
    }
    script.push_str("read -r a; read -r b; read -r c; read -r d; read -r e\n");
    script.push_str(DONE);
    let host = ScriptHost::new(Behavior::Exclusive);

    let result = run(&script, host.clone(), "x", 60_000).await;

    assert!(!result.is_error, "{}", result.content);
    assert_eq!(host.finished(), [1, 2, 3, 4, 5], "program order is kept");
}

fn fifo(directory: &std::path::Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    let made = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(made.success());
    path
}

#[tokio::test]
async fn a_worker_that_never_reads_its_first_message_cannot_outlast_the_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let ready = fifo(directory.path(), "ready");
    // The worker records its pid and never touches its stdin, so a program larger
    // than the pipe buffer would block a synchronous write for good.
    let script = format!(
        "echo $$ > {}\necho up > {}\nexec sleep 600\n",
        pid_file.display(),
        ready.display()
    );
    let host = ScriptHost::new(Behavior::Exclusive);
    let big_program = "x".repeat(4 * 1024 * 1024);
    let expire = Arc::new(tokio::sync::Notify::new());
    // The deadline is simulated only once the worker is running: the FIFO is written
    // after its pid, so a slow start can neither fake nor hide the timeout.
    let started = {
        let expire = Arc::clone(&expire);
        tokio::task::spawn_blocking(move || {
            std::fs::read_to_string(ready).unwrap();
            expire.notify_one();
        })
    };

    let result = run_with(
        RunJs::with_launcher(launcher_raw(&script)).with_expiry(expire),
        host,
        &big_program,
        60_000,
    )
    .await;

    started.await.unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("timed out"), "{}", result.content);
    assert_gone(&pid_file);
}

#[tokio::test]
async fn a_worker_that_stops_reading_after_its_requests_cannot_outlast_the_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    // It read the program (it sends requests only after that), then reads nothing
    // more: the replies, 600 KiB each, fill the pipe and block a synchronous write.
    let mut script = String::new();
    for id in 1..=3 {
        script.push_str(&call_line(id, "read", &json!({})));
    }
    script.push_str(&format!(
        "echo $$ > {}\nexec sleep 600\n",
        pid_file.display()
    ));
    let expire = Arc::new(tokio::sync::Notify::new());
    // The simulated deadline comes once the host answered the third request, so
    // all three requests reached it and the replies have started to back up.
    let host = ScriptHost::configured(Behavior::Exclusive, |host| {
        host.reply_bytes = 600 * 1024;
        host.expire_after = Some((3, Arc::clone(&expire)));
    });

    let result = run_with(
        RunJs::with_launcher(launcher(&script)).with_expiry(expire),
        host,
        "x",
        60_000,
    )
    .await;

    assert!(result.is_error);
    assert!(result.content.contains("timed out"), "{}", result.content);
    assert_gone(&pid_file);
}

#[tokio::test]
async fn replies_a_worker_does_not_read_are_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let mut script = format!("echo $$ > {}\n", pid_file.display());
    for id in 1..=2 {
        script.push_str(&call_line(id, "read", &json!({})));
    }
    script.push_str("exec sleep 600\n");
    let host = ScriptHost::configured(Behavior::Exclusive, |host| host.reply_bytes = 600 * 1024);
    let bounds = serve::Bounds {
        pending_reply_bytes: 1024 * 1024,
        ..serve::Bounds::default()
    };

    // No short deadline: the bound alone ends the call.
    let result = run_with(
        RunJs::with_launcher(launcher(&script)).with_bounds(bounds),
        host,
        "x",
        60_000,
    )
    .await;

    assert!(result.is_error);
    assert!(
        result.content.contains("not reading its replies"),
        "{}",
        result.content
    );
    assert_gone(&pid_file);
}

#[tokio::test]
async fn a_program_that_keeps_calling_after_the_limit_is_stopped_with_bounded_records() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    // Never reads a reply; 500 requests would be sent if nothing stopped it.
    let script = format!(
        "echo $$ > {}\ni=1\nwhile [ $i -le 500 ]; do\n  printf '%s\\n' \"{{\\\"type\\\":\\\"tool_call\\\",\\\"id\\\":$i,\\\"name\\\":\\\"read\\\",\\\"args\\\":{{}}}}\"\n  i=$((i+1))\ndone\nexec sleep 600\n",
        pid_file.display()
    );
    let host = ScriptHost::configured(Behavior::Exclusive, |host| {
        host.settings.max_tool_calls = 3;
    });

    let result = run(&script, host, "x", 60_000).await;

    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("kept calling tools after the limit of 3 calls"),
        "{}",
        result.content
    );
    let detail = result.detail.unwrap();
    assert_eq!(detail["run_js"]["limit_hit"], "tool_calls");
    let recorded = detail["run_js"]["calls"].as_array().unwrap().len();
    assert!(recorded <= 3 + 32, "{recorded} calls were recorded");
    assert!(
        result.content.len() < 8 * 1024,
        "{} bytes",
        result.content.len()
    );
    assert_gone(&pid_file);
}

/// A script that makes each of `calls` and reads its reply before the next.
fn sequential_script(calls: &[(&str, Value)]) -> String {
    let mut script = String::new();
    for (index, (name, args)) in calls.iter().enumerate() {
        script.push_str(&call_line(index as u64 + 1, name, args));
        script.push_str("read -r _\n");
    }
    script.push_str(DONE);
    script
}

fn failed_seqs(result: &ToolResult) -> Vec<u64> {
    result.detail.as_ref().unwrap()["run_js"]["calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|call| call["is_error"] == true)
        .map(|call| call["seq"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn a_worker_that_is_gone_after_the_time_limit_is_reported_as_a_timeout() {
    // The worker outlives the limit of 300 ms a little and then exits without a word,
    // as one stopped by its own timer does.
    let late = run(
        "sleep 1\nexit 0",
        ScriptHost::new(Behavior::Exclusive),
        "x",
        300,
    )
    .await;
    assert!(late.is_error);
    assert!(
        late.content
            .contains("run_js timed out after 300 ms and was stopped"),
        "{}",
        late.content
    );
    assert!(
        !late.content.contains("exited without finishing"),
        "{}",
        late.content
    );
    assert_eq!(late.detail.unwrap()["run_js"]["limit_hit"], "timeout");

    // One that dies well within the limit is a failure of the worker, not a timeout.
    let early = run("exit 0", ScriptHost::new(Behavior::Exclusive), "x", 60_000).await;
    assert!(
        early.content.contains("exited without finishing"),
        "{}",
        early.content
    );
    assert!(!early.content.contains("timed out"), "{}", early.content);
}

#[tokio::test]
async fn a_shell_command_running_when_the_program_ends_is_named_as_cut_off() {
    // The program starts a long command and finishes without waiting for it.
    let script = format!(
        "{}{DONE}",
        call_line(
            1,
            "shell",
            &json!({"command": "cargo check --offline -p tools --tests\n  --message-format=json"})
        )
    );
    let result = run(&script, ScriptHost::new(Behavior::Hang), "x", 60_000).await;

    let text = &result.content;
    assert!(
        text.starts_with("1 shell command was cut off when the program ended and may have partly run: shell #1 `cargo check --offline -p tools --tests --message-format=json`\n"),
        "{text}"
    );
    assert_eq!(super::acknowledged_cut_off(&result), [1]);
    // A read still running is cut off too, but only commands are named: they have effects.
    let reads = run(
        &call_line(1, "read", &json!({"path": "a"})),
        ScriptHost::new(Behavior::Hang),
        "x",
        60_000,
    )
    .await;
    assert!(
        !reads.content.contains("cut off when the program ended"),
        "{}",
        reads.content
    );
    assert!(super::acknowledged_cut_off(&reads).is_empty());
}

#[tokio::test]
async fn a_program_cannot_make_up_a_continuation_to_get_past_the_call_limit() {
    // Offsets and byte offsets nobody offered: each is an ordinary read, so after the
    // limit of 3 they are refused (with a pointer to aggregating in shell).
    let mut calls: Vec<(&str, Value)> = (0..3).map(|_| ("read", json!({"path": "a"}))).collect();
    calls.extend([
        ("read", json!({"path": "a", "offset": 2})),
        ("read", json!({"path": "a", "offset": 301})),
        (
            "read",
            json!({"path": "a", "offset": 1, "byte_offset": 16384}),
        ),
    ]);
    calls.push(("read", json!({"path": "b"})));
    let script = sequential_script(&calls);
    let host = ScriptHost::configured(Behavior::Exclusive, |host| {
        host.settings.max_tool_calls = 3;
    });

    let result = run(&script, Arc::clone(&host), "x", 60_000).await;

    assert_eq!(failed_seqs(&result), [4, 5, 6, 7], "{}", result.content);
    assert!(
        result
            .content
            .contains("tool call limit of 3 per run_js call exceeded"),
        "{}",
        result.content
    );
    assert!(
        host.seen
            .lock()
            .unwrap()
            .iter()
            .all(|(_, continuation)| !continuation),
        "nothing was a continuation"
    );
}

#[tokio::test]
async fn a_page_the_host_offered_is_free_once_and_nothing_else_is() {
    let calls = [
        // 1 counts. Its `next` offers offset 301, which 2 asks for; that offers 601, 3.
        ("read", json!({"path": "a"})),
        (
            "read",
            json!({"path": "a", "offset": 301, "byte_offset": 0}),
        ),
        ("read", json!({"path": "a", "offset": 601})),
        // 4 asks for 301 again (used up), 5 for another file's page 301, 6 goes back to
        // the first page: ordinary reads, so the limit of 3 is reached by 5 and 6 is
        // refused. 7 asks for 901, which 3 offered and nobody has used: still free.
        ("read", json!({"path": "a", "offset": 301})),
        ("read", json!({"path": "b", "offset": 301})),
        ("read", json!({"path": "a", "offset": 1})),
        ("read", json!({"path": "a", "offset": 901})),
    ];
    let host = ScriptHost::configured(Behavior::Exclusive, |host| {
        host.settings.max_tool_calls = 3;
        host.paged_reads = true;
    });

    let result = run(&sequential_script(&calls), Arc::clone(&host), "x", 60_000).await;

    assert_eq!(
        host.seen.lock().unwrap().clone(),
        [
            (1, false),
            (2, true),
            (3, true),
            (4, false),
            (5, false),
            (7, true)
        ],
        "the host is told which calls were continuations"
    );
    assert_eq!(failed_seqs(&result), [6], "{}", result.content);
}

#[tokio::test]
async fn pages_the_host_offered_are_bounded_too() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    // Follows every `next` it is given, one page after another, without end.
    let script = r#"echo $$ > PID_FILE
printf '%s\n' '{"type":"tool_call","id":1,"name":"read","args":{"path":"a"}}'
read -r _
i=1
while [ $i -le 500 ]; do
  printf '{"type":"tool_call","id":%s,"name":"read","args":{"path":"a","offset":%s}}\n' $((i + 1)) $((i * 300 + 1))
  read -r _
  i=$((i + 1))
done
exec sleep 600
"#
    .replace("PID_FILE", &pid_file.display().to_string());
    let host = ScriptHost::configured(Behavior::Exclusive, |host| {
        host.settings.max_tool_calls = 3;
        host.paged_reads = true;
    });

    let result = run(&script, host, "x", 60_000).await;

    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("kept calling tools after the limit of 3 calls"),
        "{}",
        result.content
    );
    let recorded = result.detail.unwrap()["run_js"]["calls"]
        .as_array()
        .unwrap()
        .len();
    assert!(recorded <= 3 * 10 + 32, "{recorded} calls were recorded");
    assert_gone(&pid_file);
}

/// Stands in for the dependency listing of the production start: a process that
/// writes its pid and says it is running, then is waited for until the start is
/// cancelled. It then finishes "successfully" — a preparation that completes just
/// as it is cancelled is the case that must not go on to start a worker.
struct StandInListing {
    pid_file: PathBuf,
    /// Written by the worker command if (wrongly) it is ever started.
    worker_marker: PathBuf,
}

impl StandInListing {
    fn launcher(
        &self,
        running: tokio::sync::oneshot::Sender<()>,
        release: Option<std::sync::mpsc::Receiver<()>>,
    ) -> WorkerLauncher {
        use std::io::Read;
        let pid_file = self.pid_file.clone();
        let marker = self.worker_marker.clone();
        let running = std::sync::Mutex::new(Some(running));
        let release = std::sync::Mutex::new(release);
        WorkerLauncher::preparing(move |cancel| {
            let sandbox = DirectSandbox::new_unchecked();
            let mut session = sandbox::StdioSession::spawn(
                &sandbox,
                sandbox::CommandSpec {
                    program: "sh".into(),
                    args: vec![
                        "-c".into(),
                        format!(
                            "echo $$ > {}; echo ready; exec sleep 600",
                            pid_file.display()
                        ),
                    ],
                    cwd: None,
                    extra_env: Vec::new(),
                },
            )
            .map_err(|error| error.to_string())?;
            let mut stdout = session.take_stdout().unwrap();
            let mut ready = [0u8; 6];
            stdout
                .read_exact(&mut ready)
                .map_err(|error| error.to_string())?;
            if let Some(running) = running.lock().unwrap().take() {
                let _ = running.send(());
            }
            if let Some(release) = release.lock().unwrap().take() {
                // A preparation that ignores the cancellation until released.
                let _ = release.recv();
            } else {
                // The production listing polls the flag the same way: it is blocked
                // reading a pipe, so there is no event to wait for instead.
                while !cancel.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
            }
            drop(session); // kills the process group and reaps it
            Ok(super::worker::Sandboxed {
                sandbox: Arc::new(DirectSandbox::new_unchecked()),
                spec: sandbox::CommandSpec {
                    program: "sh".into(),
                    args: vec!["-c".into(), format!("echo started > {}", marker.display())],
                    cwd: None,
                    extra_env: Vec::new(),
                },
            })
        })
    }
}

fn far_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(3600)
}

fn noop() -> Arc<dyn Fn(String) + Send + Sync> {
    Arc::new(|_| {})
}

#[tokio::test]
async fn the_deadline_kills_the_listing_in_progress_and_starts_no_worker() {
    let directory = tempfile::tempdir().unwrap();
    let listing = StandInListing {
        pid_file: directory.path().join("pid"),
        worker_marker: directory.path().join("worker"),
    };
    let (running, listing_running) = tokio::sync::oneshot::channel();
    let launcher = listing.launcher(running, None);
    // Only the first call below can see the deadline pass: it is the listing's own
    // report that it is running that sets it off.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(50);

    let outcome = launcher.launch(deadline, noop()).await;

    assert!(matches!(outcome, Err(super::worker::LaunchError::NotReady)));
    listing_running.await.unwrap();
    assert_gone(&listing.pid_file);
    assert!(
        !listing.worker_marker.exists(),
        "a worker was started after the cancellation"
    );
}

#[tokio::test]
async fn dropping_the_start_kills_the_listing_before_the_drop_returns() {
    let directory = tempfile::tempdir().unwrap();
    let listing = StandInListing {
        pid_file: directory.path().join("pid"),
        worker_marker: directory.path().join("worker"),
    };
    let (running, listing_running) = tokio::sync::oneshot::channel();
    let launcher = listing.launcher(running, None);
    let starting = tokio::spawn(async move { launcher.launch(far_deadline(), noop()).await });
    // The listing is running when this returns; nothing is raced.
    listing_running.await.unwrap();

    starting.abort();
    assert!(starting.await.is_err_and(|error| error.is_cancelled()));

    assert_gone(&listing.pid_file);
    assert!(
        !listing.worker_marker.exists(),
        "a worker was started after the cancellation"
    );
}

#[tokio::test]
async fn a_cancelled_start_that_cannot_be_confirmed_is_reported_to_the_host() {
    let directory = tempfile::tempdir().unwrap();
    let listing = StandInListing {
        pid_file: directory.path().join("pid"),
        worker_marker: directory.path().join("worker"),
    };
    let (running, listing_running) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let launcher = listing
        .launcher(running, Some(released))
        .with_cancel_confirmation(std::time::Duration::from_millis(100));
    let host = ScriptHost::new(Behavior::Exclusive);
    let report: Arc<dyn Fn(String) + Send + Sync> = {
        let host = Arc::clone(&host);
        Arc::new(move |detail| host.report_unconfirmed(&detail))
    };
    let starting = tokio::spawn({
        let report = Arc::clone(&report);
        async move { launcher.launch(far_deadline(), report).await }
    });
    listing_running.await.unwrap();

    starting.abort();
    assert!(starting.await.is_err_and(|error| error.is_cancelled()));

    assert_eq!(
        host.unconfirmed.lock().unwrap().len(),
        1,
        "the host was told"
    );
    // Let the preparation end so it does not outlive the test.
    drop(release);
}

#[tokio::test]
async fn a_worker_naming_a_huge_unknown_tool_is_stopped_without_copying_the_name() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    // 8 MB name, tiny arguments, and nothing is read back: repeated, this used to be
    // kept (several times over) in the records and in the refusal.
    let mut script = format!("echo $$ > {}\n", pid_file.display());
    for id in 1..=4 {
        script.push_str(&format!(
            "{{ printf '{{\"type\":\"tool_call\",\"id\":{id},\"name\":\"'; head -c 8000000 /dev/zero | tr '\\0' x; printf '\",\"args\":{{}}}}\\n'; }}\n"
        ));
    }
    script.push_str(&format!(
        "echo $$ > {}\nexec sleep 600\n",
        pid_file.display()
    ));
    let host = ScriptHost::new(Behavior::Exclusive);

    let result = run(&script, host.clone(), "x", 60_000).await;

    assert!(result.is_error);
    assert!(
        result.content.contains("does not provide"),
        "{}",
        result.content
    );
    assert!(
        result.content.len() < 2048,
        "{} bytes",
        result.content.len()
    );
    assert!(!result.content.contains("xxxxxxxx"));
    assert!(result.detail.unwrap().to_string().len() < 2048);
    assert_eq!(host.started.load(Ordering::SeqCst), 0);
    assert_gone(&pid_file);
}

#[tokio::test]
async fn a_known_tool_the_role_does_not_offer_is_refused_and_the_program_goes_on() {
    let mut script = String::new();
    script.push_str(&call_line(1, "git_diff", &json!({})));
    script.push_str("read -r reply\n");
    script.push_str(DONE);
    let host = ScriptHost::new(Behavior::Exclusive);

    let result = run(&script, host.clone(), "x", 60_000).await;

    assert!(!result.is_error, "{}", result.content);
    assert!(
        result
            .content
            .contains("git_diff #1: tools.git_diff is not available in this run"),
        "{}",
        result.content
    );
    assert_eq!(host.started.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn commands_that_could_not_be_confirmed_stopped_fail_the_call() {
    let script = DONE.to_owned();
    let host = ScriptHost::configured(Behavior::Exclusive, |host| {
        host.drain_error = Some("job-3 still running".into());
    });

    let result = run(&script, host.clone(), "x", 60_000).await;

    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains(
            "could not confirm that the commands it started have stopped (job-3 still running)"
        ),
        "{}",
        result.content
    );
    assert_eq!(
        host.unconfirmed.lock().unwrap().len(),
        1,
        "the host is told, not only the model"
    );
}

#[tokio::test]
async fn unfinished_calls_are_dropped_before_the_tool_returns() {
    let mut script = String::new();
    script.push_str(&call_line(
        1,
        "write",
        &json!({"path": "/w/half.rs", "content": "x"}),
    ));
    script.push_str(&call_line(2, "read", &json!({"path": "/w/behind.rs"})));
    // Finish only after the host has started the writer, so it is truly in flight.
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("started");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    script.push_str(&format!("read -r _ < {}\n", fifo.display()));
    script.push_str(DONE);
    let host = ScriptHost::new(Behavior::Hang);
    *host.signal.lock().unwrap() = Some(fifo);

    let result = run(&script, host.clone(), "x", 60_000).await;

    // The hanging writer was started and dropped; the read behind it never started.
    assert_eq!(host.dropped.load(Ordering::SeqCst), 1);
    // The host was asked to stop the commands only after the calls were gone.
    assert_eq!(host.dropped_when_drained.load(Ordering::SeqCst), 1);
    assert!(
        result
            .content
            .contains("write #1: cut off before it finished"),
        "{}",
        result.content
    );
    assert!(
        result.content.contains("read #2: never started"),
        "{}",
        result.content
    );
    assert!(
        result.content.contains("may have changed: /w/half.rs"),
        "{}",
        result.content
    );
    let detail = result.detail.unwrap();
    assert_eq!(detail["run_js"]["uncertain_paths"], json!(["/w/half.rs"]));
    assert_eq!(detail["run_js"]["calls"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn cancelling_the_tool_drops_the_calls_in_flight() {
    let mut script = String::new();
    script.push_str(&call_line(1, "read", &json!({})));
    script.push_str("exec sleep 600\n");
    let host = ScriptHost::new(Behavior::Hang);
    let ctx = ToolExecutionContext {
        run_id: "run".into(),
        call_id: Some("parent".into()),
        run_js_host: Some(host.clone()),
        ..Default::default()
    };
    let tool = RunJs::with_launcher(launcher(&script));
    let mut call =
        Box::pin(tool.execute_with_context(&ctx, json!({"code": "x", "timeout_ms": 60_000})));
    tokio::select! {
        _ = &mut call => panic!("the tool must not finish by itself"),
        () = async {
            while host.started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        } => {}
    }
    assert_eq!(host.dropped.load(Ordering::SeqCst), 0);

    drop(call);

    assert_eq!(
        host.dropped.load(Ordering::SeqCst),
        1,
        "dropped with the tool future"
    );
}
