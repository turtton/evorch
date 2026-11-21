use super::*;
use std::{
    net::Shutdown,
    os::{
        fd::{FromRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    process::{Child, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

struct Raw {
    helper: Child,
    death: OwnedFd,
    stream: UnixStream,
    decoder: wire::Decoder,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    _runtime: GuardRuntime,
}
impl Raw {
    fn new(preload: Option<&std::path::Path>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let rt = runtime();
        let name = format!(
            "pty-protocol-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let listener =
            UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap())
                .unwrap();
        let pair = portable_pty::native_pty_system().openpty(size()).unwrap();
        let peer = unsafe {
            libc::ioctl(
                pair.master.as_raw_fd().unwrap(),
                libc::TIOCGPTPEER,
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            )
        };
        assert!(peer >= 0);
        let slave = unsafe { std::fs::File::from_raw_fd(peer) };
        let [dev, ino] = rt.image().identity_args();
        let mut command = Command::new(rt.image().pty_exec_path());
        command
            .args([
                "--launch-pty",
                &dev,
                &ino,
                &std::process::id().to_string(),
                &unsafe { libc::getuid() }.to_string(),
                &name,
                "fixture",
            ])
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        if let Some(preload) = preload {
            command.env("LD_PRELOAD", preload);
        }
        let helper = command.spawn().unwrap();
        drop(pair.slave);
        let death = pidfd_open(helper.id() as i32).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stream = loop {
            wire::poll(
                &mut [
                    wire::fd(listener.as_raw_fd(), libc::POLLIN),
                    wire::fd(death.as_raw_fd(), libc::POLLIN),
                ],
                None,
            )
            .unwrap();
            assert_eq!(
                unsafe { libc::waitpid(helper.id() as i32, std::ptr::null_mut(), libc::WNOHANG) },
                0
            );
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("{e}"),
            }
        };
        stream.set_nonblocking(true).unwrap();
        let mut result = Self {
            helper,
            death,
            stream,
            decoder: wire::Decoder::default(),
            _master: pair.master,
            _runtime: rt,
        };
        let (kind, hello) = result.recv().unwrap();
        assert_eq!(kind, wire::Kind::Hello);
        assert_eq!(
            hello,
            format!("{}:fixture", process_guard::COMPAT_ID).as_bytes()
        );
        result
    }
    fn recv(&mut self) -> std::io::Result<(wire::Kind, Vec<u8>)> {
        wire::receive(
            &mut self.stream,
            &mut self.decoder,
            self.death.as_raw_fd(),
            None,
        )
    }
    fn launch(&mut self, spec: LaunchSpec, mode: wire::LaunchMode) {
        let bytes = serde_json::to_vec(&wire::LaunchRequest { mode, spec }).unwrap();
        for chunk in bytes.chunks(wire::FRAME_LIMIT) {
            wire::send(&mut self.stream, wire::Kind::LaunchChunk, chunk, None).unwrap();
        }
        wire::send(&mut self.stream, wire::Kind::LaunchEnd, &[], None).unwrap();
    }
    fn commit(&mut self) {
        assert_eq!(self.recv().unwrap(), (wire::Kind::Ready, Vec::new()));
        assert!(
            std::fs::read_to_string(format!(
                "/proc/{}/task/{}/children",
                self.helper.id(),
                self.helper.id()
            ))
            .unwrap()
            .trim()
            .is_empty(),
            "fork before Commit"
        );
        wire::send(&mut self.stream, wire::Kind::Commit, &[], None).unwrap();
    }
}
impl Drop for Raw {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
        let _ = self.helper.wait();
    }
}

#[test]
fn backend_mismatch_and_disconnect_before_commit_never_fork() {
    let target = Target::new();
    let spec = LaunchSpec::new(&target.program).unwrap();
    let mut raw = Raw::new(None);
    raw.launch(spec.clone(), wire::LaunchMode::Direct);
    assert!(raw.recv().is_err());
    assert_eq!(raw.helper.wait().unwrap().code(), Some(125));
    let mut raw = Raw::new(None);
    raw.launch(spec, wire::LaunchMode::Pty);
    assert_eq!(raw.recv().unwrap(), (wire::Kind::Ready, Vec::new()));
    assert!(
        std::fs::read_to_string(format!(
            "/proc/{}/task/{}/children",
            raw.helper.id(),
            raw.helper.id()
        ))
        .unwrap()
        .trim()
        .is_empty()
    );
    raw.stream.shutdown(Shutdown::Write).unwrap();
    assert_eq!(raw.helper.wait().unwrap().code(), Some(125));
}

#[test]
fn foreground_failure_reports_spawn_errno_without_exec() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("foreground.c");
    let library = dir.path().join("foreground.so");
    std::fs::write(&source,
        "#include <errno.h>\n#include <sys/types.h>\nint tcsetpgrp(int fd,pid_t pg) {(void)fd;(void)pg;errno=ENOTTY;return -1;}\n").unwrap();
    assert!(
        Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args(["-shared", "-fPIC", "-o"])
            .arg(&library)
            .arg(source)
            .status()
            .unwrap()
            .success()
    );
    let marker = dir.path().join("EXECUTED");
    let mut spec =
        LaunchSpec::new(std::env::var_os("EVORCH_TEST_SHELL").unwrap_or_else(|| "/bin/sh".into()))
            .unwrap();
    spec.argv.extend([
        b"-c".to_vec(),
        format!("echo exec > '{}'", marker.display()).into_bytes(),
    ]);
    let mut raw = Raw::new(Some(&library));
    raw.launch(spec, wire::LaunchMode::Pty);
    raw.commit();
    let (kind, payload) = raw.recv().unwrap();
    assert_eq!(kind, wire::Kind::SpawnError);
    assert_eq!(wire::integer(&payload).unwrap(), libc::ENOTTY);
    assert!(
        !marker.exists(),
        "foreground failure must not execute target"
    );
}

#[test]
fn disconnect_after_exec_is_force_kill_and_final_is_not_descendant_reap() {
    if !ownership::controller(
        "protocol::disconnect_after_exec_is_force_kill_and_final_is_not_descendant_reap",
        "disconnect",
    ) {
        return;
    }
    let mut reaper = ownership::Reaper::new();
    let target = Target::new();
    let mut spec = LaunchSpec::new(&target.program).unwrap();
    spec.argv.push(b"io".to_vec());
    let mut raw = Raw::new(None);
    reaper.add(raw.helper.id() as i32);
    raw.launch(spec, wire::LaunchMode::Pty);
    raw.commit();
    let (kind, pid) = raw.recv().unwrap();
    assert_eq!(kind, wire::Kind::ExecOk);
    let pid = wire::integer(&pid).unwrap();
    reaper.add(pid);
    let death = pidfd_open(pid).unwrap();
    let mut output = BufReader::new(raw._master.try_clone_reader().unwrap());
    until(&mut output, "READY ");
    raw.stream.shutdown(Shutdown::Write).unwrap();
    loop {
        let (kind, payload) = raw.recv().unwrap();
        if kind == wire::Kind::FinalStatus {
            assert_eq!(
                std::process::ExitStatus::from_raw(wire::integer(&payload).unwrap()).signal(),
                Some(libc::SIGKILL)
            );
            break;
        }
        assert_eq!(kind, wire::Kind::RootExited);
    }
    assert!(raw.helper.wait().unwrap().success());
    dead(&death);
}

fn rejected_before_hello(wrong_image: bool) {
    let mut reaper = ownership::Reaper::new();
    let rt = runtime();
    let name = format!("pty-identity-only-{}", std::process::id());
    let listener =
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (dev, ino) = rt.image().identity();
    // Exactly one credential is wrong; listener, parent PID and other credential
    // are valid, so a later connect/authentication failure cannot mask this check.
    let ino = ino.wrapping_add(u64::from(wrong_image));
    let uid = unsafe { libc::getuid() }.wrapping_add(u32::from(!wrong_image));
    let mut helper = Command::new(rt.image().pty_exec_path())
        .args([
            "--launch-pty",
            &dev.to_string(),
            &ino.to_string(),
            &std::process::id().to_string(),
            &uid.to_string(),
            &name,
            "known-nonce",
        ])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    reaper.add(helper.id() as i32);
    let death = pidfd_open(helper.id() as i32).unwrap();
    let mut events = [
        wire::fd(listener.as_raw_fd(), libc::POLLIN),
        wire::fd(death.as_raw_fd(), libc::POLLIN),
    ];
    wire::poll(&mut events, None).unwrap();
    if events[0].revents != 0 {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        let (kind, _) = wire::receive(
            &mut stream,
            &mut wire::Decoder::default(),
            death.as_raw_fd(),
            None,
        )
        .unwrap();
        panic!("invalid credential reached {kind:?} (wrong_image={wrong_image})");
    }
    assert_ne!(events[1].revents & libc::POLLIN, 0);
    assert_eq!(helper.wait().unwrap().code(), Some(125));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn image_mismatch_alone_is_rejected_before_hello() {
    if ownership::controller(
        "protocol::image_mismatch_alone_is_rejected_before_hello",
        "image-only",
    ) {
        rejected_before_hello(true);
    }
}

#[test]
fn uid_mismatch_alone_is_rejected_before_hello() {
    if ownership::controller(
        "protocol::uid_mismatch_alone_is_rejected_before_hello",
        "uid-only",
    ) {
        rejected_before_hello(false);
    }
}
