//! The real client against a helper that misbehaves.

mod common;

use std::io::Write;
use std::thread::JoinHandle;

use common::*;
use renderer_ipc::codec::{FRAME_LIMITS, Kind, Limits, encode_meta, read_message, write_message};
use renderer_ipc::*;
use serde_json::json;

/// Connects a real client to a hand-driven helper that has answered `Hello`.
fn connect_to_raw_helper() -> (RendererClient, RawPeer) {
    let (control_core, control_helper) = std::os::unix::net::UnixStream::pair().unwrap();
    let (frames_core, frames_helper) = std::os::unix::net::UnixStream::pair().unwrap();
    let connecting: JoinHandle<_> = std::thread::spawn(move || {
        RendererClient::connect(control_core, frames_core, ClientConfig::new(GENERATION))
    });
    let (hello, _) = read_request_frame(&control_helper);
    assert!(matches!(hello.request, Request::Hello { .. }));
    raw_response(&control_helper, GENERATION, hello.id.0, hello_ok(), &[]);
    let (client, _) = connecting.join().unwrap().unwrap();
    (
        client,
        RawPeer {
            control: control_helper,
            frames: frames_helper,
        },
    )
}

fn frame_meta(generation: RendererGeneration, seq: u64, width: u32, height: u32) -> Vec<u8> {
    encode_meta(&FrameMeta {
        generation,
        tab: TabId(1),
        seq,
        width,
        height,
    })
    .unwrap()
}

fn read_ack(peer: &RawPeer) -> Ack {
    let message = read_message(
        &peer.frames,
        renderer_ipc::codec::ACK_LIMITS,
        Vec::new(),
        NEVER,
    )
    .unwrap();
    assert_eq!(message.kind, Kind::Ack);
    serde_json::from_slice(&message.meta).unwrap()
}

fn send_frame(peer: &RawPeer, meta: &[u8], body: &[u8]) {
    write_message(&peer.frames, Kind::Frame, meta, body, FRAME_LIMITS).unwrap();
}

#[test]
fn a_response_of_another_generation_is_discarded_and_the_real_one_still_completes() {
    let (client, peer) = connect_to_raw_helper();
    let client = std::sync::Arc::new(client);
    let waiting = {
        let client = std::sync::Arc::clone(&client);
        std::thread::spawn(move || client.settle(TabId(1), 1000))
    };
    let (request, _) = read_request_frame(&peer.control);
    let other = RendererGeneration(GENERATION.0 + 1);
    let done = Outcome::Ok { reply: Reply::Done };
    raw_response(&peer.control, other, request.id.0, done.clone(), &[]);
    raw_response(&peer.control, GENERATION, request.id.0, done, &[]);
    waiting.join().unwrap().unwrap();
    assert_eq!(client.stats().stale_messages, 1);
}

#[test]
fn a_frame_of_another_generation_is_acknowledged_but_never_delivered() {
    let (client, peer) = connect_to_raw_helper();
    let other = RendererGeneration(GENERATION.0 + 1);
    send_frame(&peer, &frame_meta(other, 1, 1, 1), &[0; 4]);
    assert_eq!(read_ack(&peer), Ack { seq: 1 });
    send_frame(&peer, &frame_meta(GENERATION, 2, 1, 1), &[5; 4]);
    assert_eq!(read_ack(&peer), Ack { seq: 2 });
    let frame = client.wait_frame(None).unwrap();
    assert_eq!((frame.seq, frame.bgra.as_slice()), (2, [5; 4].as_slice()));
    assert_eq!(client.stats().stale_frames, 1);
}

#[test]
fn damaged_frames_end_the_connection_and_fail_what_is_pending() {
    // (what the helper sends, written after the request is pending)
    let wrong_size: (Vec<u8>, Vec<u8>) = (frame_meta(GENERATION, 1, 2, 2), vec![0; 15]);
    let zero_size: (Vec<u8>, Vec<u8>) = (frame_meta(GENERATION, 1, 0, 0), vec![]);
    let huge: (Vec<u8>, Vec<u8>) = (
        frame_meta(GENERATION, 1, MAX_VIEWPORT_WIDTH + 1, 1),
        vec![0; (MAX_VIEWPORT_WIDTH as usize + 1) * 4],
    );
    for (meta, body) in [wrong_size, zero_size, huge] {
        let (client, peer) = connect_to_raw_helper();
        let client = std::sync::Arc::new(client);
        let waiting = {
            let client = std::sync::Arc::clone(&client);
            std::thread::spawn(move || client.settle(TabId(1), 1000))
        };
        read_request_frame(&peer.control);
        // Sent past FRAME_LIMITS' own check on purpose.
        let wide = Limits {
            max_meta: usize::MAX,
            max_body: usize::MAX,
        };
        write_message(&peer.frames, Kind::Frame, &meta, &body, wide).unwrap();
        let result = waiting.join().unwrap();
        assert!(
            matches!(
                result,
                Err(ClientError::Closed(CloseReason::ProtocolViolation(_)))
            ),
            "{result:?}"
        );
        assert!(matches!(
            client.wait_closed(),
            CloseReason::ProtocolViolation(_)
        ));
        assert!(client.wait_frame(None).is_none());
    }
}

#[test]
fn a_frame_announcing_more_than_the_limit_is_refused_before_it_is_read() {
    let (client, mut peer) = connect_to_raw_helper();
    peer.frames.write_all(&raw_header(4, 2, u32::MAX)).unwrap();
    assert!(matches!(
        client.wait_closed(),
        CloseReason::ProtocolViolation(_)
    ));
}

#[test]
fn garbage_on_the_control_channel_ends_the_connection() {
    let (client, mut peer) = connect_to_raw_helper();
    peer.control.write_all(&[0xFF; 16]).unwrap();
    assert!(matches!(
        client.wait_closed(),
        CloseReason::ProtocolViolation(_)
    ));
    assert!(matches!(
        client.settle(TabId(1), 1000),
        Err(ClientError::Closed(CloseReason::ProtocolViolation(_)))
    ));
}

#[test]
fn a_helper_that_dies_mid_request_fails_that_request_with_peer_closed() {
    let (client, peer) = connect_to_raw_helper();
    let client = std::sync::Arc::new(client);
    let waiting = {
        let client = std::sync::Arc::clone(&client);
        std::thread::spawn(move || client.cdp(TabId(1), "Runtime.evaluate", json!({}), 1000))
    };
    read_request_frame(&peer.control);
    drop(peer);
    assert_eq!(
        waiting.join().unwrap(),
        Err(ClientError::Closed(CloseReason::PeerClosed))
    );
    assert_eq!(client.wait_closed(), CloseReason::PeerClosed);
}

#[test]
fn a_request_too_large_for_the_channel_is_refused_locally_and_the_connection_survives() {
    let (client, peer) = connect_to_raw_helper();
    let (open, _) = index_only();
    let body = vec![0u8; renderer_ipc::codec::MAX_CONTROL_BODY + 1];
    assert!(matches!(
        client.open_artifact(open, &body),
        Err(ClientError::Local(_))
    ));
    assert_eq!(client.closed(), None);
    drop(peer);
}

#[test]
fn shutdown_returns_when_the_helper_acknowledges_and_then_keeps_the_channels_open() {
    let (client, peer) = connect_to_raw_helper();
    let stopping = std::thread::spawn(move || {
        let reason = client.shutdown(std::time::Duration::from_millis(500));
        (reason, client.closed())
    });
    let (request, _) = read_request_frame(&peer.control);
    assert!(matches!(request.request, Request::Shutdown));
    raw_response(
        &peer.control,
        GENERATION,
        request.id.0,
        Outcome::Ok { reply: Reply::Done },
        &[],
    );
    // The helper now holds both channels and sends nothing more. `shutdown`
    // must still return, by closing the connection itself. (On a machine so
    // slow that the answer misses the deadline it ends as LocalClose instead;
    // both are final and neither is a hang.)
    let (reason, closed) = stopping.join().unwrap();
    assert!(
        matches!(
            reason,
            CloseReason::ShutdownTimedOut | CloseReason::LocalClose
        ),
        "{reason:?}"
    );
    assert_eq!(closed, Some(reason));
    // The client cut the control channel: the helper reads the end of it.
    let ended = read_message(
        &peer.control,
        renderer_ipc::codec::CONTROL_LIMITS,
        Vec::new(),
        NEVER,
    );
    assert!(matches!(
        ended,
        Err(renderer_ipc::codec::CodecError::Closed)
    ));
}
