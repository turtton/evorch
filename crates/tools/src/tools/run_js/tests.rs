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

#[tokio::test]
async fn the_pages_after_the_first_of_a_read_do_not_count_toward_the_call_limit() {
    // Three first-page reads reach the limit of 3; five continuation pages follow, then a
    // fourth first-page read, which is refused with a pointer to aggregating in shell.
    let mut script = String::new();
    let mut id = 0;
    for _ in 0..3 {
        id += 1;
        script.push_str(&call_line(id, "read", &json!({"path": "a"})));
        script.push_str("read -r _\n");
    }
    for page in 0..5 {
        id += 1;
        let args = if page % 2 == 0 {
            json!({"path": "a", "offset": 301 + page})
        } else {
            json!({"path": "a", "offset": 1, "byte_offset": 16384})
        };
        script.push_str(&call_line(id, "read", &args));
        script.push_str("read -r _\n");
    }
    id += 1;
    script.push_str(&call_line(id, "read", &json!({"path": "b"})));
    // The program's value is the reply to the refused call.
    script.push_str(
        r#"read -r reply
esc=$(printf '%s' "$reply" | sed 's/\\/\\\\/g; s/"/\\"/g')
printf '{"type":"done","value":"%s"}\n' "$esc"
"#,
    );
    let host = ScriptHost::configured(Behavior::Exclusive, |host| {
        host.settings.max_tool_calls = 3;
    });

    let result = run(&script, host, "x", 60_000).await;

    let detail = result.detail.clone().unwrap();
    let calls = detail["run_js"]["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 9, "{}", result.content);
    let failed: Vec<u64> = calls
        .iter()
        .filter(|call| call["is_error"] == true)
        .map(|call| call["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(failed, [9], "only the call beyond the limit fails");
    assert!(
        result
            .content
            .contains("tool call limit of 3 per run_js call exceeded"),
        "{}",
        result.content
    );
    assert!(
        result.content.contains("wc, grep -c, awk"),
        "{}",
        result.content
    );
    assert_eq!(detail["run_js"]["limit_hit"], "tool_calls");
}

#[tokio::test]
async fn continuation_pages_are_bounded_too() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    // Only continuation pages, and never a reply read: they must not run forever.
    let script = format!(
        "echo $$ > {}\ni=1\nwhile [ $i -le 500 ]; do\n  printf '%s\\n' \"{{\\\"type\\\":\\\"tool_call\\\",\\\"id\\\":$i,\\\"name\\\":\\\"read\\\",\\\"args\\\":{{\\\"path\\\":\\\"a\\\",\\\"offset\\\":2}}}}\"\n  i=$((i+1))\ndone\nexec sleep 600\n",
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
