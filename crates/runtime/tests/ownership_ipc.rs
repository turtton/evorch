#![cfg(unix)]

use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};

use runtime::ownership::ipc::{Request, Response, request, serve_connection};
use runtime::ownership::{Lease, Registry, ThreadOwner};

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        match self.0.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
}

#[test]
fn child_owner_process() {
    let Some(path) = std::env::var_os("EVORCH_OWNERSHIP_TEST_ROOT") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let mut registry = Registry::open(&path.join("owners.db")).expect("registry");
    let listener = UnixListener::bind(path.join("owner.sock")).expect("listen");
    println!("READY");
    for _ in 0..2 {
        let (mut stream, _) = listener.accept().expect("accept");
        serve_connection(&mut stream, &mut registry, 10).expect("serve");
    }
}

#[test]
fn two_process_attach_and_quiesce_preserve_generation() {
    use std::io::{BufRead, BufReader};
    let directory = tempfile::tempdir().expect("directory");
    let mut registry = Registry::open(&directory.path().join("owners.db")).expect("registry");
    let lease = Lease {
        owner_id: "child".into(),
        generation: 1,
        expires_at: 100,
    };
    let mut owner = ThreadOwner::new("thread-1".into(), lease.clone());
    owner
        .begin_run(&lease, "abandoned-run", 1)
        .expect("begin run");
    registry.start(&owner).expect("start");
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "child_owner_process", "--nocapture"])
            .env("EVORCH_OWNERSHIP_TEST_ROOT", directory.path())
            .stdout(Stdio::piped())
            .spawn()
            .expect("child"),
    );
    let stdout = child.0.stdout.take().expect("stdout");
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) if line.trim() == "READY" => {
                    let _ = ready_tx.send(());
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("child ready");
    let socket = directory.path().join("owner.sock");
    let attached = request(
        &socket,
        &Request::Attach {
            thread_id: "thread-1".into(),
        },
    )
    .expect("attach");
    assert!(matches!(attached, Response::Status(owner) if owner.lease == lease));
    let quiesced = request(
        &socket,
        &Request::Quiesce {
            thread_id: "thread-1".into(),
            token: lease.clone(),
        },
    )
    .expect("quiesce");
    assert!(
        matches!(quiesced, Response::Status(owner) if owner.state == runtime::ownership::OwnerState::Quiescing)
    );
    assert!(child.0.wait().expect("exit").success());
    reader.join().expect("reader");

    let mut registry = Registry::open(&directory.path().join("owners.db")).expect("registry");
    let claimed = runtime::ownership::ipc::claim(
        &mut registry,
        runtime::ownership::ipc::ClaimRequest {
            thread_id: "thread-1",
            expected: &lease,
            previous_socket: &socket,
            owner_id: "parent",
            now_ms: 200,
            grace_ms: 50,
        },
    )
    .expect("claim after child exits");
    assert_eq!(claimed.lease.generation, 2);
    assert_eq!(claimed.lease.owner_id, "parent");
    assert!(!claimed.active_turn);
    assert!(claimed.active_runs.is_empty());
}
