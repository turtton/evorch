#![cfg(target_os = "linux")]
#[path = "pty/ownership.rs"]
mod ownership;
#[path = "pty/protocol.rs"]
mod protocol;
use process_guard::{GuardChild, GuardRuntime, LaunchSpec, PtyMaster, PtySize, pidfd_open, wire};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::{fd::AsRawFd, unix::process::ExitStatusExt},
    path::PathBuf,
    process::Command,
};

fn runtime() -> GuardRuntime {
    GuardRuntime::from_bundle(&PathBuf::from(
        std::env::var_os(process_guard::BUNDLE_ENV).expect("prepare bundle"),
    ))
    .unwrap()
}
fn size() -> PtySize {
    PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}
struct Target {
    _dir: tempfile::TempDir,
    program: PathBuf,
}
impl Target {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("fixtures/pty-target.c");
        let program = dir.path().join("target");
        assert!(
            Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
                .args(["-Wall", "-Wextra", "-Werror", "-o"])
                .arg(&program)
                .arg(source)
                .status()
                .unwrap()
                .success()
        );
        Self { _dir: dir, program }
    }
    fn spawn(&self, mode: &str) -> (GuardChild, PtyMaster) {
        let mut spec = LaunchSpec::new(&self.program).unwrap();
        spec.argv.push(mode.as_bytes().to_vec());
        runtime().spawn_pty(&spec, size()).unwrap()
    }
}
fn line(output: &mut impl BufRead) -> String {
    let mut text = String::new();
    assert!(output.read_line(&mut text).unwrap() > 0, "PTY EOF");
    text.trim().to_owned()
}
fn until(output: &mut impl BufRead, prefix: &str) -> String {
    loop {
        let text = line(output);
        if text.starts_with(prefix) {
            return text;
        }
    }
}
fn state(text: &str, tag: &str, helper: u32) -> i32 {
    eprintln!("PTY_EXEC helper={helper}: {text}");
    let fields: Vec<_> = text.split_whitespace().collect();
    assert_eq!(fields[0], tag);
    let ids: Vec<i32> = fields[1..].iter().map(|s| s.parse().unwrap()).collect();
    assert_eq!(ids[1], helper as i32, "helper must be session leader");
    assert_ne!(ids[2], helper as i32, "helper must be outside target PGID");
    assert_eq!(ids[2], ids[3], "target must be foreground before exec");
    assert_eq!(ids[4..], [1, 1, 1]);
    ids[0]
}
fn dead(fd: &impl AsRawFd) {
    wire::poll(&mut [wire::fd(fd.as_raw_fd(), libc::POLLIN)], None).unwrap();
}

#[test]
fn tty_foreground_resize_fd_signals_and_sticky_raw_status() {
    let target = Target::new();
    let (mut child, master) = target.spawn("io");
    let mut output = BufReader::new(master.try_clone_reader().unwrap());
    let mut input = master.take_writer().unwrap();
    assert!(master.take_writer().is_err(), "writer must be unique");
    assert_eq!(line(&mut output), "FDS 0");
    let root = state(&line(&mut output), "READY", child.id());
    assert_eq!(root, child.target_id() as i32);
    assert!(
        std::fs::read(format!("/proc/{}/environ", child.id()))
            .unwrap()
            .is_empty(),
        "helper environment must be completely empty"
    );
    assert!(child.try_wait().unwrap().is_none());
    master
        .resize(PtySize {
            rows: 41,
            cols: 109,
            ..size()
        })
        .unwrap();
    input.write_all(b"SIZE\n").unwrap();
    assert_eq!(until(&mut output, "SIZE "), "SIZE 41 109");
    child.request_wait().unwrap();
    input.write_all(b"EXIT\n").unwrap();
    assert_eq!(child.wait().unwrap().into_raw(), 37 << 8);
    assert_eq!(child.try_wait().unwrap().unwrap().into_raw(), 37 << 8);
    assert!(child.is_reaped());
}

#[test]
fn ctrl_c_hits_target_not_helper_and_eof_is_preserved() {
    let target = Target::new();
    let (mut child, master) = target.spawn("io");
    let mut output = BufReader::new(master.try_clone_reader().unwrap());
    let mut input = master.take_writer().unwrap();
    state(&until(&mut output, "READY "), "READY", child.id());
    input.write_all(&[3]).unwrap();
    assert_eq!(child.exited().unwrap().signal(), Some(libc::SIGINT));
    assert!(!child.is_reaped());
    // exited() did not retire the helper; wait now does.
    assert_eq!(
        unsafe { libc::getsid(child.id() as i32) },
        child.id() as i32
    );
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGINT));
    let (mut child, master) = target.spawn("io");
    let mut output = BufReader::new(master.try_clone_reader().unwrap());
    let input = master.take_writer().unwrap();
    until(&mut output, "READY ");
    drop(input);
    assert_eq!(until(&mut output, "EOF"), "EOF");
    assert_eq!(child.wait().unwrap().code(), Some(42));
    let mut rest = String::new();
    output.read_to_string(&mut rest).unwrap();
}

#[test]
fn master_close_delivers_hup_without_losing_guard() {
    let target = Target::new();
    let (mut child, master) = target.spawn("io");
    let mut output = BufReader::new(master.try_clone_reader().unwrap());
    until(&mut output, "READY ");
    drop((output, master));
    let status = child.wait().unwrap();
    // Closing the master races terminal EOF against the forwarded SIGHUP.
    assert!(
        status.signal() == Some(libc::SIGHUP) || status.code() == Some(42),
        "{status}"
    );
    assert!(child.is_reaped());
}

#[test]
fn exec_errors_are_errno_and_never_return_an_owner() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-executable");
    std::fs::write(&path, "x").unwrap();
    for (program, expected) in [
        (dir.path().join("absent"), libc::ENOENT),
        (path, libc::EACCES),
    ] {
        let spec = LaunchSpec::new(program).unwrap();
        assert_eq!(
            rt.spawn_pty(&spec, size()).unwrap_err().raw_os_error(),
            Some(expected)
        );
    }
}

#[test]
fn guard_loss_is_not_final_status() {
    if !ownership::controller("guard_loss_is_not_final_status", "guard-loss") {
        return;
    }
    let mut reaper = ownership::Reaper::new();
    let target = Target::new();
    let (mut child, master) = target.spawn("io");
    let mut output = BufReader::new(master.try_clone_reader().unwrap());
    until(&mut output, "READY ");
    reaper.add(child.target_id() as i32);
    let root = pidfd_open(child.target_id() as i32).unwrap();
    let helper = pidfd_open(child.id() as i32).unwrap();
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                helper.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        },
        0
    );
    dead(&helper);
    assert!(child.wait().unwrap_err().to_string().contains("GuardLost"));
    assert!(!child.is_reaped());
    dead(&root);
}

#[test]
fn pinned_image_survives_distribution_path_replacement() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bundle = PathBuf::from(std::env::var_os(process_guard::BUNDLE_ENV).unwrap());
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&bundle).unwrap()).unwrap();
    let helper = dir.path().join("evorch-process-guard");
    std::fs::copy(
        bundle
            .parent()
            .unwrap()
            .join(receipt["helper"]["file"].as_str().unwrap()),
        &helper,
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o555)).unwrap();
    receipt["helper"]["file"] = "evorch-process-guard".into();
    let manifest = dir.path().join("guard-bundle.json");
    std::fs::write(&manifest, serde_json::to_vec(&receipt).unwrap()).unwrap();
    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o444)).unwrap();
    let rt = GuardRuntime::from_bundle(&manifest).unwrap();
    let target = Target::new();
    let mut spec = LaunchSpec::new(&target.program).unwrap();
    spec.argv.push(b"io".to_vec());
    let (mut child, master) = rt.spawn_pty(&spec, size()).unwrap();
    let mut output = BufReader::new(master.try_clone_reader().unwrap());
    until(&mut output, "READY ");
    child.force_kill().unwrap();
    child.wait().unwrap();
    eprintln!("PTY_EXEC pinned original reached READY before replacement");
    let replacement = dir.path().join("replacement");
    std::fs::copy(&helper, &replacement).unwrap();
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o555)).unwrap();
    std::fs::rename(replacement, &helper).unwrap();
    let (mut child, master) = rt.spawn_pty(&spec, size()).unwrap();
    let mut output = BufReader::new(master.try_clone_reader().unwrap());
    until(&mut output, "READY ");
    child.force_kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
}

#[test]
fn proc_locator_denial_fails_closed_without_pathname_fallback() {
    if std::env::var_os("PTY_DENY_PROC").is_some() {
        let target = Target::new();
        let spec = LaunchSpec::new(&target.program).unwrap();
        assert_eq!(
            runtime()
                .spawn_pty(&spec, size())
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EACCES)
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/pty-deny-proc.c");
    let library = dir.path().join("deny-proc.so");
    assert!(
        Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(&library)
            .arg(source)
            .arg("-ldl")
            .status()
            .unwrap()
            .success()
    );
    // Test-only syscall interposition in an isolated owner, not a product flag.
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "proc_locator_denial_fails_closed_without_pathname_fallback",
            "--nocapture",
        ])
        .env("PTY_DENY_PROC", "1")
        .env("LD_PRELOAD", library)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
