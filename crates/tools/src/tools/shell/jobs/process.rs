//! Process and PTY supervision for shell jobs.
use super::super::{io_failed, spawn_failed};
use super::{Completion, Input, Job};
use crate::ToolError;
use sandbox::WrappedCommand;
use sandbox::process::{GroupChild, ProcessGroup, ProcessGroupGuard};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

pub(super) async fn run_pipe(
    wrapped: &WrappedCommand,
    job: &Arc<Job>,
    mut input: mpsc::Receiver<Input>,
    mut cancel: watch::Receiver<bool>,
    timeout: Option<Duration>,
) -> Result<Completion, ToolError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if *cancel.borrow() {
        return Ok(Completion {
            status: "cancelled",
            exit_code: None,
            error: None,
        });
    }
    let mut command = tokio::process::Command::from(wrapped.to_command());
    command
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(if job.synchronous {
            std::process::Stdio::null()
        } else {
            std::process::Stdio::piped()
        });
    let mut child =
        GroupChild::spawn(&mut command).map_err(|error| spawn_failed(&wrapped.program, error))?;
    job.register_process(Some(child.group().clone()));
    let result = async {
        let mut stdout = child
            .take_stdout()
            .ok_or_else(|| io_failed("missing stdout"))?;
        let mut stderr = child
            .take_stderr()
            .ok_or_else(|| io_failed("missing stderr"))?;
        let mut stdin = child.take_stdin();
        let timer = deadline(timeout);
        tokio::pin!(timer);
        let mut out = [0u8; 8192];
        let mut err = [0u8; 8192];
        let mut out_open = true;
        let mut err_open = true;
        let mut exit = None;
        loop {
            if exit.is_some() && !out_open && !err_open {
                break;
            }
            tokio::select! {
                biased;
                _ = async { let _ = cancel.wait_for(|value| *value).await; } => { let _ = child.kill_and_wait().await; return Ok(Completion { status: "cancelled", exit_code: None, error: None }); }
                _ = &mut timer => { let _ = child.kill_and_wait().await; return Ok(Completion { status: "timed_out", exit_code: None, error: Some(format!("timed out after {} ms; process group stopped", timeout.expect("timer only resolves with a timeout").as_millis())) }); }
                // A shell exiting must not leave detached descendants holding the
                // output pipes or mutating the workspace after completion: waiting
                // kills them before it reaps the shell.
                status = child.wait(), if exit.is_none() => {
                    exit = Some(status.map_err(io_failed)?.code().unwrap_or(-1));
                    stdin = None;
                }
                request = input.recv() => if let Some(request) = request {
                    let written = match stdin.as_mut() {
                        Some(writer) => tokio::time::timeout(Duration::from_secs(2), writer.write_all(&request.bytes)).await.map_err(|_| "stdin write timed out; input may be partial".to_owned()).and_then(|result| result.map_err(|error| error.to_string())),
                        None => Err("stdin is closed".into()),
                    };
                    if request.close { stdin = None; }
                    let _ = request.result.send(written);
                },
                read = stdout.read(&mut out), if out_open => { let count = read.map_err(io_failed)?; out_open = count > 0; job.push(0, &out[..count]); }
                read = stderr.read(&mut err), if err_open => { let count = read.map_err(io_failed)?; err_open = count > 0; job.push(1, &err[..count]); }
            }
        }
        let code = exit.unwrap_or(-1);
        Ok(Completion {
            status: if code == 0 { "completed" } else { "failed" },
            exit_code: Some(code),
            error: None,
        })
    }.await;
    let _ = child.kill_and_wait().await;
    result
}

pub(super) async fn run_pty(
    wrapped: &WrappedCommand,
    job: &Arc<Job>,
    mut input: mpsc::Receiver<Input>,
    mut cancel: watch::Receiver<bool>,
    timeout: Option<Duration>,
) -> Result<Completion, ToolError> {
    if *cancel.borrow() {
        return Ok(Completion {
            status: "cancelled",
            exit_code: None,
            error: None,
        });
    }
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(io_failed)?;
    let mut command = portable_pty::CommandBuilder::new(&wrapped.program);
    command.args(&wrapped.args);
    if !wrapped.inherit_env {
        command.env_clear();
    }
    for (key, value) in &wrapped.env {
        command.env(key, value);
    }
    command.cwd(
        wrapped
            .cwd
            .clone()
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)
            .map_err(io_failed)?,
    );
    let mut reader = pair.master.try_clone_reader().map_err(io_failed)?;
    let writer = Arc::new(Mutex::new(Some(
        pair.master.take_writer().map_err(io_failed)?,
    )));
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| spawn_failed(&wrapped.program, error))?;
    drop(pair.slave);
    if job.synchronous {
        *writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    let group = ProcessGroup::new(child.process_id());
    // Cancelling the job future stops the PTY session's group; once the waiter
    // has reaped the shell, nothing is signalled.
    let _stop_on_drop = ProcessGroupGuard::new(group.clone());
    job.register_process(Some(group.clone()));
    let output_job = Arc::clone(job);
    let reader_task = tokio::task::spawn_blocking(move || -> Result<(), ToolError> {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => output_job.push(0, &buffer[..count]),
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => return Err(io_failed(error)),
            }
        }
        Ok(())
    });
    let reaper = group.clone();
    let mut waiting = tokio::task::spawn_blocking(move || {
        // The exited shell stays unreaped until what is left of its group is
        // killed, so the group id cannot have been reused by then.
        let Some(leader) = reaper.leader() else {
            return child.wait();
        };
        if sandbox::process::wait_for_exit(leader).is_err() {
            reaper.release();
            return child.wait();
        }
        reaper
            .kill_and_reap(|| child.wait().map(Some))?
            .ok_or_else(|| std::io::Error::other("the shell was not reaped"))
    });
    let mut writing: Option<tokio::task::JoinHandle<()>> = None;
    let timer = deadline(timeout);
    tokio::pin!(timer);
    let mut reaped = false;
    let completion = loop {
        tokio::select! {
            biased;
            _ = async { let _ = cancel.wait_for(|value| *value).await; } => { break Completion { status: "cancelled", exit_code: None, error: None }; }
            _ = &mut timer => { break Completion { status: "timed_out", exit_code: None, error: Some(format!("timed out after {} ms; process group stopped", timeout.expect("timer only resolves with a timeout").as_millis())) }; }
            status = &mut waiting => {
                reaped = true;
                break match status.map_err(io_failed).and_then(|status| status.map_err(io_failed)) {
                    Ok(status) => { let code = status.exit_code() as i32; Completion { status: if code == 0 { "completed" } else { "failed" }, exit_code: Some(code), error: None } },
                    Err(error) => Completion { status: "failed", exit_code: None, error: Some(error.to_string()) },
                };
            }
            _ = async { if let Some(task) = writing.as_mut() { let _ = task.await; } else { std::future::pending::<()>().await; } } => { writing = None; }
            request = input.recv(), if writing.is_none() => if let Some(request) = request {
                let writer = Arc::clone(&writer);
                // Blocking PTY writes run off the async executor. Cancellation
                // kills the peer, making a blocked writer return an error.
                writing = Some(tokio::task::spawn_blocking(move || {
                    let mut writer = writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let result = match writer.as_mut() {
                        Some(writer) => writer.write_all(&request.bytes).and_then(|()| {
                            if request.close { writer.write_all(&[4])?; }
                            writer.flush()
                        }).map_err(|error| error.to_string()),
                        None => Err("stdin is closed".into()),
                    };
                    if request.close { *writer = None; }
                    let _ = request.result.send(result);
                }));
            },
        }
    };
    group.kill();
    drop(pair.master);
    // The group is dead before the job reports completion, even when a
    // portable-pty reader/waiter takes a moment to observe EOF.
    if !reaped {
        let _ = waiting.await;
    }
    let read_result = reader_task.await;
    if let Some(writing) = writing {
        let _ = writing.await;
    }
    read_result.map_err(io_failed)??;
    Ok(completion)
}

async fn deadline(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending::<()>().await,
    }
}
