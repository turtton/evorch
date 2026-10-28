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

    async fn drain_children(&self, _parent_call_id: &str) -> Result<(), String> {
        self.dropped_when_drained
            .store(self.dropped.load(Ordering::SeqCst), Ordering::SeqCst);
        self.drain_error.clone().map_or(Ok(()), Err)
    }

    async fn call(&self, call: ChildCall) -> ChildOutcome {
        let _guard = CountDrop(Arc::clone(&self.dropped));
        self.started.fetch_add(1, Ordering::SeqCst);
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
        let mut outcome = ok_outcome(&[], &[]);
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

#[tokio::test]
async fn a_worker_that_never_reads_its_first_message_cannot_outlast_the_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    // The worker records its pid and never touches its stdin, so a program larger
    // than the pipe buffer would block a synchronous write for good.
    let script = format!("echo $$ > {}\nexec sleep 600\n", pid_file.display());
    let host = ScriptHost::new(Behavior::Exclusive);
    let big_program = "x".repeat(4 * 1024 * 1024);

    let result = run_with(
        RunJs::with_launcher(launcher_raw(&script)),
        host,
        &big_program,
        1,
    )
    .await;

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
    let host = ScriptHost::configured(Behavior::Exclusive, |host| host.reply_bytes = 600 * 1024);

    let result = run(&script, host, "x", 1).await;

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

#[tokio::test]
async fn starting_the_worker_is_bounded_by_the_deadline() {
    let (release, stalled) = std::sync::mpsc::channel();
    let host = ScriptHost::new(Behavior::Exclusive);

    let result = run_with(
        RunJs::with_launcher(WorkerLauncher::stalled(stalled)),
        host,
        "x",
        1,
    )
    .await;

    assert!(result.is_error);
    assert!(
        result.content.contains("was not ready within 1 ms"),
        "{}",
        result.content
    );
    // Let the stalled preparation finish so its thread does not outlive the test.
    drop(release);
}

#[tokio::test]
async fn commands_that_could_not_be_confirmed_stopped_fail_the_call() {
    let script = DONE.to_owned();
    let host = ScriptHost::configured(Behavior::Exclusive, |host| {
        host.drain_error = Some("job-3 still running".into());
    });

    let result = run(&script, host, "x", 60_000).await;

    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains(
            "could not confirm that the commands it started have stopped (job-3 still running)"
        ),
        "{}",
        result.content
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
