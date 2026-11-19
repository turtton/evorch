//! Measurement of the renderer data plane through the product path (plan §5,
//! "性能・資源"): the fake helper paints stamped frames at a fixed rate, the core
//! reads them from the real helper protocol and shares them, and real client
//! library readers receive them over the core's Unix socket. The latency of a
//! frame is the time from the helper stamping it (right before it writes the
//! first byte, so an upper bound of "the core saw the first byte") to the
//! client holding every byte. The wall clock is shared by both processes.
//!
//! This is evidence for `docs/renderer-measurement.md`, not a CI assertion:
//! every test is `#[ignore]`, asserts only delivery, and prints one JSON line
//! (also appended to `$EVORCH_MEASURE_OUT` when set). Run one test per process:
//!
//! `cargo nextest run -p evorch-core --features renderer-test-helper \
//!   --test renderer_data_plane_measure --run-ignored only --no-capture`
#![cfg(all(target_os = "linux", feature = "renderer-test-helper"))]

use std::{sync::Arc, time::Duration};

use evorch_core::{backend::HostBackend, unix};
use runtime::artifacts::{CaptureRequest, store_with_refs};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    ids::{ProjectId, ThreadId},
    namespaces::{artifacts::ArtifactRef, browser::*},
};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

#[allow(dead_code)]
#[path = "renderer_support/data_peer.rs"]
mod data_peer;
use data_peer::Peer;

/// Counts what this process allocates (the core, its clients and the test
/// harness; the helper is another process). Heap bytes still live after the
/// transfers are released are what the data plane retains.
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    pub static CALLS: AtomicU64 = AtomicU64::new(0);
    pub static BYTES: AtomicU64 = AtomicU64::new(0);
    /// Allocations of 1 MiB or more: frame-sized buffers, not bookkeeping.
    pub static LARGE: AtomicU64 = AtomicU64::new(0);
    pub static LIVE: AtomicU64 = AtomicU64::new(0);
    pub static PEAK: AtomicU64 = AtomicU64::new(0);

    pub struct Counting;
    // SAFETY: every call is forwarded to `System` unchanged; only counters are touched.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note(layout.size());
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            LIVE.fetch_sub(layout.size() as u64, Relaxed);
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            LIVE.fetch_sub(layout.size() as u64, Relaxed);
            note(new_size);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
    fn note(size: usize) {
        CALLS.fetch_add(1, Relaxed);
        BYTES.fetch_add(size as u64, Relaxed);
        if size >= 1 << 20 {
            LARGE.fetch_add(1, Relaxed);
        }
        let live = LIVE.fetch_add(size as u64, Relaxed) + size as u64;
        PEAK.fetch_max(live, Relaxed);
    }
}
#[global_allocator]
static ALLOCATOR: counting::Counting = counting::Counting;

/// The measurement window, in `now_nanos` time: `[start, end)`. Both are unset
/// until the window is open and closed. Which frames fall inside is decided only
/// after every reader has ended (see [`totals`]), by the final bounds, so a
/// frame still being recorded when the window closes is neither lost nor
/// counted by a snapshot taken too early.
struct Window {
    start: std::sync::atomic::AtomicU64,
    end: std::sync::atomic::AtomicU64,
}
impl Window {
    fn new() -> Self {
        Self {
            start: std::sync::atomic::AtomicU64::new(u64::MAX),
            end: std::sync::atomic::AtomicU64::new(u64::MAX),
        }
    }
    fn open(&self, at: u64) {
        self.start.store(at, std::sync::atomic::Ordering::Relaxed);
    }
    fn close(&self, at: u64) {
        self.end.store(at, std::sync::atomic::Ordering::Relaxed);
    }
    fn contains(&self, at: u64) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        at >= self.start.load(Relaxed) && at < self.end.load(Relaxed)
    }
    fn span_secs(&self) -> f64 {
        use std::sync::atomic::Ordering::Relaxed;
        self.end
            .load(Relaxed)
            .saturating_sub(self.start.load(Relaxed)) as f64
            / 1e9
    }
}

/// Proportional set size in KiB of `pid` (`self` for this process).
fn pss_kib(pid: &str) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("Pss:"))
                .map(|rest| {
                    rest.trim()
                        .trim_end_matches(" kB")
                        .trim()
                        .parse()
                        .unwrap_or(0)
                })
        })
        .unwrap_or(0)
}

/// PSS in KiB of this process's child processes (the helper).
fn children_pss_kib() -> u64 {
    let me = std::process::id();
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{name}/stat")).ok()?;
            let after = &stat[stat.rfind(')')? + 2..];
            let parent: u32 = after.split(' ').nth(1)?.parse().ok()?;
            (parent == me).then(|| pss_kib(&name))
        })
        .sum()
}

const FPS: u32 = 30;
const WINDOW: Duration = Duration::from_secs(8);
/// Frames dropped from each reader's statistics while the pipeline warms up.
const WARMUP_FRAMES: usize = 10;

struct Idle;
#[async_trait::async_trait]
impl runtime::AgentModel for Idle {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        unreachable!("no turn runs in this fixture")
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "renderer-data-plane-measure".into()
    }
}
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    width: u32,
    height: u32,
    /// One client, session and tab each.
    tabs: usize,
    /// Reading subscriptions per tab.
    readers: usize,
    /// Subscriptions on the first tab that bind and never read.
    stalled: usize,
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// CPU seconds of this process (user + system) from `/proc/self/stat`.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = after.split(' ').collect();
    (fields[11].parse::<f64>().unwrap() + fields[12].parse::<f64>().unwrap()) / 100.0
}

fn status_kib(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .map(|rest| rest.trim().trim_end_matches(" kB").trim().parse().unwrap())
        .unwrap_or(0)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

/// What one reading subscription saw: when each frame was received and when the
/// helper stamped it. Nothing is counted here; [`totals`] applies the window
/// once every reader has ended.
#[derive(Default)]
struct Reader {
    /// `(received, stamped)` of every frame, window or not, in `now_nanos` time.
    arrivals: Vec<(u64, u64)>,
}
impl Reader {
    fn arrived(&mut self, received: u64, stamped: u64) {
        self.arrivals.push((received, stamped));
    }
}

/// The frames of all readers inside the window, counted after every reader has
/// ended. This is the only source of the frame count and so of the denominators
/// of the per-frame figures.
struct Totals {
    frames: u64,
    /// Every frame received, window or not.
    received: u64,
    /// Latencies of the frames inside the window, sorted.
    latencies_ms: Vec<f64>,
    worst_reader_p95_ms: f64,
    /// The fewest window frames any one reader got.
    fewest_reader_frames: u64,
}
fn totals(window: &Window, readers: &[Reader]) -> Totals {
    let mut all = Vec::new();
    let mut worst_reader_p95_ms = 0f64;
    let mut fewest_reader_frames = u64::MAX;
    let mut received = 0;
    for reader in readers {
        received += reader.arrivals.len() as u64;
        let mut own = reader
            .arrivals
            .iter()
            .filter(|(at, _)| window.contains(*at))
            .map(|(at, stamped)| at.saturating_sub(*stamped) as f64 / 1e6)
            .collect::<Vec<_>>();
        fewest_reader_frames = fewest_reader_frames.min(own.len() as u64);
        own.sort_by(f64::total_cmp);
        worst_reader_p95_ms = worst_reader_p95_ms.max(percentile(&own, 0.95));
        all.extend(own);
    }
    all.sort_by(f64::total_cmp);
    Totals {
        frames: all.len() as u64,
        received,
        latencies_ms: all,
        worst_reader_p95_ms,
        fewest_reader_frames: if readers.is_empty() {
            0
        } else {
            fewest_reader_frames
        },
    }
}

fn run_scenario(scenario: Scenario) {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    // SAFETY: single-threaded at this point; one scenario per process.
    unsafe {
        let link = root.join(format!("synth-{FPS}fps"));
        std::os::unix::fs::symlink(
            std::env::var_os("CARGO_BIN_EXE_evorch-renderer-test-helper").unwrap(),
            &link,
        )
        .unwrap();
        std::env::set_var("EVORCH_RENDERER_EXEC", link);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(measure(scenario, dir, root));
}

async fn measure(scenario: Scenario, dir: test_socket_dir::SplitDir, root: std::path::PathBuf) {
    let heap_at_test_start = counting::LIVE.load(std::sync::atomic::Ordering::Relaxed);
    let project_root = root.join("one");
    std::fs::create_dir_all(&project_root).unwrap();
    let store = SidebarStore::new(root.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(StateProject::new("p1"), "p1", &project_root)
        .unwrap();
    document.sidebar.threads.push(ThreadRecord::new(
        StateThread::new("t1"),
        StateProject::new("p1"),
        "t1",
    ));
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let mut core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: project_root.clone(),
            ownership_root: dir.sockets().into(),
        },
        TestSeams {
            model: Arc::new(Idle),
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    let writer = core.composition.storage_writer.clone();
    writer
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "t1".into(),
            project_id: Some("p1".into()),
            project_root: Some(project_root.to_string_lossy().into_owned()),
            title: Some("t1".into()),
            root_run_id: Some("r1".into()),
            ..Default::default()
        }])
        .unwrap();
    std::fs::write(project_root.join("page.html"), "<p>measure</p>").unwrap();
    let artifacts = store_with_refs(root.join("artifacts"), writer).unwrap();
    let record = artifacts
        .capture(
            &project_root,
            None,
            &project_root.join("page.html"),
            CaptureRequest {
                title: "page.html",
                caption: None,
                run_id: "r1".into(),
                root_run_id: "r1".into(),
                thread_id: Some("t1".into()),
            },
        )
        .unwrap();
    let page = ArtifactRef {
        project: ProjectId::new("p1").unwrap(),
        thread: ThreadId::new("t1").unwrap(),
        artifact_id: record.artifact_id.clone(),
        digest: record.sha256.clone(),
    };

    let socket = dir.sockets().join("renderer-data-plane-measure.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let backend = Arc::new(HostBackend::new(core.host.clone(), "measure".into()));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (backend, stopped) = (backend.clone(), stopped.clone());
            tokio::spawn(async move {
                let _ = unix::connection(stream, backend, stopped).await;
            });
        }
    });

    // One client per tab, each with its own session, so the subscription
    // limit of a client never decides the scenario.
    let window = Arc::new(Window::new());
    let mut peers = Vec::new();
    let mut readers = Vec::new();
    let mut held = Vec::new();
    for index in 0..scenario.tabs {
        let mut peer = Peer::connect(&socket, &format!("measure-{index}")).await;
        let session = peer.session().await;
        let id = peer
            .accepted(workbench_protocol::commands::Command::Artifacts(
                workbench_protocol::namespaces::artifacts::ArtifactsCommand::OpenPreview {
                    artifact: page.clone(),
                    session: session.clone(),
                    viewport: RendererViewport {
                        width: scenario.width,
                        height: scenario.height,
                    },
                },
            ))
            .await
            .unwrap();
        let RendererOutput::Tab(tab) = peer.output(&id).await else {
            panic!("not a tab")
        };
        let live = scenario.readers;
        let idle = if index == 0 { scenario.stalled } else { 0 };
        for number in 0..live + idle {
            let op = peer
                .browser(BrowserCommand::SubscribeRendererFrames {
                    tab: tab.reference.clone(),
                })
                .await
                .unwrap();
            let RendererOutput::Subscription {
                subscription,
                token,
                ..
            } = peer.output(&op).await
            else {
                panic!("not a subscription")
            };
            let stream = peer
                .channel()
                .await
                .bind_frames(&peer.client, subscription, token)
                .await
                .unwrap();
            if number < live {
                readers.push(tokio::spawn(async move {
                    let mut stream = stream;
                    let mut bytes = Vec::new();
                    let mut stats = Reader::default();
                    loop {
                        match stream.next_frame_into(&mut bytes).await {
                            Ok(Some(_)) => {
                                let received = now_nanos();
                                let stamp = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                                stats.arrived(received, stamp);
                            }
                            other => {
                                eprintln!(
                                    "reader ended: {:?} after {} frames",
                                    other.map(|_| ()),
                                    stats.arrivals.len()
                                );
                                return stats;
                            }
                        }
                    }
                }));
            } else {
                held.push(stream);
            }
        }
        peers.push((peer, session, tab));
    }

    use std::sync::atomic::Ordering::Relaxed;
    for (peer, _, tab) in &peers {
        peer.browser(BrowserCommand::SetRendererStreaming {
            tab: tab.reference.clone(),
            enabled: true,
        })
        .await
        .unwrap();
    }
    // The warm-up before the window: a load duration, not a synchronization.
    tokio::time::sleep(Duration::from_secs_f64(
        (WARMUP_FRAMES as f64 + 2.0) / f64::from(FPS),
    ))
    .await;
    let cpu_before = cpu_seconds();
    let (calls_before, bytes_before, large_before) = (
        counting::CALLS.load(Relaxed),
        counting::BYTES.load(Relaxed),
        counting::LARGE.load(Relaxed),
    );
    let live_before = counting::LIVE.load(Relaxed);
    let heap_at_window_start = live_before.saturating_sub(heap_at_test_start);
    counting::PEAK.store(live_before, Relaxed);
    // A frame counts when it was received inside [start, end); readers only
    // record arrivals, and the count is made after they have all ended.
    let started = std::time::Instant::now();
    window.open(now_nanos());
    // The measurement window itself: a load duration, not a synchronization.
    let (mut pss_core, mut pss_helper) = (0, 0);
    while started.elapsed() < WINDOW {
        tokio::time::sleep(Duration::from_millis(500)).await;
        pss_core = pss_core.max(pss_kib("self"));
        pss_helper = pss_helper.max(children_pss_kib());
    }
    window.close(now_nanos());
    let cpu = cpu_seconds() - cpu_before;
    let elapsed = started.elapsed().as_secs_f64();
    let (calls, bytes, large) = (
        counting::CALLS.load(Relaxed) - calls_before,
        counting::BYTES.load(Relaxed) - bytes_before,
        counting::LARGE.load(Relaxed) - large_before,
    );
    let peak_live = counting::PEAK.load(Relaxed).saturating_sub(live_before);
    let rss_peak_mib = status_kib("VmHWM:") / 1024;
    // Closing the session closes its tab; nothing waits for the result, because
    // the tab going away may cancel the close operation itself.
    for (peer, session, _) in &peers {
        let _ = peer
            .browser(BrowserCommand::CloseRendererSession {
                session: session.clone(),
            })
            .await;
    }
    // The frame count, and so every per-frame denominator below, is fixed here,
    // after every reader has been joined.
    let mut ended = Vec::new();
    for reader in readers {
        ended.push(reader.await.unwrap());
    }
    let totals = totals(&window, &ended);
    assert!(
        totals.fewest_reader_frames > 0,
        "a reading subscription got no frame"
    );
    let latencies = totals.latencies_ms;
    let frames = totals.frames;
    let worst_reader_p95 = totals.worst_reader_p95_ms;
    let seen = frames.max(1);
    let window_secs = window.span_secs();
    let subscribers = scenario.tabs * scenario.readers;
    let line = serde_json::json!({
        "scenario": scenario.name,
        "width": scenario.width, "height": scenario.height, "fps": FPS,
        "tabs": scenario.tabs, "readers_per_tab": scenario.readers,
        "stalled": scenario.stalled,
        "samples": latencies.len(),
        "p50_ms": percentile(&latencies, 0.5),
        "p95_ms": percentile(&latencies, 0.95),
        "p99_ms": percentile(&latencies, 0.99),
        "max_ms": latencies.last().copied().unwrap_or(0.0),
        "worst_reader_p95_ms": worst_reader_p95,
        "delivered_fps_per_reader": frames as f64 / subscribers as f64 / window_secs,
        "missed_frames_per_reader": (FPS as f64 * window_secs) - frames as f64 / subscribers as f64,
        "cpu_cores": cpu / elapsed,
        "rss_peak_mib": rss_peak_mib,
    });
    // Everything is released: what stays on the heap is retained by the data
    // plane. The wait is a settling time for background tasks, not a
    // synchronization with the transfers (readers have all ended above).
    drop((peers, held));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let heap_after_close = counting::LIVE
        .load(Relaxed)
        .saturating_sub(heap_at_test_start);
    let mut line = line;
    line["frames_received_in_window"] = frames.into();
    line["frames_received_total"] = totals.received.into();
    line["window_secs"] = window_secs.into();
    line["pss_core_process_mib"] = (pss_core / 1024).into();
    line["pss_helper_mib"] = (pss_helper / 1024).into();
    line["allocs_per_received_frame"] = (calls as f64 / seen as f64).into();
    line["alloc_bytes_per_received_frame"] = (bytes as f64 / seen as f64).into();
    line["large_allocs_in_window"] = large.into();
    line["large_allocs_per_received_frame"] = (large as f64 / seen as f64).into();
    line["peak_live_heap_mib"] = (peak_live / (1 << 20)).into();
    line["heap_in_use_at_window_start_kib"] = (heap_at_window_start / 1024).into();
    line["heap_in_use_after_close_requested_kib"] = (heap_after_close / 1024).into();
    println!("MEASURE {line}");
    if let Some(path) = std::env::var_os("EVORCH_MEASURE_OUT") {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }
    let _ = stop.send(true);
    server.abort();
    core.shutdown().await.unwrap();
}

macro_rules! scenario {
    ($test:ident, $w:expr, $h:expr, $tabs:expr, $readers:expr, $stalled:expr) => {
        #[test]
        #[ignore = "measurement, run one per process (see the module comment)"]
        fn $test() {
            run_scenario(Scenario {
                name: stringify!($test),
                width: $w,
                height: $h,
                tabs: $tabs,
                readers: $readers,
                stalled: $stalled,
            });
        }
    };
}

scenario!(p720_1tab_1reader, 1280, 720, 1, 1, 0);
scenario!(p720_4tab_1reader, 1280, 720, 4, 1, 0);
scenario!(p720_1tab_8reader, 1280, 720, 1, 8, 0);
scenario!(p720_4tab_2reader, 1280, 720, 4, 2, 0);
scenario!(p1080_4tab_2reader, 1920, 1080, 4, 2, 0);
scenario!(k4_1tab_1reader, 3840, 2160, 1, 1, 0);
scenario!(k4_1tab_4reader, 3840, 2160, 1, 4, 0);
scenario!(k4_1tab_3reader_1stalled, 3840, 2160, 1, 3, 1);

/// The end of the window races with a reader that has already received a frame
/// inside it. The barriers hold that reader between receiving (the timestamp is
/// taken) and recording, close the window meanwhile, and only then let it go:
/// the frame must still be counted, once, in the count the per-frame figures
/// divide by, and a frame received after the close must not.
#[test]
fn a_frame_received_in_the_window_is_counted_even_when_recorded_after_it_closed() {
    use std::sync::Barrier;
    let window = Arc::new(Window::new());
    let received = Arc::new(Barrier::new(2));
    let closed = Arc::new(Barrier::new(2));
    let reader = {
        let (received, closed) = (received.clone(), closed.clone());
        std::thread::spawn(move || {
            let mut reader = Reader::default();
            // Received at 15 ms, inside [10 ms, 20 ms), stamped 5 ms earlier.
            let at = 15_000_000;
            received.wait();
            closed.wait();
            reader.arrived(at, at - 5_000_000);
            // Received at 25 ms, after the window closed.
            reader.arrived(25_000_000, 24_000_000);
            reader
        })
    };
    window.open(10_000_000);
    received.wait();
    window.close(20_000_000);
    closed.wait();
    let ended = [reader.join().unwrap()];
    let totals = totals(&window, &ended);
    assert_eq!(totals.frames, 1);
    assert_eq!(totals.received, 2);
    assert_eq!(totals.latencies_ms, vec![5.0]);
    assert_eq!(totals.fewest_reader_frames, 1);
    assert!((window.span_secs() - 0.01).abs() < 1e-12);
}
