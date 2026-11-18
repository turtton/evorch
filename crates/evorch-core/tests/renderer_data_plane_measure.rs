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

#[derive(Default)]
struct Reader {
    latencies_ms: Vec<f64>,
    frames: u64,
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
                                stats.frames += 1;
                                if stats.frames as usize > WARMUP_FRAMES {
                                    stats
                                        .latencies_ms
                                        .push(received.saturating_sub(stamp) as f64 / 1e6);
                                }
                            }
                            other => {
                                eprintln!(
                                    "reader ended: {:?} after {} frames",
                                    other.map(|_| ()),
                                    stats.frames
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

    let cpu_before = cpu_seconds();
    let started = std::time::Instant::now();
    for (peer, _, tab) in &peers {
        peer.browser(BrowserCommand::SetRendererStreaming {
            tab: tab.reference.clone(),
            enabled: true,
        })
        .await
        .unwrap();
    }
    // The measurement window itself: a load duration, not a synchronization.
    tokio::time::sleep(WINDOW).await;
    let cpu = cpu_seconds() - cpu_before;
    let elapsed = started.elapsed().as_secs_f64();
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
    let mut latencies = Vec::new();
    let mut frames = 0u64;
    let mut worst_reader_p95 = 0f64;
    for reader in readers {
        let stats = reader.await.unwrap();
        assert!(stats.frames > 0, "a reading subscription got no frame");
        let mut own = stats.latencies_ms.clone();
        own.sort_by(f64::total_cmp);
        worst_reader_p95 = worst_reader_p95.max(percentile(&own, 0.95));
        latencies.extend(stats.latencies_ms);
        frames += stats.frames;
    }
    latencies.sort_by(f64::total_cmp);
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
        "delivered_fps_per_reader": frames as f64 / subscribers as f64 / elapsed,
        "missed_frames_per_reader": (FPS as f64 * elapsed) - frames as f64 / subscribers as f64,
        "cpu_cores": cpu / elapsed,
        "rss_peak_mib": rss_peak_mib,
    });
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
    drop((peers, held));
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
scenario!(k4_1tab_4reader_1stalled, 3840, 2160, 1, 4, 1);
scenario!(k4_1tab_8reader, 3840, 2160, 1, 8, 0);
