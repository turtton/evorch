//! A production core composition with the real renderer helper (real CEF) behind a Unix
//! socket, and desktop windows of the core-client page connected to it through the real
//! connection actor. Shared by the CEF end-to-end test and the display measurement; both
//! are `#[ignore]`d (the `cef-e2e` job supplies `EVORCH_RENDERER_EXE`).
#![allow(dead_code)]
use async_trait::async_trait;
use egui_kittest::Harness;
use gui::core_client::{
    CoreClientScreen, Shared,
    connection::{Action, serve},
};
use runtime::artifacts::{ArtifactPages as _, ArtifactRecord, CaptureRequest, store_with_refs};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use workbench_host::areas::browser::run_pages::RuntimePages;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    ids::{ProjectId, ThreadId},
    namespaces::artifacts::ArtifactRef,
};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

pub const PROJECT: &str = "p1";
pub const THREAD: &str = "t1";
pub const RUN: &str = "r1";

/// A page that fills its canvas with a changing colour and stamps the time (0.1 ms units
/// since the epoch, modulo 2^32) into four 8x8 gray blocks at the top left; the same
/// fixture as `scripts/measure-renderer`, so a stamp decodes the same way everywhere.
pub const STAMP_PAGE: &str = r##"<!doctype html><html><body style="margin:0;overflow:hidden;background:#000">
<canvas id="c"></canvas>
<script>
const c = document.getElementById('c');
const g = c.getContext('2d', { alpha: false });
function fit() { c.width = window.innerWidth; c.height = window.innerHeight; }
fit();
window.addEventListener('resize', fit);
let n = 0;
(function draw() {
  n += 1;
  const v = n % 200;
  g.fillStyle = 'rgb(' + v + ',' + (255 - v) + ',160)';
  g.fillRect(0, 0, c.width, c.height);
  const t = Math.floor((performance.timeOrigin + performance.now()) * 10) % 4294967296;
  for (let i = 0; i < 4; i++) {
    const byte = Math.floor(t / Math.pow(256, i)) % 256;
    g.fillStyle = 'rgb(' + byte + ',' + byte + ',' + byte + ')';
    g.fillRect(i * 8, 0, 8, 8);
  }
  requestAnimationFrame(draw);
})();
</script></body></html>"##;

/// Units of 0.1 ms since the epoch, modulo 2^32.
pub fn now_ticks() -> u32 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    ((d.as_nanos() / 100_000) % (1u128 << 32)) as u32
}

/// The page's stamp in the pixels of a frame `width` pixels wide (gray blocks, so any
/// channel carries the byte).
pub fn stamp(pixel_byte_at: impl Fn(usize) -> u8, width: usize, height: usize) -> Option<u32> {
    if width < 32 || height < 8 {
        return None;
    }
    let mut value = 0u32;
    for i in 0..4 {
        value |= u32::from(pixel_byte_at(4 * width + i * 8 + 4)) << (8 * i);
    }
    Some(value)
}

/// Age in milliseconds of a stamp at `now`; `None` for what cannot be a real age.
pub fn age_ms(stamp: u32, now: u32) -> Option<f64> {
    let ms = f64::from(now.wrapping_sub(stamp) as i32) / 10.0;
    (-5.0..60_000.0).contains(&ms).then_some(ms)
}

struct Idle;
#[async_trait]
impl runtime::AgentModel for Idle {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "cef-support".into()
    }
}
struct Unisolated;
impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

pub struct Env {
    pub dir: test_socket_dir::SplitDir,
    pub root: PathBuf,
    pub boot: BootstrappedHost,
    pub identity: core_instance::ConfigIdentity,
    pub socket: PathBuf,
    pub artifacts: Vec<(ArtifactRef, ArtifactRecord)>,
    pub pages: RuntimePages,
    pub project: ProjectId,
    pub thread: ThreadId,
    _instance: evorch_core::instance::Instance,
    serving: tokio::task::JoinHandle<()>,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

/// The renderer helper the core starts: `EVORCH_RENDERER_EXE` (the cef-e2e job sets it).
pub fn use_real_helper() {
    let exe = std::env::var_os("EVORCH_RENDERER_EXE")
        .map(PathBuf::from)
        .expect("EVORCH_RENDERER_EXE names the built evorch-renderer");
    assert!(exe.is_absolute() && exe.is_file());
    assert!(exe.with_file_name("evorch-cef-helper").is_file());
    // SAFETY: called before any thread is started by the test.
    unsafe { std::env::set_var("EVORCH_RENDERER_EXEC", exe) };
}

/// A core whose thread `t1` holds one artifact per entry of `pages` (title, bytes, media
/// type), and whose run `r1` opened the first one for itself (a tab no client owns).
pub async fn start(pages: &[(&str, Vec<u8>, &str)]) -> Env {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    let (config, repo) = (root.join("config"), root.join("repo"));
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(StateProject::new(PROJECT), "repo", &repo)
        .unwrap();
    document.sidebar.threads.push(ThreadRecord::new(
        StateThread::new(THREAD),
        StateProject::new(PROJECT),
        "conversation",
    ));
    document.sidebar.refresh_derived();
    {
        let store = SidebarStore::new(config.join("sidebar.json"));
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let boot = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: config.clone(),
            project_root: repo.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        },
        TestSeams {
            model: Arc::new(Idle),
            sandbox: Arc::new(Unisolated),
        },
    )
    .await
    .unwrap();
    let writer = boot.composition.storage_writer.clone();
    writer
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: THREAD.into(),
            project_id: Some(PROJECT.into()),
            project_root: Some(repo.to_string_lossy().into_owned()),
            title: Some("conversation".into()),
            root_run_id: Some(RUN.into()),
            ..Default::default()
        }])
        .unwrap();
    let store = store_with_refs(config.join("artifacts"), writer).unwrap();
    let mut artifacts = Vec::new();
    for (index, (title, bytes, _)) in pages.iter().enumerate() {
        let file = repo.join(format!("{index}-{title}"));
        std::fs::write(&file, bytes).unwrap();
        let record = store
            .capture(
                &repo,
                None,
                &file,
                CaptureRequest {
                    title,
                    caption: None,
                    run_id: RUN.into(),
                    root_run_id: RUN.into(),
                    thread_id: Some(THREAD.into()),
                },
            )
            .unwrap();
        artifacts.push((
            ArtifactRef {
                project: ProjectId::new(PROJECT).unwrap(),
                thread: ThreadId::new(THREAD).unwrap(),
                artifact_id: record.artifact_id.clone(),
                digest: record.sha256.clone(),
            },
            record,
        ));
    }
    let pages_of_runs = RuntimePages::new(boot.host.lock().unwrap().renderer_owner());

    let identity = core_instance::ConfigIdentity::new(&root.join("identity")).unwrap();
    let instance = evorch_core::instance::Instance::acquire(
        &core_instance::LaunchGuard::acquire(&identity).unwrap(),
        dir.sockets(),
    )
    .unwrap();
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let backend = Arc::new(evorch_core::backend::HostBackend::new(
        boot.host.clone(),
        instance.identity.clone(),
    ));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let serving = tokio::spawn(async move {
        let _stop = stop;
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(evorch_core::unix::connection(
                stream,
                backend.clone(),
                stopped.clone(),
            ));
        }
    });
    let socket = instance.socket.clone();
    Env {
        dir,
        root,
        boot,
        identity,
        socket,
        artifacts,
        pages: pages_of_runs,
        project: ProjectId::new(PROJECT).unwrap(),
        thread: ThreadId::new(THREAD).unwrap(),
        _instance: instance,
        serving,
    }
}

impl Env {
    /// The run `r1` opens artifact `index` for itself: a tab no client owns.
    pub async fn run_opens(&self, index: usize, html: &[u8]) {
        self.pages
            .open_artifact(RUN, html.to_vec(), &self.artifacts[index].1)
            .await
            .unwrap();
    }
    pub fn run_tabs(&self) -> usize {
        self.boot
            .host
            .lock()
            .unwrap()
            .renderer_run_tabs()
            .unwrap()
            .len()
    }
}

/// One desktop window: the whole core-client page in a kittest harness, connected through
/// the production actor.
pub struct Window {
    pub harness: Harness<'static>,
    pub shared: Arc<Mutex<Shared>>,
    pub actions: tokio::sync::mpsc::Sender<Action>,
    pub actor: tokio::task::JoinHandle<()>,
}

pub fn open_window(
    env: &Env,
    size: egui::Vec2,
    renderer: Option<impl egui_kittest::TestRenderer + 'static>,
) -> Window {
    let launcher = core_launcher::Launcher::with_socket(
        env.identity.clone(),
        core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
        gui::core_client::client_config(),
        env.socket.clone(),
    )
    .unwrap();
    let (actions, rx) = tokio::sync::mpsc::channel(64);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let actor = tokio::spawn(serve(
        launcher,
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    let mut screen = CoreClientScreen::new(
        shared.clone(),
        actions.clone(),
        Some(env.project.clone()),
        Some(env.thread.clone()),
    );
    let mut builder = Harness::builder().with_size(size);
    if let Some(renderer) = renderer {
        builder = builder.renderer(renderer);
    }
    let harness = builder.build_ui(move |ui| screen.ui(ui));
    Window {
        harness,
        shared,
        actions,
        actor,
    }
}

impl Window {
    /// Waits for an explicit change of the shared client, then draws once more.
    pub async fn until(&mut self, mut done: impl FnMut(&mut Window) -> bool) {
        let changed = self.shared.lock().unwrap().changed.clone();
        loop {
            let notification = changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            self.harness.run_steps(8);
            if done(self) {
                return;
            }
            notification.await;
        }
    }
}
