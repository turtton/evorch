//! Frame encoding and the incremental decoder.

mod common;

use common::*;
use workbench_protocol::envelope::*;
use workbench_protocol::framing::*;
use workbench_protocol::handshake::*;

fn frame_of(payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(payload);
    frame
}

fn sample_frame() -> (Vec<u8>, RequestEnvelope) {
    let request = envelope(1, RequestBody::Command(commands().remove(0)));
    (encode_frame(&request, DEFAULT).unwrap(), request)
}

const DEFAULT: u32 = 1024 * 1024;

#[test]
fn a_frame_is_a_big_endian_length_then_json() {
    let (frame, request) = sample_frame();
    let json = serde_json::to_vec(&request).unwrap();
    assert_eq!(frame[..4], (json.len() as u32).to_be_bytes());
    assert_eq!(frame[4..], json);
}

#[test]
fn a_frame_delivered_one_byte_at_a_time_decodes_once_complete() {
    let (frame, request) = sample_frame();
    let mut decoder = FrameDecoder::new(DEFAULT);
    for (i, byte) in frame.iter().enumerate() {
        assert_eq!(decoder.next_payload().unwrap(), None, "before byte {i}");
        decoder.push(&[*byte]);
    }
    let payload = decoder.next_payload().unwrap().expect("complete");
    assert_eq!(
        decode_payload::<RequestEnvelope>(&payload).unwrap(),
        request
    );
    assert_eq!(decoder.next_payload().unwrap(), None);
    assert_eq!(decoder.buffered(), 0);
}

#[test]
fn concatenated_frames_in_one_read_come_out_in_order() {
    let pings: Vec<RequestEnvelope> = (0..3)
        .map(|n| {
            envelope(
                n,
                RequestBody::Control(Control::Ping {
                    nonce: u64::from(n).into(),
                }),
            )
        })
        .collect();
    let mut bytes = Vec::new();
    for ping in &pings {
        bytes.extend(encode_frame(ping, DEFAULT).unwrap());
    }
    let mut decoder = FrameDecoder::new(DEFAULT);
    decoder.push(&bytes);
    let mut out = Vec::new();
    while let Some(payload) = decoder.next_payload().unwrap() {
        out.push(decode_payload::<RequestEnvelope>(&payload).unwrap());
    }
    assert_eq!(out, pings);
}

#[test]
fn a_split_header_waits_for_the_rest() {
    let (frame, _) = sample_frame();
    let mut decoder = FrameDecoder::new(DEFAULT);
    decoder.push(&frame[..2]);
    assert_eq!(decoder.next_payload().unwrap(), None);
    decoder.push(&frame[2..]);
    assert!(decoder.next_payload().unwrap().is_some());
}

#[test]
fn an_oversized_length_is_refused_before_any_payload_arrives() {
    let mut decoder = FrameDecoder::new(1024);
    decoder.push(&(1025u32).to_be_bytes());
    assert!(matches!(
        decoder.next_payload(),
        Err(FramingError::FrameTooLarge {
            len: 1025,
            max: 1024
        })
    ));
    // A hostile length must not reserve memory, and the stream stays dead.
    assert_eq!(decoder.buffered(), 0);
    decoder.push(&[0; 16]);
    assert_eq!(decoder.buffered(), 0);
    assert!(decoder.next_payload().is_err());
}

#[test]
fn the_maximum_length_is_accepted_and_one_more_is_not() {
    let mut decoder = FrameDecoder::new(8);
    decoder.push(&frame_of(&[b' '; 8]));
    assert_eq!(decoder.next_payload().unwrap().unwrap().len(), 8);
    let mut decoder = FrameDecoder::new(8);
    decoder.push(&frame_of(&[b' '; 9]));
    assert!(decoder.next_payload().is_err());
}

#[test]
fn the_decoder_never_exceeds_the_hard_limit_whatever_it_is_told() {
    let mut decoder = FrameDecoder::new(u32::MAX);
    assert_eq!(
        decoder.max_frame_bytes(),
        workbench_protocol::limits::HARD_MAX_FRAME_BYTES
    );
    decoder.set_max_frame_bytes(u32::MAX);
    decoder.push(&u32::MAX.to_be_bytes());
    assert!(decoder.next_payload().is_err());
}

#[test]
fn a_zero_length_frame_is_refused() {
    let mut decoder = FrameDecoder::new(DEFAULT);
    decoder.push(&frame_of(b""));
    assert!(matches!(
        decoder.next_payload(),
        Err(FramingError::EmptyFrame)
    ));
}

#[test]
fn a_corrupt_payload_is_a_json_error_and_does_not_poison_the_framing() {
    let mut decoder = FrameDecoder::new(DEFAULT);
    decoder.push(&frame_of(b"{not json"));
    let (good, request) = sample_frame();
    decoder.push(&good);
    let bad = decoder.next_payload().unwrap().unwrap();
    assert!(matches!(
        decode_payload::<RequestEnvelope>(&bad),
        Err(FramingError::Json(_))
    ));
    // Framing is intact, so the next frame is still readable.
    let next = decoder.next_payload().unwrap().unwrap();
    assert_eq!(decode_payload::<RequestEnvelope>(&next).unwrap(), request);

    assert!(matches!(
        decode_payload::<RequestEnvelope>(&[0xff, 0xfe]),
        Err(FramingError::Json(_))
    ));
    assert!(decode_payload::<RequestEnvelope>(b"{\"kind\":\"command\"}").is_err());
}

#[test]
fn encoding_refuses_payloads_over_the_limit() {
    let request = envelope(1, RequestBody::Command(commands().remove(0)));
    let len = serde_json::to_vec(&request).unwrap().len() as u32;
    encode_frame(&request, len).unwrap();
    assert!(matches!(
        encode_frame(&request, len - 1),
        Err(FramingError::FrameTooLarge { .. })
    ));
}

#[test]
fn handshake_frames_use_the_same_framing() {
    let mut decoder = FrameDecoder::new(DEFAULT);
    decoder.push(&encode_frame(&client_hello(), DEFAULT).unwrap());
    decoder.push(&encode_frame(&HandshakeReply::Accepted(server_hello()), DEFAULT).unwrap());
    let hello = decoder.next_payload().unwrap().unwrap();
    assert_eq!(
        decode_payload::<ClientHello>(&hello).unwrap(),
        client_hello()
    );
    let reply = decoder.next_payload().unwrap().unwrap();
    assert_eq!(
        decode_payload::<HandshakeReply>(&reply).unwrap(),
        HandshakeReply::Accepted(server_hello())
    );
}
