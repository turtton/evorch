//! A backend that publishes frames itself, to measure the IPC (frame queue,
//! sender, socket, client reader) without CEF in the way.
//!
//! The first 8 bytes of every frame hold the publish time (nanoseconds since
//! `BASE`), so the receiver, in the same process, gets an exact
//! publish-to-available latency.

use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use renderer_ipc::{
    Backend, BackendContext, ExitReason, FrameQueue, Notices, OpenArtifact, RemoteError,
    ServeConfig, TabId, Viewport, serve_in_thread,
};

static BASE: OnceLock<Instant> = OnceLock::new();

pub fn base() -> Instant {
    *BASE.get_or_init(Instant::now)
}

pub fn stamp_nanos() -> u64 {
    base().elapsed().as_nanos() as u64
}

pub fn read_stamp(bgra: &[u8]) -> Option<u64> {
    bgra.get(..8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

struct Shared {
    tabs: Mutex<BTreeMap<TabId, (Viewport, bool)>>,
    stop: AtomicBool,
}

pub struct Synthetic {
    shared: Arc<Shared>,
    producer: Mutex<Option<JoinHandle<()>>>,
}

impl Backend for Synthetic {
    fn open_artifact(
        &self,
        open: &OpenArtifact,
        _files: Vec<(String, String, Vec<u8>)>,
    ) -> Result<Viewport, RemoteError> {
        self.shared
            .tabs
            .lock()
            .unwrap()
            .insert(open.tab, (open.viewport, false));
        Ok(open.viewport)
    }
    fn resize(&self, _tab: TabId, _viewport: Viewport) -> Result<(), RemoteError> {
        Ok(())
    }
    fn set_streaming(&self, tab: TabId, enabled: bool) -> Result<(), RemoteError> {
        if let Some(entry) = self.shared.tabs.lock().unwrap().get_mut(&tab) {
            entry.1 = enabled;
        }
        Ok(())
    }
    fn capture(&self, _tab: TabId, _timeout: Duration) -> Result<Vec<u8>, RemoteError> {
        Err(RemoteError::invalid("synthetic"))
    }
    fn cdp(
        &self,
        _tab: TabId,
        _method: &str,
        _params: serde_json::Value,
        _timeout: Duration,
    ) -> Result<serde_json::Value, RemoteError> {
        Err(RemoteError::invalid("synthetic"))
    }
    fn settle(&self, _tab: TabId, _timeout: Duration) -> Result<(), RemoteError> {
        Ok(())
    }
    fn take_notices(&self, _tab: TabId) -> Result<Notices, RemoteError> {
        Ok(Notices::default())
    }
    fn close_tab(&self, tab: TabId) {
        self.shared.tabs.lock().unwrap().remove(&tab);
    }
    fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.producer.lock().unwrap().take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

fn produce(shared: Arc<Shared>, queue: Arc<FrameQueue>, fps: u32) {
    let interval = (fps > 0).then(|| Duration::from_secs_f64(1.0 / f64::from(fps)));
    let mut buffers: BTreeMap<TabId, Vec<u8>> = BTreeMap::new();
    let mut next = Instant::now();
    while !shared.stop.load(Ordering::SeqCst) {
        let streaming: Vec<(TabId, Viewport)> = shared
            .tabs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, (_, on))| *on)
            .map(|(tab, (viewport, _))| (*tab, *viewport))
            .collect();
        if streaming.is_empty() {
            // Nothing to paint: wait for a state change (bounded park).
            std::thread::park_timeout(Duration::from_millis(5));
            next = Instant::now();
            continue;
        }
        for (tab, viewport) in streaming {
            let len = viewport.width as usize * viewport.height as usize * 4;
            let buffer = buffers.entry(tab).or_insert_with(|| vec![0x40; len]);
            buffer[..8].copy_from_slice(&stamp_nanos().to_le_bytes());
            queue.publish(tab, viewport.width, viewport.height, buffer);
        }
        if let Some(interval) = interval {
            // Load generator pacing: hold the frame rate, do not synchronize.
            next += interval;
            let now = Instant::now();
            if next > now {
                std::thread::park_timeout(next - now);
            } else {
                next = now;
            }
        }
    }
}

/// Starts the in-process server; returns the client's channels and the
/// server thread.
pub fn start(fps: u32) -> (UnixStream, UnixStream, JoinHandle<ExitReason>) {
    let (control, server_control) = UnixStream::pair().unwrap();
    let (frames, server_frames) = UnixStream::pair().unwrap();
    let handle = serve_in_thread(
        server_control,
        server_frames,
        ServeConfig::default(),
        move |context: BackendContext| {
            let shared = Arc::new(Shared {
                tabs: Mutex::new(BTreeMap::new()),
                stop: AtomicBool::new(false),
            });
            let producer = {
                let shared = Arc::clone(&shared);
                let queue = Arc::clone(&context.frames);
                std::thread::spawn(move || produce(shared, queue, fps))
            };
            Ok(Synthetic {
                shared,
                producer: Mutex::new(Some(producer)),
            })
        },
    );
    (control, frames, handle)
}
