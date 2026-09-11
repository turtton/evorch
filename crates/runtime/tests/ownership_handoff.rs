#![cfg(unix)]

use runtime::ownership::ipc::{Request, Response, request, serve_connection};
use runtime::ownership::{Lease, Registry, ThreadOwner};

#[test]
fn ipc_handoff_requires_checkpoint_and_fences_previous_owner() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let socket = directory.path().join("owner.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("listener");
    let mut owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "first".into(),
            generation: 1,
            expires_at: 100,
        },
    );
    let token = owner.lease.clone();
    owner.begin_turn(&token, 1).expect("turn");
    let mut registry = Registry::open(&path).expect("registry");
    registry.start(&owner).expect("start");
    let worker = std::thread::spawn(move || {
        let mut registry = Registry::open(&path).expect("registry");
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept");
            serve_connection(&mut stream, &mut registry, 20).expect("serve");
        }
    });
    let settings = config::OwnershipConfig::default();
    let handoff = Request::Handoff {
        thread_id: "thread".into(),
        token: token.clone(),
        successor_id: "second".into(),
        settings,
    };
    assert!(matches!(
        request(&socket, &handoff).expect("active request"),
        Response::Rejected(_)
    ));
    registry
        .update("thread", |owner| owner.checkpoint(&token))
        .expect("checkpoint");
    assert!(
        matches!(request(&socket, &handoff).expect("handoff"), Response::Status(owner) if owner.lease.owner_id == "second" && owner.lease.generation == 2)
    );
    assert!(matches!(
        request(
            &socket,
            &Request::Heartbeat {
                thread_id: "thread".into(),
                token
            }
        )
        .expect("old heartbeat"),
        Response::Rejected(_)
    ));
    worker.join().expect("worker");
}
