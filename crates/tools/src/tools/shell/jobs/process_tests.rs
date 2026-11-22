//! Typed shell supervision: READY/output notifications, pidfd exits and a real job timeout.
use super::*;
use sandbox::{CommandSpec, DirectSandbox};
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
        ledger: None,
        stream_output: false,
        timeout_ms: None,
        state: Mutex::new(State {
            output: LiveOutput::default(),
            final_result: None,
            exposed: true,
            completion: None,
            teardown_confirmed: false,
            observed: false,
            completion_notified: false,
            artifact: None,
            artifact_notice: None,
            process: None,
            sandbox: None,
            delivered_cursor: 0,
            last_seen: None,
        }),
        teardown: Arc::default(),
        changed,
        registry_changes: watch::channel(0).0,
        cancel,
        input,
        escalated: None,
    });
    (job, received, cancelled)
}

fn stored_group(job: &Job) -> Option<JobGroup> {
    job.state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .process
        .clone()
}

async fn ready(
    job: &Job,
    task: &mut tokio::task::JoinHandle<Result<Completion, crate::ToolError>>,
) {
    let mut changed = job.changed.subscribe();
    loop {
        if job.state.lock().unwrap().output.text.contains("READY") {
            return;
        }
        tokio::select! {
            changed = changed.changed() => changed.unwrap(),
            result = &mut *task => match result {
                Ok(Ok(completion)) => panic!("supervisor ended before READY: {}", completion.status),
                Ok(Err(error)) => panic!("supervisor failed before READY: {error}"),
                Err(error) => panic!("supervisor task ended before READY: {error}"),
            },
        }
    }
}

// Also runs on assertion / observation-bound failure: abort is not just a
// dropped JoinHandle, and the registered typed capability stops the real peer.
struct StopTestSupervisor {
    job: Arc<Job>,
    task: tokio::task::AbortHandle,
}
impl Drop for StopTestSupervisor {
    fn drop(&mut self) {
        self.job.cancel_now();
        self.task.abort();
    }
}

// Given a real sh -c PTY caller, cancellation, timeout and abandonment must
// use its typed stop capability; the waiter confirms a receipt before completion.
#[tokio::test]
async fn pty_supervision_preserves_status_cancel_timeout_and_abandonment() {
    for action in ["exit", "signal", "eof", "cancel", "timeout", "abort"] {
        eprintln!("PTY_SUPERVISION {action}: starting");
        let (mut job, input, cancel) = job();
        let ledger = sandbox::SpawnLedger::new();
        Arc::get_mut(&mut job).unwrap().ledger = Some(ledger.clone());
        let wrapped = DirectSandbox::new_unchecked()
            .wrap(CommandSpec {
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    if action == "signal" {
                        "printf 'READY\n'; read value; kill -INT $$"
                    } else {
                        "printf 'READY\n'; read value || exit 42; exit 37"
                    }
                    .into(),
                ],
                cwd: None,
                extra_env: vec![],
            })
            .unwrap();
        let worker = job.clone();
        let mut task = tokio::spawn(async move {
            run_pty(
                &wrapped,
                &worker,
                input,
                cancel,
                if action == "timeout" {
                    Some(Duration::from_millis(200))
                } else {
                    None
                },
            )
            .await
        });
        let _cleanup = StopTestSupervisor {
            job: job.clone(),
            task: task.abort_handle(),
        };
        // Real PTY IO and the product timeout both use real time. This fixed
        // observation bound is failure-only (explicitly requested for this
        // regression); it is not a readiness delay or product grace period.
        tokio::time::timeout(Duration::from_secs(5), async {
            // The real job deadline starts before output is observed. Timeout
            // is valid even if launch / READY delivery takes longer than 200ms;
            // prove typed registration and the receipt below, not READY order
            // or a pidfd opened after the root might already have been reaped.
            let death = if action == "timeout" {
                None
            } else {
                ready(&job, &mut task).await;
                let root = ledger.children()[0].leader.unwrap();
                eprintln!("PTY_SUPERVISION {action}: READY root={root}");
                Some(process_guard_pidfd(root))
            };
            match action {
                "exit" | "signal" | "eof" => {
                    let (result, received) = oneshot::channel();
                    job.input
                        .send(Input {
                            bytes: if action == "eof" {
                                Vec::new()
                            } else {
                                b"finish\n".to_vec()
                            },
                            close: action == "eof",
                            result,
                        })
                        .await
                        .unwrap();
                    received.await.unwrap().unwrap();
                }
                "cancel" => job.cancel_now(),
                "timeout" => {} // Let the real 200 ms job deadline fire.
                "abort" => task.abort(),
                _ => unreachable!(),
            }
            eprintln!("PTY_SUPERVISION {action}: action delivered");
            if action == "abort" {
                assert!(task.await.err().expect("aborted supervisor").is_cancelled());
            } else {
                let completion = task.await.unwrap().unwrap();
                assert_eq!(
                    completion.status,
                    match action {
                        "exit" | "signal" | "eof" => "failed",
                        "cancel" => "cancelled",
                        _ => "timed_out",
                    }
                );
                if ["exit", "signal", "eof"].contains(&action) {
                    assert_eq!(
                        completion.exit_code,
                        Some(match action {
                            "signal" => 1,
                            "eof" => 42,
                            _ => 37,
                        })
                    );
                }
                if action == "timeout" {
                    assert!(completion.error.as_deref().unwrap().contains("200 ms"));
                }
            }
            eprintln!("PTY_SUPERVISION {action}: supervisor finished");
            if let Some(death) = death {
                tokio::task::spawn_blocking(move || {
                    use std::os::fd::AsRawFd;
                    let mut fds = [libc::pollfd {
                        fd: death.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    }];
                    loop {
                        let result = unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) };
                        if result > 0 {
                            break;
                        }
                        assert_eq!(
                            std::io::Error::last_os_error().kind(),
                            std::io::ErrorKind::Interrupted
                        );
                    }
                })
                .await
                .unwrap();
            }
            assert_eq!(ledger.child_stats().registered, 1);
            assert!(matches!(stored_group(&job), Some(JobGroup::Pty(_))));
            ledger.reap_all_children().await.unwrap();
            assert_eq!(ledger.child_stats().reaped, 1);
            assert_eq!(ledger.child_stats().unconfirmed, 0);
            assert!(stored_group(&job).unwrap().retired());
            eprintln!("PTY_SUPERVISION {action}: receipt confirmed");
        })
        .await
        .unwrap_or_else(|_| panic!("PTY_SUPERVISION {action}: 5 s observation bound exceeded"));
    }
}
fn process_guard_pidfd(pid: i32) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(fd >= 0);
    unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }
}
// Given: a pipe job's guard-owned group registered for stop, whose supervisor
// is not consuming the cancel signal / When: the job is cancelled from another
// thread / Then: the registered group is killed through its stop capability.
#[tokio::test]
async fn cancelling_a_job_kills_its_registered_owned_group() {
    use std::os::unix::process::ExitStatusExt;
    let (job, _input, _cancelled) = job();
    let mut command = sandbox::owned::OwnedCommand::new("sleep");
    command.arg("60");
    let mut child = sandbox::owned::OwnedChild::spawn_blocking(
        None,
        sandbox::ChildKind::ShellJob,
        None,
        command,
    )
    .await
    .unwrap();
    job.register_process(Some(JobGroup::Owned(child.group().clone())));
    job.cancel_now();
    let status = child.wait().await.unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert!(stored_group(&job).unwrap().retired());
}

#[tokio::test(flavor = "current_thread")]
async fn pty_writer_drop_never_writes_eot_on_the_async_worker() {
    struct Notice(Option<tokio::sync::oneshot::Sender<std::thread::ThreadId>>);
    impl std::io::Write for Notice {
        fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
            Ok(value.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Drop for Notice {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(std::thread::current().id());
        }
    }
    let worker = std::thread::current().id();
    let (sent, received) = tokio::sync::oneshot::channel();
    drop(super::process::PtyInputWriter::new(Box::new(Notice(Some(
        sent,
    )))));
    assert_ne!(received.await.unwrap(), worker);
}
