//! A pass-through on the frame channel between the real helper and the
//! `RendererClient`, to time what the client alone cannot see.
//!
//! The helper writes frames to this proxy's socket, the proxy forwards each
//! message unchanged to the client's socket, and forwards the client's
//! acknowledgements back, so the helper's backpressure rule is unchanged (one
//! frame in flight until acknowledged), with one extra hop.
//!
//! For every frame it records
//! - the time the first byte became readable (the sender began to write) and the
//!   time the whole message had been read: `wire`, the helper's socket transfer
//!   of a full frame through its real sender;
//! - the time to write the message on to the client: `forward`;
//! - the page's own time stamp in the pixels against the first byte: how long the
//!   page's drawing, CEF's paint, the frame copy and the queue took before the
//!   sender began.

use std::collections::HashMap;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use renderer_ipc::codec::{ACK_LIMITS, FRAME_LIMITS, read_message, write_message};
use renderer_ipc::{FrameMeta, codec::decode_meta};
use rustix::net::{RecvFlags, recv};

use crate::fixture;

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// When the first byte of the frame was readable.
    pub first: Instant,
    /// First byte to the last byte read.
    pub wire_ms: f64,
    /// Writing the message to the client.
    pub forward_ms: f64,
    /// Page time stamp to first byte, `None` when the stamp is unreadable.
    pub page_to_first_byte_ms: Option<f64>,
}

#[derive(Default)]
pub struct Shared {
    pub samples: Mutex<Vec<Sample>>,
    /// `seq` -> first byte, for the client to look up on receipt.
    pub first_seen: Mutex<HashMap<u64, Instant>>,
}

/// Blocks until a byte is readable (`true`) or the stream ended (`false`),
/// without consuming it.
fn wait_readable(stream: &UnixStream) -> bool {
    let mut byte = [0u8; 1];
    loop {
        match recv(stream, &mut byte, RecvFlags::PEEK) {
            Ok((0, _)) => return false,
            Ok(_) => return true,
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return false,
        }
    }
}

/// Starts the two forwarding threads. They end when either side closes.
pub fn start(helper_side: UnixStream, client_side: UnixStream) -> Arc<Shared> {
    let shared = Arc::new(Shared::default());
    let helper_ack = helper_side.try_clone().expect("duplicate the helper side");
    let client_ack = client_side.try_clone().expect("duplicate the client side");
    let helper_end = helper_side.try_clone().expect("duplicate the helper side");
    let client_end = client_side.try_clone().expect("duplicate the client side");
    {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let mut spare: Vec<u8> = Vec::new();
            while wait_readable(&helper_side) {
                let first = Instant::now();
                let first_ticks = fixture::now_ticks();
                let Ok(message) = read_message(
                    &helper_side,
                    FRAME_LIMITS,
                    std::mem::take(&mut spare),
                    Duration::from_secs(10),
                ) else {
                    break;
                };
                let wire_ms = first.elapsed().as_secs_f64() * 1000.0;
                let meta = decode_meta::<FrameMeta>(&message.meta).ok();
                let page_to_first_byte_ms = meta
                    .and_then(|m| fixture::stamp(&message.body, m.width))
                    .and_then(|s| fixture::age_ms(s, first_ticks));
                if let Some(meta) = meta {
                    shared.first_seen.lock().unwrap().insert(meta.seq, first);
                }
                let forward_started = Instant::now();
                if write_message(
                    &client_side,
                    message.kind,
                    &message.meta,
                    &message.body,
                    FRAME_LIMITS,
                )
                .is_err()
                {
                    break;
                }
                shared.samples.lock().unwrap().push(Sample {
                    first,
                    wire_ms,
                    forward_ms: forward_started.elapsed().as_secs_f64() * 1000.0,
                    page_to_first_byte_ms,
                });
                spare = message.body;
            }
            let _ = client_end.shutdown(Shutdown::Both);
        });
    }
    std::thread::spawn(move || {
        while let Ok(message) =
            read_message(&client_ack, ACK_LIMITS, Vec::new(), Duration::from_secs(10))
        {
            if write_message(&helper_ack, message.kind, &message.meta, &[], ACK_LIMITS).is_err() {
                break;
            }
        }
        let _ = helper_end.shutdown(Shutdown::Both);
    });
    shared
}

#[cfg(test)]
mod tests {
    use super::*;
    use renderer_ipc::codec::{Kind, encode_meta};
    use renderer_ipc::{Ack, RendererGeneration, TabId};

    #[test]
    fn forwards_frames_and_acks_unchanged_and_times_each_frame() {
        let (helper, proxy_helper) = UnixStream::pair().unwrap();
        let (proxy_client, client) = UnixStream::pair().unwrap();
        let shared = start(proxy_helper, proxy_client);

        let (width, height) = (64u32, 16u32);
        let body = vec![0x40u8; (width * height * 4) as usize];
        let meta = encode_meta(&FrameMeta {
            generation: RendererGeneration(1),
            tab: TabId(1),
            seq: 7,
            width,
            height,
        })
        .unwrap();
        write_message(&helper, Kind::Frame, &meta, &body, FRAME_LIMITS).unwrap();

        let got = read_message(&client, FRAME_LIMITS, Vec::new(), Duration::from_secs(10)).unwrap();
        assert_eq!(got.kind, Kind::Frame);
        assert_eq!(got.meta, meta);
        assert_eq!(got.body, body);

        let ack = encode_meta(&Ack { seq: 7 }).unwrap();
        write_message(&client, Kind::Ack, &ack, &[], ACK_LIMITS).unwrap();
        let back = read_message(&helper, ACK_LIMITS, Vec::new(), Duration::from_secs(10)).unwrap();
        assert_eq!((back.kind, back.meta), (Kind::Ack, ack));

        // The sample is pushed after the write to the client, so closing the
        // helper side (the proxy loop ends) is the condition to wait for.
        drop(helper);
        let mut end = [0u8; 1];
        let mut reader = &client;
        assert_eq!(std::io::Read::read(&mut reader, &mut end).unwrap(), 0);
        let samples = shared.samples.lock().unwrap();
        assert_eq!(samples.len(), 1);
        assert!(samples[0].wire_ms >= 0.0 && samples[0].forward_ms >= 0.0);
        assert!(shared.first_seen.lock().unwrap().contains_key(&7));
    }
}
