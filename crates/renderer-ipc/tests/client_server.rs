//! The real client against the real server with a fake backend.

mod common;

use common::*;
use renderer_ipc::*;
use serde_json::json;

#[test]
fn a_tab_is_opened_operated_and_closed_and_shutdown_is_acknowledged() {
    let Connected {
        client,
        helper,
        fake,
    } = connect_fake();
    let (open, body) = OpenArtifact::with_files(
        SessionId(1),
        TabId(1),
        Viewport {
            width: 64,
            height: 48,
        },
        30,
        5000,
        vec![
            (
                "index.html".into(),
                "text/html".into(),
                b"<img src=a.png>".to_vec(),
            ),
            ("a.png".into(), "image/png".into(), vec![1, 2, 3]),
        ],
    );
    let viewport = client.open_artifact(open, &body).unwrap();
    assert_eq!((viewport.width, viewport.height), (64, 48));

    assert_eq!(
        client
            .cdp(
                TabId(1),
                "Runtime.evaluate",
                json!({"op": "echo", "n": 1}),
                1000
            )
            .unwrap(),
        json!({"op": "echo", "n": 1})
    );
    assert_eq!(client.capture_png(TabId(1), 1000).unwrap(), b"\x89PNG fake");
    client.settle(TabId(1), 1000).unwrap();
    client
        .resize(
            TabId(1),
            Viewport {
                width: 80,
                height: 60,
            },
        )
        .unwrap();
    let notices = client.take_notices(TabId(1)).unwrap();
    assert_eq!(
        notices.blocked,
        ["request http://x (artifacts are offline)"]
    );
    client.close_tab(TabId(1)).unwrap();

    assert_eq!(client.shutdown(NEVER), CloseReason::ShutdownAcknowledged);
    assert_eq!(helper.join().unwrap(), ExitReason::ShutdownRequested);
    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        [
            "open 1 [index.html:text/html:15,a.png:image/png:3]",
            "cdp 1 Runtime.evaluate",
            "capture 1",
            "settle 1",
            "resize 1 80x60",
            "notices 1",
            "close 1",
            "shutdown",
        ]
    );
}

#[test]
fn requests_the_server_can_refuse_without_a_browser_never_reach_the_backend() {
    let connected = connect_fake();
    let client = &connected.client;
    open_tab(client);

    fn remote<T: std::fmt::Debug>(result: Result<T, ClientError>) -> ErrorKind {
        match result {
            Err(ClientError::Remote(error)) => error.kind,
            other => panic!("expected a remote error, got {other:?}"),
        }
    }
    // Navigation is not part of operating an offline page.
    assert_eq!(
        remote(client.cdp(TabId(1), "Page.navigate", json!({"url": "http://x"}), 1000)),
        ErrorKind::Unsupported
    );
    // A second tab with the same id.
    let (open, body) = index_only();
    assert_eq!(
        remote(client.open_artifact(open, &body)),
        ErrorKind::Conflict
    );
    // A tab that was never opened.
    assert_eq!(remote(client.settle(TabId(99), 1000)), ErrorKind::NotFound);
    // A bundle whose files do not add up.
    let (mut open, body) = index_only();
    open.tab = TabId(2);
    open.files[0].len += 1;
    assert_eq!(
        remote(client.open_artifact(open, &body)),
        ErrorKind::Invalid
    );
    // A result too big for the control channel is refused, not sent.
    assert_eq!(
        remote(client.cdp(TabId(1), "Runtime.evaluate", json!({"op": "huge"}), 1000)),
        ErrorKind::TooLarge
    );

    let calls = connected.fake.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        ["open 1 [index.html:text/html:9]", "cdp 1 Runtime.evaluate"],
        "only the open and the oversized evaluate reached the backend"
    );
    client.shutdown(NEVER);
    connected.helper.join().unwrap();
}

#[test]
fn at_most_four_tabs_and_closing_a_session_closes_its_tabs() {
    let connected = connect_fake();
    let client = &connected.client;
    for tab in 1..=MAX_TABS as u64 {
        let (open, body) = OpenArtifact::with_files(
            SessionId(if tab <= 2 { 1 } else { 2 }),
            TabId(tab),
            Viewport {
                width: 8,
                height: 8,
            },
            1,
            1000,
            vec![("index.html".into(), "text/html".into(), vec![0])],
        );
        client.open_artifact(open, &body).unwrap();
    }
    let (mut open, body) = index_only();
    open.tab = TabId(50);
    match client.open_artifact(open, &body) {
        Err(ClientError::Remote(error)) => assert_eq!(error.kind, ErrorKind::Busy),
        other => panic!("{other:?}"),
    }
    client.close_session(SessionId(1)).unwrap();
    // Tabs 1 and 2 are gone, 3 and 4 remain.
    assert!(matches!(
        client.settle(TabId(1), 1000),
        Err(ClientError::Remote(RemoteError {
            kind: ErrorKind::NotFound,
            ..
        }))
    ));
    client.settle(TabId(3), 1000).unwrap();
    client.shutdown(NEVER);
    connected.helper.join().unwrap();
}

#[test]
fn a_timed_out_request_fails_alone_and_its_late_response_is_discarded() {
    let connected = connect_fake();
    let client = &connected.client;
    open_tab(client);
    let gate = Request::Cdp {
        tab: TabId(1),
        method: "Runtime.evaluate".into(),
        params: json!({"op": "gate"}),
        timeout_ms: 1000,
    };
    // The backend holds the request until released, so a zero wait cannot see
    // an answer: the outcome does not depend on how fast anything runs.
    assert_eq!(
        client.request(gate, &[], std::time::Duration::ZERO),
        Err(ClientError::Timeout)
    );
    connected.fake.entered.recv().unwrap();
    connected.fake.release.send(()).unwrap();
    // The connection is still usable for the next request.
    client.settle(TabId(1), 1000).unwrap();
    assert_eq!(client.shutdown(NEVER), CloseReason::ShutdownAcknowledged);
    assert_eq!(client.stats().late_responses, 1);
    connected.helper.join().unwrap();
}

#[test]
fn queued_requests_are_answered_with_an_error_when_the_helper_is_told_to_stop() {
    let connected = connect_fake();
    let client = std::sync::Arc::new(connected.client);
    open_tab(&client);
    let gated = {
        let client = std::sync::Arc::clone(&client);
        std::thread::spawn(move || {
            client.cdp(TabId(1), "Runtime.evaluate", json!({"op": "gate"}), 1000)
        })
    };
    connected.fake.entered.recv().unwrap();
    let queued = {
        let client = std::sync::Arc::clone(&client);
        std::thread::spawn(move || client.settle(TabId(1), 1000))
    };
    // Shutdown is read while the gate request runs; the backend is told to
    // stop, and the helper then waits for the running request to return.
    let shutdown = {
        let client = std::sync::Arc::clone(&client);
        std::thread::spawn(move || client.shutdown(NEVER))
    };
    // The backend hears about the shutdown as soon as the helper reads the
    // request; only then is it safe to release the gate without racing the
    // queued request into the backend.
    connected.fake.shutdown_seen.recv().unwrap();
    connected.fake.release.send(()).unwrap();
    assert_eq!(gated.join().unwrap().unwrap(), json!({"gated": true}));
    let queued = queued.join().unwrap();
    match queued {
        // Either the request was still queued and refused, or the helper had
        // not read it before the shutdown and the connection ended.
        Err(ClientError::Remote(error)) => assert_eq!(error.kind, ErrorKind::ShuttingDown),
        Err(ClientError::Closed(_)) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(shutdown.join().unwrap(), CloseReason::ShutdownAcknowledged);
    connected.helper.join().unwrap();
}

#[test]
fn raw_frames_reach_the_client_newest_first_and_slow_consumers_only_lose_frames() {
    let connected = connect_fake();
    let client = &connected.client;
    open_tab(client);
    client.set_streaming(TabId(1), true).unwrap();
    let publish = |fill: u64| {
        client
            .cdp(
                TabId(1),
                "Runtime.evaluate",
                json!({"op": "frame", "fill": fill}),
                1000,
            )
            .unwrap()
    };
    for fill in 1..=20 {
        assert_eq!(publish(fill), json!({"published": true}));
    }
    // The newest frame is always delivered, whatever was dropped on the way.
    let mut last_seq = 0;
    loop {
        let frame = client.wait_frame(None).expect("connection is open");
        assert_eq!((frame.tab, frame.width, frame.height), (TabId(1), 2, 2));
        assert_eq!(frame.bgra.len(), 16);
        assert!(frame.seq > last_seq, "frames never go backwards");
        last_seq = frame.seq;
        let fill = frame.bgra[0];
        client.recycle_frame(frame);
        if fill == 20 {
            break;
        }
    }
    let stats = client.stats();
    assert!(stats.received_frames >= 1 && stats.received_frames <= 20);
    assert_eq!(stats.stale_frames, 0);

    // After streaming stops, paints are ignored and nothing more arrives.
    client.set_streaming(TabId(1), false).unwrap();
    assert_eq!(publish(21), json!({"published": false}));
    client.shutdown(NEVER);
    connected.helper.join().unwrap();
}

#[test]
fn a_page_that_goes_away_is_reported_and_forgotten() {
    let connected = connect_fake();
    let client = &connected.client;
    open_tab(client);
    client
        .cdp(TabId(1), "Runtime.evaluate", json!({"op": "crash"}), 1000)
        .unwrap();
    assert_eq!(
        client.next_event(None),
        Some(EventKind::TabGone {
            tab: TabId(1),
            reason: TabGoneReason::Crashed
        })
    );
    assert!(matches!(
        client.settle(TabId(1), 1000),
        Err(ClientError::Remote(RemoteError {
            kind: ErrorKind::NotFound,
            ..
        }))
    ));
    // The slot is free again for the same id.
    open_tab(client);
    client.shutdown(NEVER);
    connected.helper.join().unwrap();
}

#[test]
fn dropping_the_client_makes_the_helper_stop_by_itself() {
    let connected = connect_fake();
    open_tab(&connected.client);
    drop(connected.client);
    assert_eq!(connected.helper.join().unwrap(), ExitReason::PeerClosed);
    let calls = connected.fake.calls.lock().unwrap().clone();
    assert_eq!(calls.last().map(String::as_str), Some("shutdown"));
}

#[test]
fn a_stop_during_large_frames_ends_as_an_acknowledged_shutdown_every_time() {
    // The frame is far larger than a socket buffer, so the stop regularly
    // meets the sender inside the body.
    for _ in 0..20 {
        let c = connect_fake();
        open_tab(&c.client);
        c.client.set_streaming(TabId(1), true).unwrap();
        let published = c
            .client
            .cdp(
                TabId(1),
                "Runtime.evaluate",
                json!({"op": "large_frame", "width": 1280, "height": 720, "fill": 9}),
                1000,
            )
            .unwrap();
        assert_eq!(published["published"], true);
        assert_eq!(c.client.shutdown(NEVER), CloseReason::ShutdownAcknowledged);
        assert_eq!(c.helper.join().unwrap(), ExitReason::ShutdownRequested);
    }
}
