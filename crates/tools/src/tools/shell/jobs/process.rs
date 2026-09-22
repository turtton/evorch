//! Process and PTY supervision for shell jobs.
use super::super::{ProcessGroup, PtyKiller, io_failed, spawn_failed};
use super::{Completion, Input, Job};
use crate::ToolError;
use sandbox::WrappedCommand;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

pub(super) async fn run_pipe(
    wrapped: &WrappedCommand,
    job: &Arc<Job>,
    mut input: mpsc::Receiver<Input>,
    mut cancel: watch::Receiver<bool>,
    timeout: Duration,
) -> Result<Completion, ToolError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if *cancel.borrow() {
        return Ok(Completion {
            status: "cancelled",
            exit_code: None,
            error: None,
        });
    }
    let mut command = tokio::process::Command::new(&wrapped.program);
    command
        .args(&wrapped.args)
        .env_clear()
        .envs(wrapped.env.iter().cloned())
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped())
        .process_group(0);
    if let Some(cwd) = &wrapped.cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .map_err(|error| spawn_failed(&wrapped.program, error))?;
    let group = ProcessGroup(
        child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(id as i32)),
    );
    job.register_process(group.0);
    let result = async {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io_failed("missing stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io_failed("missing stderr"))?;
        let mut stdin = child.stdin.take();
        let timer = tokio::time::sleep(timeout);
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
                _ = async { let _ = cancel.wait_for(|value| *value).await; } => { group.kill(); let _ = child.wait().await; return Ok(Completion { status: "cancelled", exit_code: None, error: None }); }
                _ = &mut timer => { group.kill(); let _ = child.wait().await; return Ok(Completion { status: "timed_out", exit_code: None, error: Some(format!("timed out after {} ms; process group stopped", timeout.as_millis())) }); }
                status = child.wait(), if exit.is_none() => {
                    exit = Some(status.map_err(io_failed)?.code().unwrap_or(-1));
                    // A shell exiting must not leave detached descendants holding the
                    // output pipes or mutating the workspace past its lease.
                    group.kill();
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
    group.kill();
    let _ = child.wait().await;
    result
}

pub(super) async fn run_pty(
    wrapped: &WrappedCommand,
    job: &Arc<Job>,
    mut input: mpsc::Receiver<Input>,
    mut cancel: watch::Receiver<bool>,
    timeout: Duration,
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
    command.env_clear();
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
    let group = ProcessGroup(
        child
            .process_id()
            .and_then(|id| rustix::process::Pid::from_raw(id as i32)),
    );
    job.register_process(group.0);
    let mut killer = PtyKiller(child.clone_killer());
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
    let mut waiting = tokio::task::spawn_blocking(move || child.wait());
    let mut writing: Option<tokio::task::JoinHandle<()>> = None;
    let timer = tokio::time::sleep(timeout);
    tokio::pin!(timer);
    let mut reaped = false;
    let completion = loop {
        tokio::select! {
            biased;
            _ = async { let _ = cancel.wait_for(|value| *value).await; } => { break Completion { status: "cancelled", exit_code: None, error: None }; }
            _ = &mut timer => { break Completion { status: "timed_out", exit_code: None, error: Some(format!("timed out after {} ms; process group stopped", timeout.as_millis())) }; }
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
    let _ = killer.0.kill();
    drop(pair.master);
    // The group is dead before releasing any workspace guard, even when a
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
