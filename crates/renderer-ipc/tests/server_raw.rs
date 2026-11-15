//! The real server against a peer that says things the real client never would.

mod common;

use std::io::Write;
use std::time::Duration;

use common::*;
use renderer_ipc::codec::{
    ACK_LIMITS, FRAME_LIMITS, Kind, encode_meta, read_message, write_message,
};
use renderer_ipc::*;
use serde_json::json;

fn error_kind(outcome: &Outcome) -> ErrorKind {
    match outcome {
        Outcome::Err { error } => error.kind,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn a_first_request_that_is_not_hello_ends_the_connection() {
    let (peer, fake, helper) = serve_raw(ServeConfig::default());
    raw_request(
        &peer.control,
        GENERATION,
        1,
        Request::CloseTab { tab: TabId(1) },
        &[],
    );
    let (response, _) = read_response_frame(&peer.control);
    assert_eq!(error_kind(&response.outcome), ErrorKind::Invalid);
    assert!(matches!(
        helper.join().unwrap(),
        ExitReason::HandshakeFailed(_)
    ));
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn an_unsupported_protocol_version_is_refused() {
    let (peer, _fake, helper) = serve_raw(ServeConfig::default());
    raw_request(
        &peer.control,
        GENERATION,
        1,
        Request::Hello {
            protocol: PROTOCOL_VERSION + 1,
        },
        &[],
    );
    let (response, _) = read_response_frame(&peer.control);
    assert_eq!(error_kind(&response.outcome), ErrorKind::Unsupported);
    assert!(matches!(
        helper.join().unwrap(),
        ExitReason::HandshakeFailed(_)
    ));
}

#[test]
fn a_core_that_never_says_hello_is_not_waited_for_forever() {
    // The peer sends nothing, so the handshake can only end by its deadline,
    // however slow the machine is.
    let (_peer, _fake, helper) = serve_raw(ServeConfig {
        hello_timeout: Duration::from_millis(1),
        ..ServeConfig::default()
    });
    assert!(matches!(
        helper.join().unwrap(),
        ExitReason::HandshakeFailed(_)
    ));
}

#[test]
fn a_backend_that_cannot_start_answers_hello_with_the_reason() {
    let (control_core, control_helper) = std::os::unix::net::UnixStream::pair().unwrap();
    let (_frames_core, frames_helper) = std::os::unix::net::UnixStream::pair().unwrap();
    let helper = serve_in_thread(
        control_helper,
        frames_helper,
        ServeConfig::default(),
        |_ctx| Err::<Fake, _>(RemoteError::new(ErrorKind::Unavailable, "no sandbox here")),
    );
    raw_request(
        &control_core,
        GENERATION,
        1,
        Request::Hello {
            protocol: PROTOCOL_VERSION,
        },
        &[],
    );
    let (response, _) = read_response_frame(&control_core);
    assert_eq!(error_kind(&response.outcome), ErrorKind::Unavailable);
    assert_eq!(
        helper.join().unwrap(),
        ExitReason::InitFailed("no sandbox here".into())
    );
}

#[test]
fn a_request_for_another_generation_is_refused_and_never_acted_on() {
    let (peer, fake, helper) = serve_raw(ServeConfig::default());
    raw_hello(&peer);
    raw_request(
        &peer.control,
        RendererGeneration(GENERATION.0 + 1),
        2,
        Request::CloseTab { tab: TabId(1) },
        &[],
    );
    let (response, _) = read_response_frame(&peer.control);
    assert_eq!(
        response.generation, GENERATION,
        "answered as this generation"
    );
    assert_eq!(error_kind(&response.outcome), ErrorKind::StaleGeneration);
    raw_request(&peer.control, GENERATION, 3, Request::Shutdown, &[]);
    let (response, _) = read_response_frame(&peer.control);
    assert!(matches!(response.outcome, Outcome::Ok { .. }));
    assert_eq!(helper.join().unwrap(), ExitReason::ShutdownRequested);
    assert_eq!(fake.calls.lock().unwrap().as_slice(), ["shutdown"]);
}

#[test]
fn a_body_larger_than_the_limit_is_a_protocol_violation_without_reading_it() {
    let (mut peer, _fake, helper) = serve_raw(ServeConfig::default());
    raw_hello(&peer);
    // Announces a 4 GiB body and sends none of it.
    peer.control.write_all(&raw_header(1, 2, u32::MAX)).unwrap();
    peer.control.write_all(b"{}").unwrap();
    assert!(matches!(
        helper.join().unwrap(),
        ExitReason::ProtocolViolation(_)
    ));
}

#[test]
fn a_full_request_queue_answers_busy_and_the_rest_still_run() {
    let (peer, fake, helper) = serve_raw(ServeConfig::default());
    raw_hello(&peer);
    let (open, body) = index_only();
    raw_request(
        &peer.control,
        GENERATION,
        2,
        Request::OpenArtifact(open),
        &body,
    );
    let (response, _) = read_response_frame(&peer.control);
    assert!(matches!(response.outcome, Outcome::Ok { .. }));
    let gate = |id| {
        raw_request(
            &peer.control,
            GENERATION,
            id,
            Request::Cdp {
                tab: TabId(1),
                method: "Runtime.evaluate".into(),
                params: json!({"op": "gate"}),
                timeout_ms: 1000,
            },
            &[],
        );
    };
    gate(10);
    fake.entered.recv().unwrap();
    // The executor is held in request 10; the queue takes MAX_QUEUED_REQUESTS
    // more and refuses the next, which is answered at once.
    let queued = MAX_QUEUED_REQUESTS as u64;
    for id in 11..11 + queued {
        raw_request(
            &peer.control,
            GENERATION,
            id,
            Request::TakeNotices { tab: TabId(1) },
            &[],
        );
    }
    let refused = 11 + queued;
    raw_request(
        &peer.control,
        GENERATION,
        refused,
        Request::TakeNotices { tab: TabId(1) },
        &[],
    );
    let (response, _) = read_response_frame(&peer.control);
    assert_eq!(response.id, RequestId(refused));
    assert_eq!(error_kind(&response.outcome), ErrorKind::Busy);

    fake.release.send(()).unwrap();
    let mut answered = Vec::new();
    for _ in 0..=queued {
        let (response, _) = read_response_frame(&peer.control);
        assert!(
            matches!(response.outcome, Outcome::Ok { .. }),
            "{response:?}"
        );
        answered.push(response.id.0);
    }
    assert_eq!(
        answered,
        (10..11 + queued).collect::<Vec<_>>(),
        "in arrival order"
    );
    raw_request(&peer.control, GENERATION, 99, Request::Shutdown, &[]);
    let _ = read_response_frame(&peer.control);
    assert_eq!(helper.join().unwrap(), ExitReason::ShutdownRequested);
}

fn stream_one_frame(peer: &RawPeer) -> renderer_ipc::codec::Message {
    let (open, body) = index_only();
    raw_request(
        &peer.control,
        GENERATION,
        2,
        Request::OpenArtifact(open),
        &body,
    );
    read_response_frame(&peer.control);
    raw_request(
        &peer.control,
        GENERATION,
        3,
        Request::SetStreaming {
            tab: TabId(1),
            enabled: true,
        },
        &[],
    );
    read_response_frame(&peer.control);
    raw_request(
        &peer.control,
        GENERATION,
        4,
        Request::Cdp {
            tab: TabId(1),
            method: "Runtime.evaluate".into(),
            params: json!({"op": "frame", "fill": 9}),
            timeout_ms: 1000,
        },
        &[],
    );
    read_response_frame(&peer.control);
    let frame = read_message(&peer.frames, FRAME_LIMITS, Vec::new(), NEVER).unwrap();
    assert_eq!(frame.kind, Kind::Frame);
    assert_eq!(frame.body, [9; 16]);
    frame
}

#[test]
fn a_frame_is_acknowledged_by_sequence_and_a_wrong_acknowledgement_ends_the_helper() {
    let (peer, _fake, helper) = serve_raw(ServeConfig::default());
    raw_hello(&peer);
    let frame = stream_one_frame(&peer);
    let meta: FrameMeta = serde_json::from_slice(&frame.meta).unwrap();
    assert_eq!(
        (meta.generation, meta.tab, meta.width, meta.height),
        (GENERATION, TabId(1), 2, 2)
    );
    let wrong = encode_meta(&Ack { seq: meta.seq + 1 }).unwrap();
    write_message(&peer.frames, Kind::Ack, &wrong, &[], ACK_LIMITS).unwrap();
    assert!(matches!(
        helper.join().unwrap(),
        ExitReason::FrameChannelFailed(_)
    ));
}

#[test]
fn a_core_that_drops_the_frame_channel_ends_the_helper() {
    let (peer, _fake, helper) = serve_raw(ServeConfig::default());
    raw_hello(&peer);
    stream_one_frame(&peer);
    drop(peer.frames);
    assert!(matches!(
        helper.join().unwrap(),
        ExitReason::FrameChannelFailed(_)
    ));
}

#[test]
fn a_core_that_never_acknowledges_a_frame_ends_the_helper() {
    // The peer reads the frame and says nothing, so the acknowledgement can
    // only fail by its deadline, however slow the machine is.
    let (peer, _fake, helper) = serve_raw(ServeConfig {
        ack_timeout: Duration::from_millis(1),
        ..ServeConfig::default()
    });
    raw_hello(&peer);
    stream_one_frame(&peer);
    assert!(matches!(
        helper.join().unwrap(),
        ExitReason::FrameChannelFailed(_)
    ));
    drop(peer.control);
}
