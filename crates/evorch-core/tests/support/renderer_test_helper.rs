//! A fake `evorch-renderer` for the owned-spawn tests (PR 16a-2): it takes the
//! control channel from stdin and the frame channel from stdout exactly as the
//! real helper does, serves the `renderer-ipc` protocol with a backend that
//! renders no page (a capture is a fixed PNG, a `DOM.getDocument` probe answers with free text or a secret CDP result), and exits when the core shuts it down or closes the
//! control channel. No CEF, no timers: every step waits on a message.
//!
//! `evorch-renderer-test-helper --cache-dir <absolute dir>` (the flag is
//! accepted so the core starts it like the real helper, and is otherwise
//! unused). Not shipped; built only with the `renderer-test-helper` feature.
use renderer_ipc::{
    Backend, BackendContext, ExitReason, FrameQueue, Notices, OpenArtifact, RemoteError,
    ServeConfig, TabId, Viewport, serve,
};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

/// What a capture returns: a PNG signature and a marker the leak checks look for.
const FAKE_PIXELS: &[u8] = b"\x89PNG\r\n\x1a\nFAKE-PIXELS";

struct Blank {
    /// How many `DOM.getDocument` probes reached this helper.
    probes: std::sync::atomic::AtomicUsize,
    shutdown_gate: Option<std::path::PathBuf>,
    frames: std::sync::Arc<FrameQueue>,
    /// How many frames were published; the fill byte of the next one.
    painted: std::sync::atomic::AtomicU8,
    /// Set when the helper was started through a link named `synth-<N>fps`
    /// (the core clears the environment, so the name is the only channel):
    /// paint stamped frames of the tab's viewport at N per second while
    /// streaming is on, for the measurement of the data plane.
    synth_fps: Option<u32>,
    /// Per tab: its viewport and the flag that keeps its producer running.
    synth: std::sync::Mutex<
        std::collections::BTreeMap<
            TabId,
            (Viewport, std::sync::Arc<std::sync::atomic::AtomicBool>),
        >,
    >,
}

impl Blank {
    /// One 4x2 frame whose bytes all equal how many frames came before it.
    /// Painting is driven by a request (streaming on, resize), never a timer.
    /// Stamps every frame with the wall clock (nanoseconds since the Unix
    /// epoch) in its first eight bytes, taken right before it is published, so
    /// a reader in another process can tell how long the frame took to arrive.
    fn synthesize(&self, tab: TabId, fps: u32) {
        use std::sync::atomic::Ordering::SeqCst;
        let Some((viewport, on)) = self.synth.lock().unwrap().get(&tab).cloned() else {
            return;
        };
        if on.swap(true, SeqCst) {
            return;
        }
        let frames = self.frames.clone();
        std::thread::spawn(move || {
            let mut bgra = vec![0x40u8; viewport.width as usize * viewport.height as usize * 4];
            let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
            let mut next = std::time::Instant::now();
            while on.load(SeqCst) {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64;
                bgra[..8].copy_from_slice(&stamp.to_le_bytes());
                frames.publish(tab, viewport.width, viewport.height, &bgra);
                next += interval;
                let now = std::time::Instant::now();
                if next > now {
                    std::thread::park_timeout(next - now);
                } else {
                    next = now;
                }
            }
        });
    }

    fn paint(&self, tab: TabId) {
        let fill = self
            .painted
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .wrapping_add(1);
        self.frames.publish(tab, 4, 2, &[fill; 4 * 2 * 4]);
    }
}

impl Backend for Blank {
    fn open_artifact(
        &self,
        open: &OpenArtifact,
        _files: Vec<(String, String, Vec<u8>)>,
    ) -> Result<Viewport, RemoteError> {
        if self.synth_fps.is_some() {
            self.synth.lock().unwrap().insert(
                open.tab,
                (open.viewport, std::sync::Arc::new(Default::default())),
            );
        }
        Ok(open.viewport)
    }
    fn resize(&self, tab: TabId, _: Viewport) -> Result<(), RemoteError> {
        self.paint(tab);
        Ok(())
    }
    fn set_streaming(&self, tab: TabId, enabled: bool) -> Result<(), RemoteError> {
        if let Some(fps) = self.synth_fps {
            if enabled {
                self.synthesize(tab, fps);
            } else if let Some((_, on)) = self.synth.lock().unwrap().get(&tab) {
                on.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            return Ok(());
        }
        if enabled {
            self.paint(tab);
        }
        Ok(())
    }
    fn set_frame_rate(&self, _: TabId, _: u8) -> Result<(), RemoteError> {
        Ok(())
    }
    fn capture(&self, _: TabId, _: Duration) -> Result<Vec<u8>, RemoteError> {
        Ok(FAKE_PIXELS.to_vec())
    }
    fn cdp(
        &self,
        _: TabId,
        method: &str,
        params: serde_json::Value,
        _: Duration,
    ) -> Result<serde_json::Value, RemoteError> {
        // `DOM.getDocument` is an allowed method, so a probe reaches this helper
        // through the real validation. `{"probe":"error"}` answers with free text
        // and `{"probe":"secret"}` with a result the owner may read and nobody
        // else may see; both report how many probes this helper has served.
        if method == "DOM.getDocument"
            && let Some(probe) = params["probe"].as_str()
        {
            let served = self
                .probes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            return match probe {
                "error" => Err(RemoteError::internal("HELPER-FREE-TEXT")),
                _ => Ok(serde_json::json!({
                    "secret": "HELPER-CDP-SECRET", "served": served
                })),
            };
        }
        // Enough of a page for `inspect_artifact`'s snapshot and click.
        Ok(match method {
            "Accessibility.getFullAXTree" => serde_json::json!({"nodes": [{
                "nodeId": "1", "role": {"value": "button"}, "name": {"value": "Add"},
                "backendDOMNodeId": 7
            }]}),
            "DOM.getContentQuads" => {
                serde_json::json!({"quads": [[10, 20, 30, 20, 30, 40, 10, 40]]})
            }
            _ => serde_json::Value::Null,
        })
    }
    fn settle(&self, _: TabId, _: Duration) -> Result<(), RemoteError> {
        Ok(())
    }
    fn take_notices(&self, _: TabId) -> Result<Notices, RemoteError> {
        Ok(Notices::default())
    }
    fn close_tab(&self, tab: TabId) {
        if let Some((_, on)) = self.synth.lock().unwrap().remove(&tab) {
            on.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    fn shutdown(&self) {
        if let Some(path) = &self.shutdown_gate {
            use std::io::{Read, Write};
            let mut gate = UnixStream::connect(path).unwrap();
            gate.write_all(b"R").unwrap();
            gate.read_exact(&mut [0]).unwrap();
        }
    }
}

fn synth_fps() -> Option<u32> {
    let name = std::env::args_os().next().map(std::path::PathBuf::from)?;
    name.file_name()?
        .to_str()?
        .strip_prefix("synth-")?
        .strip_suffix("fps")?
        .parse()
        .ok()
}

fn adopt(fd: impl AsFd) -> std::io::Result<UnixStream> {
    Ok(UnixStream::from(fd.as_fd().try_clone_to_owned()?))
}

fn fixture_root() -> std::path::PathBuf {
    let args: Vec<_> = std::env::args_os().collect();
    let index = args
        .iter()
        .position(|s| s == "--cache-dir")
        .expect("cache directory");
    std::path::PathBuf::from(&args[index + 1])
}
fn assert_not_subreaper() {
    #[cfg(target_os = "linux")]
    assert_eq!(
        rustix::process::child_subreaper().unwrap(),
        None,
        "core must not become a subreaper"
    );
}
fn owner_core() -> ExitCode {
    use std::io::Write;
    use workbench_host::areas::{
        AreaTasks,
        browser::renderer::{OwnerConfig, RendererOwner},
    };
    let args: Vec<_> = std::env::args_os().collect();
    let executable = std::path::PathBuf::from(&args[2]);
    let profiles = std::path::PathBuf::from(&args[3]);
    assert_not_subreaper();
    // Intentionally inheritable sentinel: apply_launch_policy must close it.
    let sentinel = std::fs::File::open("/dev/null").unwrap();
    rustix::io::fcntl_setfd(&sentinel, rustix::io::FdFlags::empty()).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let tasks = AreaTasks::new(tokio::runtime::Handle::current());
            let ledger = workbench_host::areas::browser::renderer::SpawnLedger::new();
            let owner = RendererOwner::new(
                &tasks,
                ledger.clone(),
                OwnerConfig::new(executable, profiles),
            )
            .unwrap();
            let (open, body) = OpenArtifact::with_files(
                renderer_ipc::SessionId(1),
                TabId(1),
                Viewport {
                    width: 64,
                    height: 48,
                },
                30,
                30_000,
                vec![(
                    "index.html".into(),
                    "text/html".into(),
                    b"<p>EOF cleanup</p>".to_vec(),
                )],
            );
            owner.open(open, body).await.unwrap();
            assert_not_subreaper();
            println!("{}", ledger.children()[0].leader.unwrap());
            std::io::stdout().flush().unwrap();
            std::future::pending::<()>().await;
        });
    ExitCode::SUCCESS
}
fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--owner-core") {
        return owner_core();
    }
    use std::io::{Read, Write};
    if std::env::args().any(|s| s == "--grandchild") {
        std::io::stdout().write_all(b"R").unwrap();
        std::io::stdout().flush().unwrap();
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).unwrap();
        return ExitCode::SUCCESS;
    }
    assert!(
        std::env::var_os("EVORCH_RENDERER_TEST_SECRET").is_none(),
        "credential environment inherited"
    );
    #[cfg(target_os = "linux")]
    for entry in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
        let fd: i32 = entry.file_name().to_string_lossy().parse().unwrap();
        if fd <= 2 {
            continue;
        }
        if let Ok(target) = std::fs::read_link(entry.path()) {
            assert!(
                target.to_string_lossy().ends_with("/fd"),
                "unexpected inherited FD {fd}: {target:?}"
            );
        }
    }
    let profile = fixture_root();
    let root = profile.parent().unwrap();
    let mode = std::fs::read_to_string(root.join("fixture-mode")).unwrap_or_default();
    let mut descendant = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--grandchild")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = [0];
    descendant
        .stdout
        .take()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    std::fs::write(profile.join("descendant"), descendant.id().to_string()).unwrap();
    let _lease = std::fs::File::open(profile.join("owner.lock"))
        .ok()
        .inspect(|f| {
            rustix::fs::flock(f, rustix::fs::FlockOperation::LockShared).unwrap();
        });
    if mode == "gate-hello" || mode == "fail-hello" {
        let mut gate = UnixStream::connect(root.join("gate.sock")).unwrap();
        gate.write_all(b"R").unwrap();
        gate.read_exact(&mut ready).unwrap();
        if mode == "fail-hello" {
            drop(descendant.stdin.take());
            descendant.wait().unwrap();
            return ExitCode::from(2);
        }
    }

    let channels = adopt(std::io::stdin()).and_then(|c| Ok((c, adopt(std::io::stdout())?)));
    let Ok((control, frames)) = channels else {
        eprintln!("evorch-renderer-test-helper: stdin and stdout must be sockets");
        drop(descendant.stdin.take());
        descendant.wait().unwrap();
        return ExitCode::from(2);
    };
    let shutdown_gate = (mode == "gate-shutdown").then(|| root.join("gate.sock"));
    let reason = serve(
        control,
        frames,
        ServeConfig::default(),
        move |context: BackendContext| {
            Ok(Blank {
                probes: Default::default(),
                shutdown_gate,
                frames: context.frames,
                painted: Default::default(),
                synth_fps: synth_fps(),
                synth: Default::default(),
            })
        },
    );
    drop(descendant.stdin.take());
    descendant.wait().unwrap();
    match reason {
        ExitReason::ShutdownRequested | ExitReason::PeerClosed => ExitCode::SUCCESS,
        ExitReason::InitFailed(_) | ExitReason::HandshakeFailed(_) => ExitCode::from(2),
        ExitReason::ProtocolViolation(_) | ExitReason::FrameChannelFailed(_) => ExitCode::from(1),
    }
}
