#![cfg(target_os = "linux")]
use sandbox::{
    ChildKind, SpawnLedger,
    owned::{OwnedChild, OwnedCommand, OwnedSession},
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::process::ExitStatusExt,
    process::Stdio,
};
fn command(script: &str) -> OwnedCommand {
    let mut c = OwnedCommand::new("/bin/sh");
    c.args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}
fn line(reader: &mut impl BufRead) -> String {
    let mut s = String::new();
    assert!(reader.read_line(&mut s).unwrap() > 0);
    s.trim().to_owned()
}
fn death(pid: u32) {
    use std::os::fd::AsRawFd;
    let fd = process_guard::pidfd_open(pid as i32).unwrap();
    process_guard::wire::poll(
        &mut [process_guard::wire::fd(fd.as_raw_fd(), libc::POLLIN)],
        None,
    )
    .unwrap();
}
#[test]
fn session_stdio_status_errno_and_target_environment() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = command(
        r#"read value; printf '%s:%s:%s' "$value" "$ONLY" "$PWD"; printf err >&2; exit 37"#,
    );
    c.env_clear().env("ONLY", "target").current_dir(dir.path());
    let result = OwnedSession::spawn(&mut c)
        .unwrap()
        .output(Some(b"hello\n"))
        .unwrap();
    assert_eq!(result.status.code(), Some(37));
    assert_eq!(
        result.stdout,
        format!("hello:target:{}", dir.path().display()).as_bytes()
    );
    assert_eq!(result.stderr, b"err");
    for script in [
        "exit 0",
        "exit 37",
        "kill -TERM $$",
        "kill -KILL $$",
        "ulimit -c 0; kill -ABRT $$",
    ] {
        let expected = std::process::Command::new("/bin/sh")
            .args(["-c", script])
            .status()
            .unwrap();
        let mut child = OwnedSession::spawn(&mut command(script)).unwrap();
        assert_eq!(child.wait().unwrap().into_raw(), expected.into_raw());
        assert_eq!(child.wait().unwrap().into_raw(), expected.into_raw());
    }
    for path in [dir.path().join("missing"), dir.path().join("noexec")] {
        if path.ends_with("noexec") {
            std::fs::write(&path, b"x").unwrap();
        }
        let expected = std::process::Command::new(&path)
            .spawn()
            .unwrap_err()
            .raw_os_error();
        let actual = OwnedSession::spawn(&mut OwnedCommand::new(&path))
            .unwrap_err()
            .raw_os_error();
        assert_eq!(actual, expected);
    }
}
#[test]
fn sync_try_wait_creator_thread_and_drop_are_owned() {
    let mut session = std::thread::spawn(|| {
        OwnedSession::spawn(&mut command("echo READY; read gate; echo DONE; exit 37")).unwrap()
    })
    .join()
    .unwrap();
    let mut stdout = BufReader::new(session.take_stdout().unwrap());
    assert_eq!(line(&mut stdout), "READY");
    assert!(session.try_wait().unwrap().is_none());
    session.take_stdin().unwrap().write_all(b"GO\n").unwrap();
    assert_eq!(line(&mut stdout), "DONE");
    assert_eq!(session.wait().unwrap().code(), Some(37));
    assert_eq!(session.try_wait().unwrap().unwrap().code(), Some(37));
    let mut session = OwnedSession::spawn(&mut command("echo READY; read gate")).unwrap();
    let pid = session.id().unwrap();
    let mut stdout = BufReader::new(session.take_stdout().unwrap());
    assert_eq!(line(&mut stdout), "READY");
    drop(session);
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
}
#[tokio::test]
async fn async_cancelled_wait_and_exited_preserve_two_stages() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut child = OwnedChild::spawn(&mut command("echo READY; read gate; exit 37")).unwrap();
    let mut stdout = tokio::io::BufReader::new(child.take_stdout().unwrap()).lines();
    assert_eq!(stdout.next_line().await.unwrap().unwrap(), "READY");
    // Poll exactly once; the future must send sticky Wait before yielding.
    {
        let mut wait = Box::pin(child.wait());
        std::future::poll_fn(|cx| {
            assert!(wait.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }
    child
        .take_stdin()
        .unwrap()
        .write_all(b"GO\n")
        .await
        .unwrap();
    assert_eq!(child.wait().await.unwrap().code(), Some(37));
    assert_eq!(child.wait().await.unwrap().code(), Some(37));
    let mut child = OwnedChild::spawn(&mut command("exit 23")).unwrap();
    let pid = child.id().unwrap();
    child.exited().await.unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    assert!(stat.rsplit_once(") ").unwrap().1.starts_with("Z "));
    assert_eq!(child.wait().await.unwrap().code(), Some(23));
}
#[tokio::test]
async fn ledger_drop_reaper_confirms_target_not_helper_status() {
    use tokio::io::AsyncBufReadExt;
    let ledger = SpawnLedger::new();
    let mut child = ledger
        .admit()
        .unwrap()
        .spawn_owned(
            ChildKind::Eval,
            Some("typed"),
            &mut command("echo READY; read gate"),
        )
        .unwrap();
    let mut out = tokio::io::BufReader::new(child.take_stdout().unwrap()).lines();
    assert_eq!(out.next_line().await.unwrap().unwrap(), "READY");
    assert_eq!(ledger.child_stats().registered, 1);
    let id = ledger.children()[0].id;
    drop(child);
    ledger.reap_child(id).await.unwrap();
    assert_eq!(ledger.child_stats().reaped, 1);
    assert_eq!(ledger.child_stats().unconfirmed, 0);
}
#[tokio::test]
async fn all_kinds_and_unmanaged_have_guard_control() {
    use tokio::io::AsyncBufReadExt;
    for kind in [
        ChildKind::McpStdio,
        ChildKind::SandboxSession,
        ChildKind::PullRequestSource,
        ChildKind::ShellJob,
        ChildKind::Eval,
        ChildKind::Probe,
        ChildKind::Renderer,
        ChildKind::Other,
    ] {
        let ledger = SpawnLedger::new();
        let mut child = ledger
            .admit()
            .unwrap()
            .spawn_owned(kind, None, &mut command("echo READY; read gate"))
            .unwrap();
        let mut out = tokio::io::BufReader::new(child.take_stdout().unwrap()).lines();
        assert_eq!(out.next_line().await.unwrap().unwrap(), "READY");
        assert_eq!(ledger.children()[0].kind, kind);
        ledger.stop_all_children();
        assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGKILL));
        ledger.reap_all_children().await.unwrap();
    }
    let mut child = OwnedChild::spawn(&mut command("echo READY; read gate")).unwrap();
    let mut out = tokio::io::BufReader::new(child.take_stdout().unwrap()).lines();
    assert_eq!(out.next_line().await.unwrap().unwrap(), "READY");
    assert_eq!(
        child.kill_and_wait().await.unwrap().signal(),
        Some(libc::SIGKILL)
    );
}
#[tokio::test]
async fn normal_stop_is_term_and_guard_loss_is_unconfirmed() {
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    use tokio::io::AsyncBufReadExt;
    let ledger = SpawnLedger::new();
    let mut child = ledger
        .admit()
        .unwrap()
        .spawn_owned(ChildKind::Eval, None, &mut command("echo READY; read gate"))
        .unwrap();
    let mut out = tokio::io::BufReader::new(child.take_stdout().unwrap()).lines();
    assert_eq!(out.next_line().await.unwrap().unwrap(), "READY");
    ledger.terminate_child(ledger.children()[0].id).unwrap();
    assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGTERM));
    // Guard attacks are outside the guarantee: loss must not become cleanup success.
    let mut child = ledger
        .admit()
        .unwrap()
        .spawn_owned(
            ChildKind::Other,
            None,
            &mut command("echo READY; read gate"),
        )
        .unwrap();
    let mut out = tokio::io::BufReader::new(child.take_stdout().unwrap()).lines();
    assert_eq!(out.next_line().await.unwrap().unwrap(), "READY");
    let pid = child.id().unwrap();
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let guard = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_eq!(unsafe { libc::kill(guard, libc::SIGKILL) }, 0);
    assert!(
        child
            .wait()
            .await
            .unwrap_err()
            .to_string()
            .contains("GuardLost")
    );
    let id = ledger.children()[0].id;
    drop(child);
    assert!(ledger.reap_child(id).await.is_err());
    // Root PDEATHSIG is part of the low-level contract. pidfd is a death event,
    // not a timing assertion; PID 1 reaping is explicitly outside the guarantee.
    if std::path::Path::new(&format!("/proc/{pid}")).exists() {
        death(pid);
    }
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, &mut status, 0) },
        pid as i32
    );
    assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
}

#[tokio::test]
async fn retired_control_clone_preserves_cached_final() {
    let mut child = OwnedChild::spawn(&mut command("exit 37")).unwrap();
    let group = child.group().clone();
    assert_eq!(child.wait().await.unwrap().code(), Some(37));
    group.terminate().unwrap();
    group.kill().unwrap();
    assert_eq!(child.wait().await.unwrap().code(), Some(37));
    assert!(group.id().is_none());
}
#[test]
fn wrapped_typed_command_and_non_utf8_target_environment() {
    use sandbox::Sandbox;
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let wrapped = sandbox::DirectSandbox::new_unchecked()
        .wrap(sandbox::CommandSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "printf '%s' \"$RAW\"".into()],
            cwd: None,
            extra_env: vec![],
        })
        .unwrap();
    let mut command = wrapped.to_owned_command();
    command
        .env("RAW", OsString::from_vec(vec![b'x', 255]))
        .stdout(Stdio::piped());
    let out = OwnedSession::spawn(&mut command)
        .unwrap()
        .output(None)
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, vec![b'x', 255]);
}

#[tokio::test]
async fn final_drained_by_control_clone_still_confirms_ledger_on_drop() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let ledger = SpawnLedger::new();
    let mut child = ledger
        .admit()
        .unwrap()
        .spawn_owned(
            ChildKind::Other,
            None,
            &mut command("echo READY; read gate; exit 37"),
        )
        .unwrap();
    let id = ledger.children()[0].id;
    let pid = child.id().unwrap();
    let guard = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.take_stdout().unwrap()).lines();
    assert_eq!(lines.next_line().await.unwrap().unwrap(), "READY");
    let group = child.group().clone();
    {
        let mut waiting = Box::pin(child.wait());
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx)))
                .await
                .is_pending()
        );
    }
    child
        .take_stdin()
        .unwrap()
        .write_all(b"GO\n")
        .await
        .unwrap();
    death(guard); // helper exit event guarantees its FinalStatus is queued.
    group.terminate().unwrap();
    assert!(group.id().is_none());
    drop(child);
    assert!(ledger.reap_child(id).await.is_ok());
}
