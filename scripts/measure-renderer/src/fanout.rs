//! Fan-out prototype for the renderer data plane (16a-2 F3, plan step 1).
//!
//! This is NOT product code. It reproduces, with plain threads and Unix
//! sockets, the shape the core would have once several data connections read
//! one tab's frames: a helper thread writes a frame to a socket pair and waits
//! for the acknowledgement, a core reader thread takes it into one buffer and
//! publishes it as the tab's latest frame, and one sender thread per
//! subscriber writes that frame to its own socket for a client reader thread.
//! The variants decide the buffer ownership and the write size before any
//! product code commits to them:
//!
//! - `share=1`: every sender writes the one shared allocation (`Arc`); the core
//!   reuses the buffer only when no sender holds it any more.
//! - `share=0`: every sender copies the frame into an allocation of its own.
//! - `chunk=N`: the frame is written in pieces of N bytes (`0`: one write).
//!
//! The latency is taken from the first byte the core reader observes of a
//! frame to the moment the subscriber's client holds every byte of it, which is
//! the interval the acceptance criterion names (p95 <= 33 ms). A stalled
//! subscriber connects and never reads.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::procfs;
use crate::stats;

/// Frames discarded per reader before the samples count.
const WARMUP_FRAMES: u64 = 10;
const FRAME_HEADER: usize = 16;
const SUB_HEADER: usize = 24;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub secs: u64,
    pub tabs: usize,
    pub subs: usize,
    pub stalled: usize,
    pub chunk: usize,
    pub share: bool,
}

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);

/// A frame allocation whose bytes are counted while it lives.
struct Buf {
    bytes: Vec<u8>,
}

impl Buf {
    fn new(len: usize) -> Self {
        let now = LIVE.fetch_add(len as u64, Ordering::SeqCst) + len as u64;
        PEAK.fetch_max(now, Ordering::SeqCst);
        Self {
            bytes: vec![0; len],
        }
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        LIVE.fetch_sub(self.bytes.len() as u64, Ordering::SeqCst);
    }
}

struct Frame {
    seq: u64,
    /// Nanoseconds from the run's epoch to the first byte the core observed.
    t0: u64,
    buf: Buf,
}

#[derive(Default)]
struct Slot {
    latest: Option<Arc<Frame>>,
    version: u64,
    closed: bool,
}

#[derive(Default)]
struct Hub {
    slot: Mutex<Slot>,
    changed: Condvar,
}

fn nanos(epoch: Instant) -> u64 {
    u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn helper(mut socket: UnixStream, p: Params, stop: &AtomicBool) {
    let len = p.width as usize * p.height as usize * 4;
    let body = vec![0xA5u8; len];
    let period = Duration::from_secs_f64(1.0 / f64::from(p.fps));
    let mut next = Instant::now();
    let mut seq = 0u64;
    let mut ack = [0u8; 1];
    while !stop.load(Ordering::SeqCst) {
        seq += 1;
        let mut head = [0u8; FRAME_HEADER];
        head[..8].copy_from_slice(&seq.to_le_bytes());
        head[8..].copy_from_slice(&(len as u64).to_le_bytes());
        if socket.write_all(&head).is_err() || socket.write_all(&body).is_err() {
            return;
        }
        if socket.read_exact(&mut ack).is_err() {
            return;
        }
        next += period;
        let now = Instant::now();
        if next > now {
            thread::park_timeout(next - now);
        } else {
            next = now;
        }
    }
}

fn core_reader(mut socket: UnixStream, hub: &Hub, epoch: Instant) {
    let mut spare: Option<Buf> = None;
    let mut first = [0u8; 1];
    loop {
        if socket.read_exact(&mut first).is_err() {
            break;
        }
        let t0 = nanos(epoch);
        let mut rest = [0u8; FRAME_HEADER - 1];
        if socket.read_exact(&mut rest).is_err() {
            break;
        }
        let mut head = [0u8; FRAME_HEADER];
        head[0] = first[0];
        head[1..].copy_from_slice(&rest);
        let seq = u64::from_le_bytes(head[..8].try_into().unwrap());
        let len = u64::from_le_bytes(head[8..].try_into().unwrap()) as usize;
        let mut buf = match spare.take() {
            Some(buf) if buf.bytes.len() == len => buf,
            _ => Buf::new(len),
        };
        if socket.read_exact(&mut buf.bytes).is_err() || socket.write_all(&[1]).is_err() {
            break;
        }
        let frame = Arc::new(Frame { seq, t0, buf });
        let old = {
            let mut slot = hub.slot.lock().unwrap();
            slot.version += 1;
            slot.latest.replace(frame)
        };
        hub.changed.notify_all();
        // The previous allocation is reused only when no sender holds it.
        if let Some(Ok(old)) = old.map(Arc::try_unwrap) {
            spare = Some(old.buf);
        }
    }
    hub.slot.lock().unwrap().closed = true;
    hub.changed.notify_all();
}

fn sender(mut socket: UnixStream, hub: &Hub, p: Params) {
    let mut seen = 0u64;
    loop {
        let frame = {
            let mut slot = hub.slot.lock().unwrap();
            while slot.version == seen && !slot.closed {
                slot = hub.changed.wait(slot).unwrap();
            }
            if slot.closed {
                return;
            }
            seen = slot.version;
            slot.latest.clone().expect("a published frame")
        };
        let owned;
        let (seq, t0, bytes): (u64, u64, &[u8]) = if p.share {
            (frame.seq, frame.t0, &frame.buf.bytes)
        } else {
            let mut copy = Buf::new(frame.buf.bytes.len());
            copy.bytes.copy_from_slice(&frame.buf.bytes);
            let meta = (frame.seq, frame.t0);
            drop(frame);
            owned = copy;
            (meta.0, meta.1, &owned.bytes)
        };
        let mut head = [0u8; SUB_HEADER];
        head[..8].copy_from_slice(&seq.to_le_bytes());
        head[8..16].copy_from_slice(&t0.to_le_bytes());
        head[16..].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        if socket.write_all(&head).is_err() {
            return;
        }
        let step = if p.chunk == 0 { bytes.len() } else { p.chunk };
        for piece in bytes.chunks(step) {
            if socket.write_all(piece).is_err() {
                return;
            }
        }
    }
}

#[derive(Default)]
struct Received {
    latency_ms: Vec<f64>,
    frames: u64,
    gaps: u64,
}

/// The measurement window, in nanoseconds since the run began, shared by every
/// reader: a frame counts only when it completed inside `[start, end)`, so the
/// count and the wall time it is divided by describe the same interval.
struct Window {
    start: AtomicU64,
    end: AtomicU64,
}
impl Window {
    fn new() -> Self {
        Self {
            start: AtomicU64::new(u64::MAX),
            end: AtomicU64::new(u64::MAX),
        }
    }
    fn contains(&self, at: u64) -> bool {
        at >= self.start.load(Ordering::SeqCst) && at < self.end.load(Ordering::SeqCst)
    }
}

fn client_reader(mut socket: UnixStream, epoch: Instant, window: &Window) -> Received {
    let mut out = Received::default();
    let mut buffer: Vec<u8> = Vec::new();
    let mut last = 0u64;
    loop {
        let mut head = [0u8; SUB_HEADER];
        if socket.read_exact(&mut head).is_err() {
            return out;
        }
        let seq = u64::from_le_bytes(head[..8].try_into().unwrap());
        let t0 = u64::from_le_bytes(head[8..16].try_into().unwrap());
        let len = u64::from_le_bytes(head[16..].try_into().unwrap()) as usize;
        buffer.resize(len, 0);
        if socket.read_exact(&mut buffer).is_err() {
            return out;
        }
        let done = nanos(epoch);
        if window.contains(done) {
            out.frames += 1;
            out.latency_ms
                .push(done.saturating_sub(t0) as f64 / 1_000_000.0);
            if last != 0 && seq > last + 1 {
                out.gaps += seq - last - 1;
            }
        }
        last = seq;
    }
}

/// Runs one fan-out configuration and returns its JSON result.
pub fn run(p: Params) -> Value {
    let epoch = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let mut sockets: Vec<UnixStream> = Vec::new();
    let mut threads = Vec::new();
    let hubs: Vec<Arc<Hub>> = (0..p.tabs).map(|_| Arc::new(Hub::default())).collect();
    for hub in &hubs {
        let (helper_end, core_end) = UnixStream::pair().expect("socket pair");
        sockets.push(helper_end.try_clone().unwrap());
        sockets.push(core_end.try_clone().unwrap());
        let s = Arc::clone(&stop);
        threads.push(thread::spawn(move || helper(helper_end, p, &s)));
        let hub = Arc::clone(hub);
        threads.push(thread::spawn(move || core_reader(core_end, &hub, epoch)));
    }
    let window = Arc::new(Window::new());
    let mut readers = Vec::new();
    let mut idle_ends = Vec::new();
    for index in 0..p.subs + p.stalled {
        let (send_end, client_end) = UnixStream::pair().expect("socket pair");
        sockets.push(send_end.try_clone().unwrap());
        sockets.push(client_end.try_clone().unwrap());
        let hub = Arc::clone(&hubs[index % p.tabs]);
        threads.push(thread::spawn(move || sender(send_end, &hub, p)));
        if index < p.subs {
            let window = Arc::clone(&window);
            readers.push(thread::spawn(move || {
                client_reader(client_end, epoch, &window)
            }));
        } else {
            idle_ends.push(client_end);
        }
    }
    // Start the window after the warm-up frames of every reader.
    thread::park_timeout(Duration::from_secs_f64(
        (WARMUP_FRAMES as f64 + 2.0) / f64::from(p.fps),
    ));
    PEAK.store(LIVE.load(Ordering::SeqCst), Ordering::SeqCst);
    let (mem0, ticks0) = procfs::own();
    let start = Instant::now();
    window.start.store(nanos(epoch), Ordering::SeqCst);
    thread::park_timeout(Duration::from_secs(p.secs));
    window.end.store(nanos(epoch), Ordering::SeqCst);
    let wall = start.elapsed().as_secs_f64();
    let (mem1, ticks1) = procfs::own();
    let peak = PEAK.load(Ordering::SeqCst);
    stop.store(true, Ordering::SeqCst);
    for socket in &sockets {
        let _ = socket.shutdown(std::net::Shutdown::Both);
    }
    drop(idle_ends);
    let mut latency = Vec::new();
    let (mut frames, mut gaps) = (0u64, 0u64);
    for reader in readers {
        let got = reader.join().unwrap_or_default();
        latency.extend(got.latency_ms);
        frames += got.frames;
        gaps += got.gaps;
    }
    for t in threads {
        let _ = t.join();
    }
    let readers = p.subs.max(1) as f64;
    json!({
        "scenario": "fanout",
        "viewport": format!("{}x{}", p.width, p.height),
        "frame_bytes": u64::from(p.width) * u64::from(p.height) * 4,
        "requested_fps": p.fps,
        "tabs": p.tabs,
        "subscribers": p.subs,
        "stalled_readers": p.stalled,
        "share": p.share,
        "chunk": p.chunk,
        "window_secs": stats::round(wall),
        "frames_per_reader_per_s": stats::round(frames as f64 / readers / wall),
        "skipped_frames": gaps,
        "first_byte_to_complete_ms": stats::dist(latency),
        "peak_live_frame_mib": stats::round(peak as f64 / 1_048_576.0),
        "cpu_percent_one_core": stats::round(
            (ticks1 - ticks0) as f64 / procfs::CLK_TCK / wall * 100.0),
        "rss_mib": { "start": procfs::mib(mem0.rss_kib), "end": procfs::mib(mem1.rss_kib) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The allocation counters are process-wide: tests that read them take turns.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn tiny(share: bool, chunk: usize, stalled: usize) -> Params {
        Params {
            width: 64,
            height: 32,
            fps: 200,
            secs: 1,
            tabs: 2,
            subs: 3,
            stalled,
            chunk,
            share,
        }
    }

    #[test]
    fn every_variant_delivers_whole_frames_to_the_readers_that_read() {
        let _turn = serial();
        for (share, chunk, stalled) in [(true, 0, 0), (true, 1000, 1), (false, 4096, 1)] {
            let out = run(tiny(share, chunk, stalled));
            assert!(
                out["first_byte_to_complete_ms"]["n"].as_u64().unwrap() > 0,
                "{out}"
            );
        }
    }

    #[test]
    fn counted_allocations_are_returned() {
        let _turn = serial();
        let before = LIVE.load(Ordering::SeqCst);
        drop(Buf::new(1024));
        assert_eq!(LIVE.load(Ordering::SeqCst), before);
    }
}
