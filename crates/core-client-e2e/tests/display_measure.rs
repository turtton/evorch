//! Display-path measurement of the core-client page (Stage 16b PR V): the real helper (real
//! CEF) renders a stamped page, the production core shares its frames over the data
//! plane, the production connection actor and the browser feature of the page receive them,
//! convert them and hand them to the GUI thread, and the page's texture is uploaded to a
//! real wgpu device. The interval measured is
//!
//!   page stamp (the page paints the time into the pixels) -> the texture upload has
//!   finished (the staged write was submitted and the device went idle).
//!
//! That is *not* "shown on a screen": no window system, compositor or display is involved,
//! and the paint is recorded into an offscreen target. A second figure, the same stamp ->
//! the end of an offscreen paint pass of the page, is reported next to it.
//!
//! This is evidence for docs/renderer-measurement.md, not a CI assertion: every test is
//! `#[ignore]`d, asserts only that frames arrive, and prints one JSON line (also appended to
//! `$EVORCH_MEASURE_OUT`). Run one test per process, with a real helper:
//!
//! `EVORCH_RENDERER_EXE=$PWD/target/debug/evorch-renderer EVORCH_MEASURE_OUT=out.jsonl \
//!  cargo nextest run -p core-client-e2e --features gui/cef --test display_measure \
//!  --run-ignored only --no-capture -E 'test(s720_one_tab)'`
//!
//! `EVORCH_MEASURE_ADAPTER=cpu` picks a software adapter; `EVORCH_MEASURE_SECS` and
//! `EVORCH_SOAK_SECS` set the windows.
#![cfg(target_os = "linux")]
#![allow(clippy::too_many_lines)]

use std::{
    collections::HashMap,
    io::Write as _,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use egui_kittest::{TestRenderer, kittest::Queryable};
use egui_wgpu::wgpu;
use serde_json::{Value, json};
use workbench_protocol::namespaces::browser::*;

#[allow(dead_code)]
#[path = "../../evorch-core/tests/renderer_support/data_peer.rs"]
mod data_peer;
#[path = "cef_support/mod.rs"]
mod support;
use support::{Env, Window, age_ms, now_ticks, open_window, stamp};

/// Counts what this process allocates (the core, the client and the page).
mod counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    pub static LIVE: AtomicU64 = AtomicU64::new(0);
    pub struct Counting;
    // SAFETY: every call is forwarded to `System` unchanged; only a counter is touched.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            LIVE.fetch_add(layout.size() as u64, Relaxed);
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            LIVE.fetch_add(layout.size() as u64, Relaxed);
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            LIVE.fetch_sub(layout.size() as u64, Relaxed);
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            LIVE.fetch_sub(layout.size() as u64, Relaxed);
            LIVE.fetch_add(new_size as u64, Relaxed);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
}
#[global_allocator]
static ALLOCATOR: counting::Counting = counting::Counting;

// ---- the process ----------------------------------------------------------------------

fn field_kib(path: &str, key: &str) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix(key))
                .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
        })
        .unwrap_or(0)
}

fn cpu_seconds(path: &str) -> f64 {
    let Ok(stat) = std::fs::read_to_string(path) else {
        return 0.0;
    };
    let Some(after) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
        return 0.0;
    };
    let fields: Vec<&str> = after.split_whitespace().collect();
    let ticks = |i: usize| {
        fields
            .get(i)
            .and_then(|f| f.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    (ticks(11) + ticks(12)) / 100.0
}

/// Every descendant of this process (the helper and the CEF processes under it).
fn descendants() -> Vec<i32> {
    let mut parents = HashMap::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some(parent) = stat.rsplit_once(')').and_then(|(_, rest)| {
            rest.split_whitespace()
                .nth(1)
                .and_then(|p| p.parse::<i32>().ok())
        }) {
            parents.insert(pid, parent);
        }
    }
    let me = std::process::id() as i32;
    parents
        .keys()
        .copied()
        .filter(|&pid| {
            let mut at = pid;
            for _ in 0..64 {
                match parents.get(&at) {
                    Some(&parent) if parent == me => return true,
                    Some(&parent) if parent > 1 => at = parent,
                    _ => return false,
                }
            }
            false
        })
        .collect()
}

#[derive(Clone, Copy, Default)]
struct Cpu {
    process: f64,
    main_thread: f64,
    tree: f64,
}
fn cpu_now() -> Cpu {
    Cpu {
        process: cpu_seconds("/proc/self/stat"),
        main_thread: cpu_seconds("/proc/thread-self/stat"),
        tree: descendants()
            .iter()
            .map(|pid| cpu_seconds(&format!("/proc/{pid}/stat")))
            .sum(),
    }
}

// ---- the GPU -----------------------------------------------------------------------------

struct Gpu {
    state: egui_wgpu::RenderState,
    info: wgpu::AdapterInfo,
}

fn gpu() -> Arc<Gpu> {
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup
        .instance_descriptor
        .backends
        .remove(wgpu::Backends::BROWSER_WEBGPU);
    let software = std::env::var("EVORCH_MEASURE_ADAPTER").as_deref() == Ok("cpu");
    setup.native_adapter_selector = Some(Arc::new(move |adapters, _| {
        let rank = |a: &wgpu::Adapter| match (a.get_info().device_type, software) {
            (wgpu::DeviceType::Cpu, true) => 0,
            (wgpu::DeviceType::DiscreteGpu, false) => 0,
            (wgpu::DeviceType::IntegratedGpu, false) => 1,
            (wgpu::DeviceType::Cpu, false) => 3,
            _ => 2,
        };
        adapters
            .iter()
            .min_by_key(|a| rank(a))
            .cloned()
            .ok_or_else(|| "no adapter".to_owned())
    }));
    let state = egui_kittest::wgpu::create_render_state(
        egui_wgpu::WgpuSetup::CreateNew(setup),
        egui_wgpu::RendererOptions::PREDICTABLE,
    );
    let info = state.adapter.get_info();
    Arc::new(Gpu { state, info })
}

#[derive(Default)]
struct Log {
    uploads: Vec<Upload>,
    paints: Vec<Paint>,
    /// Bytes of the page's textures that are alive, per window.
    texture_bytes: HashMap<usize, u64>,
}
struct Upload {
    window: usize,
    done: u32,
    stamp: Option<u32>,
    size: [usize; 2],
    micros: u64,
}
struct Paint {
    window: usize,
    done: u32,
    stamp: Option<u32>,
    micros: u64,
}

/// A renderer that uploads and paints for real and writes down when it finished.
struct Measured {
    window: usize,
    gpu: Arc<Gpu>,
    renderer: egui_wgpu::Renderer,
    log: Arc<Mutex<Log>>,
    live: HashMap<egui::TextureId, u64>,
    last_stamp: Option<u32>,
}

impl Measured {
    fn new(window: usize, gpu: &Arc<Gpu>, log: &Arc<Mutex<Log>>) -> Self {
        Self {
            window,
            gpu: gpu.clone(),
            renderer: egui_wgpu::Renderer::new(
                &gpu.state.device,
                gpu.state.target_format,
                egui_wgpu::RendererOptions::PREDICTABLE,
            ),
            log: log.clone(),
            live: HashMap::new(),
            last_stamp: None,
        }
    }
    fn idle(&self) {
        self.gpu
            .state
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(20)),
            })
            .expect("the device went idle");
    }
}

impl TestRenderer for Measured {
    fn handle_delta(&mut self, delta: &mut egui::TexturesDelta) {
        let started = Instant::now();
        let mut pictures = Vec::new();
        #[expect(clippy::iter_over_hash_type)]
        for (id, images) in delta.set.drain() {
            for image in images {
                if image.pos.is_none() {
                    let egui::ImageData::Color(color) = &image.image;
                    let [w, h] = color.size;
                    self.live.insert(id, (w * h * 4) as u64);
                    // The page's pictures; the font atlas and widgets are smaller.
                    if w >= 256 && h >= 144 {
                        let at = |i: usize| color.pixels[i].r();
                        pictures.push((stamp(at, w, h), [w, h]));
                    }
                }
                self.renderer.update_texture(
                    &self.gpu.state.device,
                    &self.gpu.state.queue,
                    id,
                    &image,
                );
            }
        }
        #[expect(clippy::iter_over_hash_type)]
        for id in delta.free.drain() {
            self.renderer.free_texture(&id);
            self.live.remove(&id);
        }
        if pictures.is_empty() {
            return;
        }
        // The staged writes go out with an empty submission; the upload has finished when
        // the device is idle again.
        self.gpu.state.queue.submit(std::iter::empty());
        self.idle();
        let done = now_ticks();
        let micros = started.elapsed().as_micros() as u64;
        let mut log = self.log.lock().unwrap();
        let bytes = self.live.values().filter(|b| **b >= 256 * 144 * 4).sum();
        log.texture_bytes.insert(self.window, bytes);
        for (stamp, size) in pictures {
            self.last_stamp = stamp;
            log.uploads.push(Upload {
                window: self.window,
                done,
                stamp,
                size,
                micros,
            });
        }
    }

    fn render(
        &mut self,
        ctx: &egui::Context,
        output: &egui::FullOutput,
    ) -> Result<image::RgbaImage, String> {
        let started = Instant::now();
        let (device, queue) = (&self.gpu.state.device, &self.gpu.state.queue);
        let size = ctx.content_rect().size() * ctx.pixels_per_point();
        let screen = egui_wgpu::ScreenDescriptor {
            pixels_per_point: ctx.pixels_per_point(),
            size_in_pixels: [size.x.round() as u32, size.y.round() as u32],
        };
        let jobs = ctx.tessellate(output.shapes.clone(), ctx.pixels_per_point());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("measure"),
        });
        let user = self
            .renderer
            .update_buffers(device, queue, &mut encoder, &jobs, &screen);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("measure target"),
            size: wgpu::Extent3d {
                width: screen.size_in_pixels[0],
                height: screen.size_in_pixels[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.gpu.state.target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("measure pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    ..Default::default()
                })
                .forget_lifetime();
            self.renderer.render(&mut pass, &jobs, &screen);
        }
        queue.submit(user.into_iter().chain(std::iter::once(encoder.finish())));
        self.idle();
        self.log.lock().unwrap().paints.push(Paint {
            window: self.window,
            done: now_ticks(),
            stamp: self.last_stamp,
            micros: started.elapsed().as_micros() as u64,
        });
        Ok(image::RgbaImage::new(1, 1))
    }
}

// ---- the rig ---------------------------------------------------------------------------

struct Rig {
    env: Env,
    wins: Vec<Window>,
    log: Arc<Mutex<Log>>,
    gpu: Arc<Gpu>,
    tick: Duration,
    next: Instant,
    /// A client of its own, to read how many tabs the helper holds.
    watcher: data_peer::Peer,
}

fn html(n: usize) -> Vec<u8> {
    support::STAMP_PAGE
        .replace("</body>", &format!("<!-- page {n} --></body>"))
        .into_bytes()
}

impl Rig {
    async fn new(
        gpu: Arc<Gpu>,
        windows: usize,
        tabs: usize,
        size: egui::Vec2,
        tick: Duration,
    ) -> Self {
        support::use_real_helper();
        let titles: Vec<String> = (0..tabs).map(|n| format!("p{n}.html")).collect();
        let pages: Vec<(&str, Vec<u8>, &str)> = titles
            .iter()
            .enumerate()
            .map(|(n, t)| (t.as_str(), html(n), "text/html"))
            .collect();
        let env = support::start(&pages).await;
        let log = Arc::new(Mutex::new(Log::default()));
        let watcher = data_peer::Peer::connect(&env.socket, "watcher").await;
        let mut rig = Self {
            env,
            wins: Vec::new(),
            log,
            gpu: gpu.clone(),
            tick,
            next: Instant::now(),
            watcher,
        };
        for index in 0..windows {
            let window = open_window(&rig.env, size, Some(Measured::new(index, &gpu, &rig.log)));
            // The device's own limit, as the real window's painter reports it.
            let mut window = window;
            window.harness.input_mut().max_texture_side =
                Some(gpu.state.device.limits().max_texture_dimension_2d as usize);
            rig.wins.push(window);
            // One preview at a time, the next once the one before shows frames: opening
            // them all at once is refused as Busy by the core's private resource budget.
            for n in 0..tabs {
                let artifact = rig.env.artifacts[n].0.clone();
                rig.wins[index]
                    .shared
                    .lock()
                    .unwrap()
                    .features
                    .open_preview(artifact);
                rig.wait_tabs(index * tabs + n + 1).await;
            }
            let shown = rig
                .until(Duration::from_secs(60), |r| r.uploads_of(index) > 0)
                .await;
            assert!(shown, "window {index} shows frames");
        }
        rig
    }

    /// One frame of every window; a window that uploaded a picture also paints.
    fn frame(&mut self) {
        for index in 0..self.wins.len() {
            let before = self.log.lock().unwrap().uploads.len();
            self.wins[index].harness.step();
            if self.log.lock().unwrap().uploads.len() > before {
                let _ = self.wins[index].harness.render();
            }
        }
    }

    /// Frames at the rig's rate until `done`, or `limit` has passed.
    async fn until(&mut self, limit: Duration, mut done: impl FnMut(&Rig) -> bool) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            self.frame();
            if done(self) {
                return true;
            }
            self.next += self.tick;
            let now = Instant::now();
            if self.next > now {
                tokio::time::sleep(self.next - now).await;
            } else {
                self.next = now;
            }
        }
        false
    }

    async fn run(&mut self, duration: Duration) {
        self.until(duration, |_| false).await;
    }

    /// Frames until the helper holds `expected` tabs.
    async fn wait_tabs(&mut self, expected: usize) {
        let started = Instant::now();
        loop {
            self.run(self.tick).await;
            let used = match self
                .watcher
                .client
                .query(workbench_protocol::queries::Query::Browser(
                    BrowserQuery::GetRendererStatus {},
                ))
                .await
                .unwrap()
            {
                workbench_protocol::queries::QueryReply::Browser(BrowserReply::Status(s)) => {
                    s.tabs_used as usize
                }
                other => panic!("unexpected reply: {other:?}"),
            };
            if used >= expected {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "{used} of {expected} tabs"
            );
        }
    }

    fn uploads_of(&self, window: usize) -> usize {
        self.log
            .lock()
            .unwrap()
            .uploads
            .iter()
            .filter(|u| u.window == window)
            .count()
    }

    fn stats(&self, window: usize) -> gui::core_client::features::data::DataStats {
        self.wins[window]
            .shared
            .lock()
            .unwrap()
            .features
            .data_stats()
    }

    fn click(&mut self, window: usize, label: &str) {
        self.wins[window].harness.get_by_label(label).click();
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}
fn quantiles(mut values: Vec<f64>) -> Value {
    values.sort_by(f64::total_cmp);
    json!({
        "n": values.len(),
        "p50": percentile(&values, 0.5),
        "p95": percentile(&values, 0.95),
        "p99": percentile(&values, 0.99),
        "max": values.last().copied().unwrap_or(f64::NAN),
    })
}

fn emit(value: &Value) {
    let line = value.to_string();
    println!("{line}");
    if let Some(path) = std::env::var_os("EVORCH_MEASURE_OUT") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }
}

fn secs(name: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}

/// A snapshot of what the processes hold.
fn sample(rig: &Rig) -> Value {
    let tree = descendants();
    let sum = |key: &str| -> u64 {
        tree.iter()
            .map(|pid| field_kib(&format!("/proc/{pid}/smaps_rollup"), key))
            .sum()
    };
    let rss_tree: u64 = tree
        .iter()
        .map(|pid| field_kib(&format!("/proc/{pid}/status"), "VmRSS:"))
        .sum();
    let texture_bytes: u64 = rig.log.lock().unwrap().texture_bytes.values().sum();
    let held: u64 = (0..rig.wins.len()).map(|w| rig.stats(w).bytes_held).sum();
    json!({
        "rss_self_kib": field_kib("/proc/self/status", "VmRSS:"),
        "pss_self_kib": field_kib("/proc/self/smaps_rollup", "Pss:"),
        "rss_helper_tree_kib": rss_tree,
        "pss_helper_tree_kib": sum("Pss:"),
        "heap_live_bytes": counting::LIVE.load(std::sync::atomic::Ordering::Relaxed),
        "fds": std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0),
        "threads": field_kib("/proc/self/status", "Threads:"),
        "processes_in_helper_tree": tree.len(),
        "texture_bytes_gpu": texture_bytes,
        "frame_bytes_held_cpu": held,
    })
}

struct Config {
    name: &'static str,
    windows: usize,
    tabs: usize,
    size: (f32, f32),
    tick_ms: u64,
    /// Peers that read every frame / read one frame each 250 ms, each with a tab of its own.
    fast_peers: usize,
    slow_peers: usize,
    /// Select the next tab of window 0 this often.
    switch: Option<Duration>,
}

async fn background_peer(env: &Env, name: String, slow: bool) {
    let mut peer = data_peer::Peer::connect(&env.socket, &name).await;
    let session = peer.session().await;
    let tab = peer.open(&env.artifacts[0].0, &session).await;
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
    let mut stream = peer
        .channel()
        .await
        .bind_frames(&peer.client, subscription, token)
        .await
        .unwrap();
    peer.browser(BrowserCommand::SetRendererStreaming {
        tab: tab.reference.clone(),
        enabled: true,
    })
    .await
    .unwrap();
    tokio::spawn(async move {
        let _peer = peer;
        let mut buffer = Vec::new();
        while let Ok(Some(_)) = stream.next_frame_into(&mut buffer).await {
            if slow {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
}

async fn steady(gpu: Arc<Gpu>, mut cfg: Config) {
    if let Some(tabs) = std::env::var("EVORCH_MEASURE_TABS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        cfg.tabs = tabs;
    }
    let window_time = secs("EVORCH_MEASURE_SECS", 20);
    let mut rig = Rig::new(
        gpu,
        cfg.windows,
        cfg.tabs,
        egui::vec2(cfg.size.0, cfg.size.1),
        Duration::from_millis(cfg.tick_ms),
    )
    .await;
    for n in 0..cfg.fast_peers {
        background_peer(&rig.env, format!("fast-{n}"), false).await;
    }
    for n in 0..cfg.slow_peers {
        background_peer(&rig.env, format!("slow-{n}"), true).await;
    }
    let warm = rig
        .until(Duration::from_secs(90), |r| {
            (0..r.wins.len()).all(|w| r.uploads_of(w) >= 30)
        })
        .await;
    if !warm {
        for w in 0..rig.wins.len() {
            let shared = rig.wins[w].shared.lock().unwrap();
            eprintln!(
                "window {w}: uploads {} stats {:?} status {:?} connected {}",
                rig.uploads_of(w),
                shared.features.data_stats(),
                shared.status,
                shared.connected
            );
        }
    }
    if !warm {
        for n in rig.wins[0]
            .harness
            .get_all_by_role(egui::accesskit::Role::Label)
        {
            let text = format!("{n:?}");
            if let Some(at) = text.find("value: \"") {
                let value: String = text[at + 8..].chars().take_while(|c| *c != '"').collect();
                if [
                    "開",
                    "準備",
                    "接続",
                    "失敗",
                    "待",
                    "Lost",
                    "分かり",
                    "フレーム",
                ]
                .iter()
                .any(|k| value.contains(k))
                {
                    eprintln!("LABEL {value}");
                }
            }
        }
    }
    assert!(warm, "every window shows frames");
    let mut next_switch = cfg.switch.map(|every| Instant::now() + every);
    let mut selected = 0usize;
    let (t0, started) = (now_ticks(), Instant::now());
    let cpu0 = cpu_now();
    let stats0: Vec<_> = (0..rig.wins.len()).map(|w| rig.stats(w)).collect();
    let uploads0: Vec<_> = (0..rig.wins.len()).map(|w| rig.uploads_of(w)).collect();
    let mut peaks = sample(&rig);
    let mut last_sample = Instant::now();
    while started.elapsed() < window_time {
        rig.run(Duration::from_millis(250)).await;
        if let Some(at) = next_switch
            && Instant::now() >= at
        {
            selected = (selected + 1) % cfg.tabs;
            let id = rig.env.artifacts[selected].0.artifact_id.clone();
            rig.click(0, &id);
            next_switch = cfg.switch.map(|every| Instant::now() + every);
        }
        if last_sample.elapsed() >= Duration::from_secs(2) {
            last_sample = Instant::now();
            let now = sample(&rig);
            for key in [
                "pss_self_kib",
                "pss_helper_tree_kib",
                "rss_self_kib",
                "rss_helper_tree_kib",
            ] {
                if now[key].as_u64() > peaks[key].as_u64() {
                    peaks[key] = now[key].clone();
                }
            }
        }
    }
    let (t1, elapsed) = (now_ticks(), started.elapsed().as_secs_f64());
    let cpu1 = cpu_now();
    let end = sample(&rig);
    let log = rig.log.lock().unwrap();
    let mut windows = Vec::new();
    for w in 0..cfg.windows {
        let inside = |tick: u32| tick.wrapping_sub(t0) < t1.wrapping_sub(t0);
        let ups: Vec<&Upload> = log
            .uploads
            .iter()
            .filter(|u| u.window == w && inside(u.done))
            .collect();
        let up_age: Vec<f64> = ups
            .iter()
            .filter_map(|u| u.stamp.and_then(|s| age_ms(s, u.done)))
            .collect();
        let paints: Vec<&Paint> = log
            .paints
            .iter()
            .filter(|p| p.window == w && inside(p.done))
            .collect();
        let paint_age: Vec<f64> = paints
            .iter()
            .filter_map(|p| p.stamp.and_then(|s| age_ms(s, p.done)))
            .collect();
        let after = rig.stats(w);
        let (received, replaced, taken) = (
            after.frames_received - stats0[w].frames_received,
            after.frames_replaced - stats0[w].frames_replaced,
            after.frames_taken - stats0[w].frames_taken,
        );
        let sizes: std::collections::BTreeSet<[usize; 2]> = ups.iter().map(|u| u.size).collect();
        windows.push(json!({
            "window": w,
            "texture_sizes": sizes,
            "uploads": ups.len(),
            "uploads_before_window": uploads0[w],
            "upload_fps": ups.len() as f64 / elapsed,
            "received_fps": received as f64 / elapsed,
            "taken_fps": taken as f64 / elapsed,
            "skipped_replaced_in_slot": replaced,
            "skipped_pct": if received > 0 { 100.0 * replaced as f64 / received as f64 } else { 0.0 },
            "stamp_to_upload_done_ms": quantiles(up_age),
            "upload_call_ms": quantiles(ups.iter().map(|u| u.micros as f64 / 1000.0).collect()),
            "stamp_to_paint_done_offscreen_ms": quantiles(paint_age),
            "paint_pass_ms": quantiles(paints.iter().map(|p| p.micros as f64 / 1000.0).collect()),
        }));
    }
    drop(log);
    emit(&json!({
        "scenario": cfg.name,
        "adapter": { "name": rig.gpu.info.name, "type": format!("{:?}", rig.gpu.info.device_type), "backend": format!("{:?}", rig.gpu.info.backend) },
        "tick_ms": cfg.tick_ms,
        "windows": cfg.windows, "tabs_per_window": cfg.tabs, "window_points": [cfg.size.0, cfg.size.1],
        "background_fast_peers": cfg.fast_peers, "background_slow_peers": cfg.slow_peers,
        "seconds": elapsed,
        "cpu_cores_process": (cpu1.process - cpu0.process) / elapsed,
        "cpu_cores_gui_thread": (cpu1.main_thread - cpu0.main_thread) / elapsed,
        "cpu_cores_helper_tree": (cpu1.tree - cpu0.tree) / elapsed,
        "peak": peaks,
        "end": end,
        "per_window": windows,
    }));
}

macro_rules! steady_test {
    ($name:ident, $cfg:expr) => {
        #[test]
        #[ignore = "measurement; needs real CEF and a GPU adapter"]
        fn $name() {
            let gpu = gpu();
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(steady(gpu, $cfg));
        }
    };
}

/// The page area is the window's width less the page margin and its scroll bar; a window of
/// this width gives a 1280-point area (a 720p frame at 16:9) and of that one a 3840-point
/// area (a 4K frame).
const W720: f32 = 1280.0 + 34.0;
const W4K: f32 = 3840.0 + 34.0;

steady_test!(
    s720_one_tab,
    Config {
        name: "720p-1window-1tab",
        windows: 1,
        tabs: 1,
        size: (W720, 1500.0),
        tick_ms: 16,
        fast_peers: 0,
        slow_peers: 0,
        switch: None
    }
);
steady_test!(
    s720_four_windows,
    Config {
        name: "720p-4windows-1tab-each",
        windows: 4,
        tabs: 1,
        size: (W720, 1500.0),
        tick_ms: 16,
        fast_peers: 0,
        slow_peers: 0,
        switch: None
    }
);
steady_test!(
    s720_four_tabs_switching,
    Config {
        name: "720p-1window-4tabs-switching-every-3s",
        windows: 1,
        tabs: 4,
        size: (W720, 1500.0),
        tick_ms: 16,
        fast_peers: 0,
        slow_peers: 0,
        switch: Some(Duration::from_secs(3))
    }
);
steady_test!(
    s4k_one_tab,
    Config {
        name: "4k-1window-1tab",
        windows: 1,
        tabs: 1,
        size: (W4K, 2400.0),
        tick_ms: 16,
        fast_peers: 0,
        slow_peers: 0,
        switch: None
    }
);
steady_test!(
    s720_subscribers_and_slow_consumer,
    Config {
        name: "720p-1window+2fast-1slow-peers",
        windows: 1,
        tabs: 1,
        size: (W720, 1500.0),
        tick_ms: 16,
        fast_peers: 2,
        slow_peers: 1,
        switch: None
    }
);
steady_test!(
    s720_slow_gui,
    Config {
        name: "720p-1window-gui-at-10fps",
        windows: 1,
        tabs: 1,
        size: (W720, 1500.0),
        tick_ms: 100,
        fast_peers: 0,
        slow_peers: 0,
        switch: None
    }
);

// ---- resize, hide, close, open again -------------------------------------------------------

/// Time (ms) from `action` until a picture of a size that satisfies `want` was uploaded.
async fn after(
    rig: &mut Rig,
    what: &str,
    action: impl FnOnce(&mut Rig),
    want: impl Fn(&Upload) -> bool,
) -> f64 {
    let seen = rig.log.lock().unwrap().uploads.len();
    let started = Instant::now();
    action(rig);
    let ok = rig
        .until(Duration::from_secs(30), |r| {
            r.log.lock().unwrap().uploads[seen..].iter().any(&want)
        })
        .await;
    if !ok {
        for n in rig.wins[0]
            .harness
            .get_all_by_role(egui::accesskit::Role::Label)
        {
            let text = format!("{n:?}");
            if let Some(at) = text.find("value: \"") {
                let value: String = text[at + 8..].chars().take_while(|c| *c != '"').collect();
                if [
                    "開",
                    "準備",
                    "接続",
                    "失敗",
                    "待",
                    "Lost",
                    "分かり",
                    "フレーム",
                    "閉",
                    "表示",
                ]
                .iter()
                .any(|k| value.contains(k))
                {
                    eprintln!("LABEL {value}");
                }
            }
        }
    }
    assert!(ok, "no picture after: {what}");
    started.elapsed().as_secs_f64() * 1000.0
}

/// One round of operations on window 0, with their latencies.
async fn operations(rig: &mut Rig, close: bool) -> Value {
    let artifact = rig.env.artifacts[0].0.clone();
    let size = rig.wins[0].harness.ctx.content_rect().size();
    let current = rig.log.lock().unwrap().uploads.last().unwrap().size;
    let resize_down = after(
        rig,
        "resize narrower",
        |r| {
            r.wins[0]
                .harness
                .set_size(egui::vec2(size.x - 300.0, size.y));
        },
        |u| u.size[0] != current[0],
    )
    .await;
    let new = rig.log.lock().unwrap().uploads.last().unwrap().size;
    let resize_up = after(
        rig,
        "resize wider",
        |r| {
            r.wins[0].harness.set_size(size);
        },
        |u| u.size[0] != new[0],
    )
    .await;
    // Hide: the section is collapsed; the stream must stop, and start again when shown.
    rig.click(0, "Browser");
    rig.run(Duration::from_millis(1500)).await;
    let before = rig.stats(0).frames_received;
    rig.run(Duration::from_secs(1)).await;
    let received_while_hidden = rig.stats(0).frames_received - before;
    let show = after(rig, "show again", |r| r.click(0, "Browser"), |_| true).await;
    if !close {
        return json!({
            "resize_narrower_to_first_new_size_ms": resize_down,
            "resize_wider_to_first_new_size_ms": resize_up,
            "frames_received_during_1s_while_hidden": received_while_hidden,
            "hide_then_show_to_first_picture_ms": show,
        });
    }
    // Close the preview, and open the same artifact again. A closed tab's allocation
    // stays in the core's budget until the helper generation ends, so a soak closes rarely.
    rig.click(0, "×");
    let closed = rig
        .until(Duration::from_secs(30), |r| {
            r.wins[0]
                .harness
                .query_by_label_contains("アーティファクトのプレビューを開くと")
                .is_some()
        })
        .await;
    rig.run(Duration::from_millis(500)).await;
    let reopen = after(
        rig,
        "close then open again",
        |r| {
            r.wins[0]
                .shared
                .lock()
                .unwrap()
                .features
                .open_preview(artifact)
        },
        |_| true,
    )
    .await;
    json!({
        "resize_narrower_to_first_new_size_ms": resize_down,
        "resize_wider_to_first_new_size_ms": resize_up,
        "frames_received_during_1s_while_hidden": received_while_hidden,
        "hide_then_show_to_first_picture_ms": show,
        "close_seen": closed,
        "close_then_open_again_to_first_picture_ms": reopen,
        "run_tabs_untouched": rig.env.run_tabs(),
    })
}

#[test]
#[ignore = "measurement; needs real CEF and a GPU adapter"]
fn lifecycle_resize_hide_close_open_again() {
    let gpu = gpu();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let mut rig = Rig::new(
                gpu,
                1,
                1,
                egui::vec2(W720, 1500.0),
                Duration::from_millis(16),
            )
            .await;
            assert!(
                rig.until(Duration::from_secs(90), |r| r.uploads_of(0) >= 30)
                    .await
            );
            // A run's own tab beside the client's: the operations must not touch it.
            let page = html(0);
            rig.env.run_opens(0, &page).await;
            let mut rounds = Vec::new();
            for round in 0..5 {
                let result = operations(&mut rig, true).await;
                emit(&json!({ "scenario": "lifecycle-round", "round": round, "result": result }));
                rounds.push(result);
                // The core keeps every operation's result for 60 s and holds 64 at once, so a
                // round of operations is followed by a pause (see the budget test below).
                rig.run(Duration::from_secs(25)).await;
            }
            emit(&json!({ "scenario": "lifecycle", "rounds": rounds, "end": sample(&rig) }));
        });
}

// ---- the operation budget -----------------------------------------------------------------

/// The core holds the result of every renderer operation for 60 s and at most 64 at once
/// (`MAX_RENDERER_OPERATIONS`, `RESULT_TTL`): a client that sends more than that within a
/// minute is refused as Busy until the oldest results expire. This sends resizes as fast as
/// the page allows (one per 100 ms) until the page reports the refusal, counts them, waits
/// the TTL out and uses the page's explicit retry.
#[test]
#[ignore = "measurement; needs real CEF and a GPU adapter"]
fn operation_budget_is_exhausted_by_resizes_and_recovers_after_the_ttl() {
    let gpu = gpu();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let mut rig = Rig::new(
                gpu,
                1,
                1,
                egui::vec2(W720, 1500.0),
                Duration::from_millis(16),
            )
            .await;
            assert!(
                rig.until(Duration::from_secs(90), |r| r.uploads_of(0) >= 30)
                    .await
            );
            let started = Instant::now();
            let mut resizes = 0u32;
            let mut wide = false;
            let refused = loop {
                wide = !wide;
                let width = if wide { W720 } else { W720 - 200.0 };
                rig.wins[0].harness.set_size(egui::vec2(width, 1500.0));
                rig.run(Duration::from_millis(150)).await;
                resizes += 1;
                let refused = rig.wins[0]
                    .harness
                    .query_by_label_contains("サイズ変更に失敗しました")
                    .is_some();
                if refused || resizes >= 400 {
                    break refused;
                }
            };
            let seconds_to_refusal = started.elapsed().as_secs_f64();
            // The same request after the TTL: the explicit retry sends it again.
            let mut status = Vec::new();
            for n in rig.wins[0]
                .harness
                .get_all_by_role(egui::accesskit::Role::Label)
            {
                let text = format!("{n:?}");
                if let Some(at) = text.find("value: \"") {
                    let value: String = text[at + 8..].chars().take_while(|c| *c != '"').collect();
                    if value.contains("失敗") || value.contains("Busy") {
                        status.push(value);
                    }
                }
            }
            rig.run(Duration::from_secs(62)).await;
            let retry_button = rig.wins[0]
                .harness
                .query_by_label("サイズ変更を再試行")
                .is_some();
            if retry_button {
                rig.click(0, "サイズ変更を再試行");
            }
            let seen = rig.log.lock().unwrap().uploads.len();
            let size = rig.log.lock().unwrap().uploads.last().unwrap().size;
            rig.wins[0]
                .harness
                .set_size(egui::vec2(W720 - 100.0, 1500.0));
            let recovered = rig
                .until(Duration::from_secs(30), |r| {
                    r.log.lock().unwrap().uploads[seen..]
                        .iter()
                        .any(|u| u.size[0] != size[0])
                })
                .await;
            emit(&json!({
                "scenario": "operation-budget",
                "resizes_sent_until_refused": resizes,
                "refused": refused,
                "seconds_to_refusal": seconds_to_refusal,
                "status_lines_when_refused": status,
                "retry_button_shown": retry_button,
                "recovered_after_ttl_with_retry": recovered,
            }));
        });
}

// ---- soak ---------------------------------------------------------------------------------

#[test]
#[ignore = "measurement; needs real CEF and a GPU adapter"]
fn soak_720p_with_periodic_operations() {
    let gpu = gpu();
    let total = secs("EVORCH_SOAK_SECS", 1800);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let mut rig = Rig::new(gpu, 1, 1, egui::vec2(W720, 1500.0), Duration::from_millis(16)).await;
            background_peer(&rig.env, "soak-slow".into(), true).await;
            assert!(rig.until(Duration::from_secs(90), |r| r.uploads_of(0) >= 30).await);
            let page = html(0);
            rig.env.run_opens(0, &page).await;
            let started = Instant::now();
            let mut next_operation = Instant::now() + Duration::from_secs(60);
            let mut next_sample = Instant::now();
            let mut samples = Vec::new();
            let mut rounds = 0u32;
            while started.elapsed() < total {
                rig.run(Duration::from_millis(500)).await;
                if Instant::now() >= next_sample {
                    next_sample = Instant::now() + Duration::from_secs(30);
                    let mut s = sample(&rig);
                    s["t_s"] = json!(started.elapsed().as_secs());
                    s["uploads"] = json!(rig.uploads_of(0));
                    s["operation_rounds"] = json!(rounds);
                    emit(&json!({ "scenario": "soak-sample", "sample": s }));
                    samples.push(s);
                }
                if Instant::now() >= next_operation {
                    let round = operations(&mut rig, rounds % 5 == 4).await;
                    rounds += 1;
                    assert_eq!(rig.env.run_tabs(), 1, "the run's tab survives: {round}");
                    next_operation = Instant::now() + Duration::from_secs(60);
                }
            }
            emit(&json!({ "scenario": "soak", "seconds": total.as_secs(), "operation_rounds": rounds, "samples": samples.len() }));
        });
}
