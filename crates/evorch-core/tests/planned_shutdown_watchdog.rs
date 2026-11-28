#![cfg(target_os = "linux")]
//! Actual feature-free main::serve negative cases; nextest owns hang detection.
#[path = "planned_shutdown_watchdog/support.rs"]
mod support;
fn setup(idle: u64) -> (tempfile::TempDir, test_socket_dir::SocketDir) {
    let root = tempfile::tempdir().unwrap();
    let sockets = test_socket_dir::SocketDir::new();
    std::fs::create_dir(root.path().join("config")).unwrap();
    std::fs::write(
        root.path().join("config/config.toml"),
        format!("[core]\nidle_grace_secs={idle}\n"),
    )
    .unwrap();
    (root, sockets)
}
#[test]
fn cold_shutdown_without_capture_preserves_zero_and_joins_watcher() {
    let (root, sockets) = setup(3600);
    let mut child = support::Child(
        support::command(root.path(), sockets.path())
            .spawn()
            .unwrap(),
    );
    let (_out, _) = support::ready(&mut child);
    let identity = core_instance::ConfigIdentity::new(&root.path().join("config")).unwrap();
    let record = identity.read_record().unwrap().unwrap();
    support::hello(&record.socket, Some(record.epoch), identity.identity());
    support::term(&child);
    support::wait(&mut child, 0);
}
#[test]
fn cold_idle_without_capture_preserves_zero_and_joins_watcher() {
    let (root, sockets) = setup(0);
    let mut child = support::Child(
        support::command(root.path(), sockets.path())
            .spawn()
            .unwrap(),
    );
    let (_out, _) = support::ready(&mut child);
    support::wait(&mut child, 0);
}
#[test]
fn cold_startup_web_bind_error_preserves_one_not_seventy_four() {
    let (root, sockets) = setup(3600);
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        root.path().join("config/config.toml"),
        format!(
            "[web]\nenabled=true\nport={}\n",
            occupied.local_addr().unwrap().port()
        ),
    )
    .unwrap();
    let mut child = support::Child(
        support::command(root.path(), sockets.path())
            .spawn()
            .unwrap(),
    );
    let error = support::wait(&mut child, 1);
    assert!(error.contains("Address already in use"), "{error}");
}

#[test]
fn cold_listener_accept_error_without_capture_keeps_original_error() {
    let (root, sockets) = setup(3600);
    let mut child = support::Child(
        support::command(root.path(), sockets.path())
            .spawn()
            .unwrap(),
    );
    let (_out, _) = support::ready(&mut child);
    let identity = core_instance::ConfigIdentity::new(&root.path().join("config")).unwrap();
    let record = identity.read_record().unwrap().unwrap();
    let pid = rustix::process::Pid::from_raw(child.0.id() as i32).unwrap();
    // OS-only fault after Ready: existing listener remains valid but accept(2)
    // cannot allocate a descriptor. No product callback or injection interface.
    rustix::process::prlimit(
        Some(pid),
        rustix::process::Resource::Nofile,
        rustix::process::Rlimit {
            current: Some(3),
            maximum: Some(3),
        },
    )
    .unwrap();
    let _connection = std::os::unix::net::UnixStream::connect(record.socket).unwrap();
    let error = support::wait(&mut child, 1);
    // Previously OwnerHost::begin_quiesce reopened owners.db during cleanup:
    // that EMFILE failure was wrapped as "durable intent" and masked accept's
    // error. No owner DB is opened now, so preserve the original accept EMFILE
    // and code 1 (checked above), not the planned-watchdog exit code 74.
    assert!(
        error.contains("evorch-core: Too many open files (os error 24)"),
        "{error}"
    );
}
