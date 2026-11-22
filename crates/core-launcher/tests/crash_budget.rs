//! The shared crash budget with real processes and the real filesystem. The
//! 5-minute window itself is unit-tested with an injected clock in
//! `src/budget.rs`; nothing here depends on elapsed time except a bounded wait
//! for process exit events. Every transition is acknowledged over the control
//! socket, as in `process.rs`.
use core_launcher::{CrashBudget, LaunchError, LaunchStage};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{cell::Cell, fs, os::unix::fs::PermissionsExt, path::Path};

mod support;
use support::*;

/// A child forked by another test holds a copy of a lock's open file description
/// until it execs, so a lock that was just released can look busy. The tests of
/// this file therefore run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn recent(r: &Rig) -> u32 {
    CrashBudget::recent_crashes(r.identity().config_dir()).unwrap()
}
fn flag(r: &Rig, name: &str) {
    fs::write(r.config.join(name), b"").unwrap();
}
/// A pidfd becomes readable when the process terminates.
fn wait_exit(peer: &mut Peer) {
    let limit = Timespec {
        tv_sec: 30,
        tv_nsec: 0,
    };
    let ready = poll(&mut [PollFd::new(&peer.fd, PollFlags::IN)], Some(&limit)).unwrap();
    assert_eq!(ready, 1, "process did not exit");
    peer.core = false;
}
/// One client that starts a core, connects, and exits. Returns the live core.
fn start_core(r: &Rig) -> Peer {
    let (mut client, mut peer) = r.client(&r.config);
    let mut core = r.spawn(&mut peer);
    r.ready(&mut core);
    peer.ack();
    peer.next("connected");
    peer.ack();
    assert!(client.0.wait().unwrap().success());
    core.core = true;
    core
}
/// A launcher inside the test process; returns the error and whether it spawned.
fn launch_in_process(r: &Rig) -> (LaunchError, bool) {
    let launcher = local_launcher(r);
    let spawned = Cell::new(false);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = rt.block_on(launcher.connect_or_start_observed(|stage| {
        if matches!(stage, LaunchStage::Spawned(_)) {
            spawned.set(true);
        }
    }));
    (
        result.err().expect("the start must be refused"),
        spawned.get(),
    )
}
fn nothing_is_running(r: &Rig) {
    let launch = core_instance::LaunchGuard::acquire(&r.identity()).unwrap();
    assert!(launch.lifetime_available().unwrap());
}

#[test]
fn normal_start_and_clean_exit_cycles_are_never_refused_or_counted() {
    let _serial = serial();
    let r = Rig::new();
    flag(&r, "exit-after-first-hello");
    let socket = r.identity().socket(&r.runtime).unwrap();
    // More cycles than the budget, each one a real spawn (r.spawn requires the
    // client to report before-spawn) and a real exit of the core process.
    for _ in 0..5 {
        let mut core = start_core(&r);
        wait_exit(&mut core);
        assert!(!socket.exists(), "a clean exit removes its socket");
        assert_eq!(recent(&r), 0);
    }
    nothing_is_running(&r);
}

#[test]
fn three_crashes_refuse_the_next_start_and_the_refusal_is_persistent() {
    let _serial = serial();
    let r = Rig::new();
    let mut core = start_core(&r);
    for crashed_so_far in 1..=2 {
        core.kill();
        // The next client sees the left-over socket, counts the crash once and
        // is still allowed to start (fewer than three crashes).
        core = start_core(&r);
        assert_eq!(recent(&r), crashed_so_far);
    }
    core.kill();
    let (mut client, mut peer) = r.client(&r.config);
    peer.next("before-spawn");
    peer.ack();
    assert!(!client.0.wait().unwrap().success());
    assert_eq!(recent(&r), 3);
    nothing_is_running(&r);

    // A new client (another process lifetime) is refused from the persisted
    // record, with a reason for a person, and starts nothing.
    for _ in 0..2 {
        let (error, spawned) = launch_in_process(&r);
        assert!(error.is_budget_blocked(), "{error}");
        assert!(!spawned);
        let text = error.to_string();
        assert!(text.contains("3 core crashes"), "{text}");
        assert!(text.contains("crash-budget"), "{text}");
        assert_eq!(recent(&r), 3, "a refusal neither resets nor adds a crash");
        nothing_is_running(&r);
    }
}

#[test]
fn cores_that_die_before_ready_are_counted_so_a_startup_loop_stops() {
    let _serial = serial();
    let r = Rig::new();
    flag(&r, "fail-before-ready");
    for failed in 1..=3 {
        let (mut client, mut peer) = r.client(&r.config);
        let mut core = r.spawn(&mut peer);
        core.ack();
        wait_exit(&mut core);
        peer.ack();
        assert!(!client.0.wait().unwrap().success());
        assert_eq!(recent(&r), failed);
    }
    let (error, spawned) = launch_in_process(&r);
    assert!(error.is_budget_blocked(), "{error}");
    assert!(!spawned);
    nothing_is_running(&r);
}

#[test]
fn simultaneous_reconnects_after_one_crash_start_one_core_and_count_one_crash() {
    let _serial = serial();
    let r = Rig::new();
    let mut crashed = start_core(&r);
    crashed.kill();
    let (mut a, mut pa) = r.client(&r.config);
    let (mut b, mut pb) = r.client(&r.config);
    // r.client already released each client; wait for their elections.
    pa.receive();
    pb.receive();
    if pa.event["stage"] == "waiting-launch" {
        std::mem::swap(&mut pa, &mut pb);
    }
    pa.expect("before-spawn");
    pb.expect("waiting-launch");
    pa.ack();
    pa.next("spawned");
    let mut core = r.accept("inherited");
    core.ack();
    core.next("adopted");
    r.ready(&mut core);
    pa.ack();
    pb.ack();
    pa.next("connected");
    pb.next("connected");
    assert_eq!(pa.event["value"], pb.event["value"]);
    pa.ack();
    pb.ack();
    assert!(a.0.wait().unwrap().success());
    assert!(b.0.wait().unwrap().success());
    assert_eq!(recent(&r), 1);
}

#[test]
fn an_untrusted_record_stops_automatic_starts_and_is_left_untouched() {
    let _serial = serial();
    let r = Rig::new();
    let record = r.config.join(".evorch-core.crash-budget");
    let other = r.config.join("elsewhere");
    for case in ["garbage", "mode", "symlink"] {
        let _ = fs::remove_file(&record);
        match case {
            "garbage" => {
                fs::write(&record, b"{ not json").unwrap();
                fs::set_permissions(&record, fs::Permissions::from_mode(0o600)).unwrap();
            }
            "mode" => {
                fs::write(&record, br#"{"version":1,"crashes_ms":[]}"#).unwrap();
                fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap();
            }
            _ => {
                fs::write(&other, br#"{"version":1,"crashes_ms":[]}"#).unwrap();
                fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();
                std::os::unix::fs::symlink(&other, &record).unwrap();
            }
        }
        let before = fs::read(&record).unwrap();
        let (error, spawned) = launch_in_process(&r);
        assert!(error.is_budget_blocked(), "{case}: {error}");
        assert!(!spawned, "{case}");
        let text = error.to_string();
        assert!(
            text.contains("cannot be trusted") && text.contains("remove the file"),
            "{text}"
        );
        assert_eq!(fs::read(&record).unwrap(), before, "{case}");
        assert!(Path::new(&record).symlink_metadata().is_ok());
        nothing_is_running(&r);
    }
}

#[test]
fn a_core_that_rejects_the_version_is_reported_as_incompatible_and_left_running() {
    let _serial = serial();
    use std::io::{Read, Write};
    use std::os::unix::{fs::PermissionsExt as _, net::UnixListener};
    use workbench_protocol::handshake::{
        HandshakeRejected, HandshakeReply, ProtocolVersion, RecommendedAction, RejectReason,
    };
    let r = Rig::new();
    let path = r.identity().socket(&r.runtime).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut len = [0; 4];
        stream.read_exact(&mut len).unwrap();
        let mut hello = vec![0; u32::from_be_bytes(len) as usize];
        stream.read_exact(&mut hello).unwrap();
        let reply = HandshakeReply::Rejected(HandshakeRejected {
            reason: RejectReason::UnsupportedVersion,
            message: "protocol 1 is not supported".into(),
            supported_versions: vec![ProtocolVersion::CURRENT],
            missing_capabilities: vec![],
            recommended_action: RecommendedAction::UpgradeClient,
        });
        stream
            .write_all(&workbench_protocol::framing::encode_frame(&reply, 1024 * 1024).unwrap())
            .unwrap();
        // Still serving after the rejection: it must not have been stopped.
        let mut rest = [0; 16];
        let _ = stream.read(&mut rest);
        listener
    });
    let (error, spawned) = launch_in_process(&r);
    assert!(error.is_incompatible(), "{error}");
    assert!(!spawned);
    let text = error.to_string();
    assert!(
        text.contains("NOT stopped") && text.contains("UnsupportedVersion"),
        "{text}"
    );
    assert_eq!(recent(&r), 0, "an incompatible core is not a crash");
    let _listener = server.join().unwrap();
    assert!(path.exists(), "the socket of the running core is untouched");
}

/// Restores the config directory so the temporary directory can be removed.
struct Writable(std::path::PathBuf);
impl Drop for Writable {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
    }
}
fn make_unwritable(r: &Rig) -> Writable {
    fs::set_permissions(&r.config, fs::Permissions::from_mode(0o500)).unwrap();
    Writable(r.config.clone())
}
fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

#[test]
fn a_crash_that_cannot_be_recorded_is_refused_and_its_evidence_is_kept() {
    let _serial = serial();
    if is_root() {
        return;
    }
    let r = Rig::new();
    let socket = r.identity().socket(&r.runtime).unwrap();
    let mut core = start_core(&r);
    core.kill();
    assert!(socket.exists(), "a killed core leaves its socket");
    let _restore = make_unwritable(&r);
    let (error, spawned) = launch_in_process(&r);
    assert!(error.is_budget_blocked(), "{error}");
    assert!(!spawned, "no core is started on an unrecorded crash");
    assert!(
        error.to_string().contains("cannot be trusted"),
        "the reason names the record: {error}"
    );
    assert!(socket.exists(), "the evidence stays until it is recorded");
    assert_eq!(recent(&r), 0, "nothing was written");
}

#[test]
fn a_failed_start_that_cannot_be_recorded_is_reported_as_a_record_fault() {
    let _serial = serial();
    if is_root() {
        return;
    }
    let r = Rig::new();
    flag(&r, "fail-before-ready");
    // The lock files exist before the directory stops accepting new files.
    let launch = core_instance::LaunchGuard::acquire(&r.identity()).unwrap();
    assert!(launch.lifetime_available().unwrap());
    drop(launch);
    let _restore = make_unwritable(&r);
    let (error, spawned) = std::thread::scope(|scope| {
        // The core announces itself, is released, and exits before ready.
        scope.spawn(|| {
            let mut core = r.accept("inherited");
            core.ack();
            core.next("adopted");
            core.ack();
            wait_exit(&mut core);
        });
        launch_in_process(&r)
    });
    assert!(spawned);
    assert!(
        error.is_budget_blocked(),
        "the exit is reported with the record fault, not as a plain exit: {error}"
    );
    assert_eq!(recent(&r), 0);
}
