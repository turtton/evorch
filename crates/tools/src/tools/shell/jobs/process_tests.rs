//! Process group lifetimes of pipe and PTY jobs (Linux: `/proc`).
//!
//! The group must be stopped while its exited leader still reserves the group
//! id, and every stored handle must be retired once the leader is reaped, so a
//! later stop cannot signal a reused id.
use super::*;
use sandbox::{CommandSpec, DirectSandbox};
use std::path::Path;
use std::time::Instant;

fn job() -> (Arc<Job>, mpsc::Receiver<Input>, watch::Receiver<bool>) {
    let (changed, _) = watch::channel(0);
    let (cancel, cancelled) = watch::channel(false);
    let (input, received) = mpsc::channel(4);
    let job = Arc::new(Job {
        id: "process-test-job".into(),
        handle: "job-0".into(),
        owner: "owner".into(),
        call_id: None,
        thread: None,
        synchronous: false,
        timeout_ms: None,
        state: Mutex::new(State {
            output: LiveOutput::default(),
            final_result: None,
            exposed: true,
            completion: None,
            observed: false,
            completion_notified: false,
            artifact: None,
            artifact_notice: None,
            process: None,
            sandbox: None,
            delivered_cursor: 0,
            last_seen: None,
        }),
        changed,
        cancel,
        input,
        escalated: None,
    });
    (job, received, cancelled)
}

fn stored_group(job: &Job) -> Option<sandbox::process::ProcessGroup> {
    job.state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .process
        .clone()
}

fn running(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A helper that ignores the terminal hangup and watches the job's shell
/// (`$$`): `kill -0` keeps succeeding on the exited, unreaped shell, so the
/// helper writes `reaped-first` only if the shell is reaped while the helper
/// still runs. It reports its pid once it watches.
fn watcher(dir: &Path) -> String {
    let dir = dir.display();
    format!(
        "( trap '' HUP; while kill -0 $$ 2>/dev/null; do :; done; echo yes > '{dir}/reaped-first' ) & \
         echo $! > '{dir}/helper.tmp' && mv '{dir}/helper.tmp' '{dir}/helper.pid'"
    )
}

async fn run(
    interactive: bool,
    script: String,
    dir: &Path,
    stop: bool,
) -> (Completion, Arc<Job>, String) {
    let wrapped = DirectSandbox::new_unchecked()
        .wrap(CommandSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script],
            cwd: Some(dir.to_path_buf()),
            extra_env: Vec::new(),
        })
        .unwrap();
    let (job, input, cancel) = job();
    let worker = Arc::clone(&job);
    let task = tokio::spawn(async move {
        let timeout = Duration::from_secs(60);
        if interactive {
            run_pty(&wrapped, &worker, input, cancel, Some(timeout)).await
        } else {
            run_pipe(&wrapped, &worker, input, cancel, Some(timeout)).await
        }
    });
    let pid_file = dir.join("helper.pid");
    wait_until("the helper pid", || {
        std::fs::read_to_string(&pid_file).is_ok_and(|pid| pid.ends_with('\n'))
    })
    .await;
    let helper = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_owned();
    if stop {
        // The stop action: cancel the job and kill its group from here.
        job.cancel_now();
    }
    let completion = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("the job ends")
        .unwrap()
        .unwrap();
    (completion, job, helper)
}

async fn assert_retired_after_reap(job: &Job, dir: &Path, helper: &str) {
    // The group was stopped before the shell was reaped: the helper dies
    // without ever seeing the shell vanish.
    wait_until("the helper to stop", || !running(helper)).await;
    assert!(
        !dir.join("reaped-first").exists(),
        "the shell was reaped while its group still ran"
    );
    // The handle stored for stop is retired by the reap, and a later stop or
    // guard drop through it signals nothing.
    let stored = stored_group(job).expect("the job stored its group");
    assert_eq!(stored.leader(), None);
    job.cancel_now();
    drop(sandbox::process::ProcessGroupGuard::new(stored.clone()));
    assert_eq!(stored.leader(), None);
}

// Given: a job whose shell exits while a helper still holds its output /
// When: the job ends on its own / Then: the helper is stopped before the
// shell is reaped, and the stored group handle is retired.
#[tokio::test]
async fn a_finished_job_stops_its_group_before_reaping_and_retires_it() {
    for interactive in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let script = format!("{}; exit 0", watcher(dir.path()));
        let (completion, job, helper) = run(interactive, script, dir.path(), false).await;
        assert_eq!(completion.status, "completed", "interactive: {interactive}");
        assert_eq!(completion.exit_code, Some(0));
        assert_retired_after_reap(&job, dir.path(), &helper).await;
    }
}

// Given: a running job with a helper / When: it is stopped / Then: the group
// is stopped before the shell is reaped, and the stored handle is retired.
#[tokio::test]
async fn a_stopped_job_stops_its_group_before_reaping_and_retires_it() {
    for interactive in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let script = format!("{}; exec sleep 60", watcher(dir.path()));
        let (completion, job, helper) = run(interactive, script, dir.path(), true).await;
        assert_eq!(completion.status, "cancelled", "interactive: {interactive}");
        assert_retired_after_reap(&job, dir.path(), &helper).await;
    }
}
