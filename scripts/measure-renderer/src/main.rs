//! Measurement tool for the out-of-process renderer helper (ADR 0045, 16a).
//!
//! Usage: `measure-renderer <evorch-renderer> <work-dir> <scenario> [key=value ...]`
//!
//! Scenarios (each prints one JSON object per run on stdout):
//!   startup  runs=N kill=0|1       spawn / Hello / open / first-frame latency, stop time
//!   stream   fps= secs= tabs= w= h= warm=   real helper: delivery, CPU, memory, stop
//!   tabs     w= h=                 memory and process count while opening 1..=4 tabs, then a 5th
//!   ipc      fps= secs= w= h= tabs= hold_ms= warm=   synthetic producer: IPC only
//!
//! Waiting is by condition (`wait_frame`, `shutdown`, `waitid`); the only
//! timed parking is the synthetic producer's frame pacing and the consumer
//! hold of `hold_ms`, which are load shapes, not synchronization.

mod fixture;
mod helper;
mod procfs;
mod proxy;
mod stats;
mod synthetic;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use renderer_ipc::{
    ClientConfig, ClientError, ErrorKind, OpenArtifact, RendererClient, RendererGeneration,
    SessionId, TabId, Viewport,
};
use serde_json::{Value, json};

use helper::{Config, Launched};
use procfs::Snapshot;

struct Args(BTreeMap<String, String>);

impl Args {
    fn get<T: std::str::FromStr>(&self, key: &str, default: T) -> T {
        self.0
            .get(key)
            .map(|v| {
                v.parse()
                    .unwrap_or_else(|_| fail(&format!("bad {key}={v}")))
            })
            .unwrap_or(default)
    }
}

fn fail(message: &str) -> ! {
    eprintln!("measure-renderer: {message}");
    std::process::exit(2);
}

fn emit(value: Value) {
    println!("{value}");
}

fn open_request(tab: u64, w: u32, h: u32, fps: u8) -> (OpenArtifact, Vec<u8>) {
    OpenArtifact::with_files(
        SessionId(1),
        TabId(tab),
        Viewport {
            width: w,
            height: h,
        },
        fps,
        60_000,
        vec![(
            "index.html".into(),
            "text/html".into(),
            fixture::HTML.as_bytes().to_vec(),
        )],
    )
}

fn open_tab(l: &Launched, tab: u64, w: u32, h: u32, fps: u8) -> Result<f64, ClientError> {
    let (open, body) = open_request(tab, w, h, fps);
    let started = Instant::now();
    l.client.open_artifact(open, &body)?;
    Ok(helper::ms(started.elapsed()))
}

fn stop_json(timings: &helper::StopTimings) -> Value {
    json!({
        "close": timings.close,
        "exit_status": timings.exit_status,
        "shutdown_returned_ms": timings.shutdown_returned_ms,
        "exit_ms": timings.exit_ms,
        "reaped_ms": timings.reaped_ms,
        "reaped_orphans": timings.reaped_orphans,
        "leftover_processes": timings.leftover,
    })
}

fn environment() -> Value {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let cpuinfo = read("/proc/cpuinfo");
    let model = cpuinfo
        .lines()
        .find_map(|l| l.strip_prefix("model name"))
        .map(|l| l.trim_start_matches([' ', '\t', ':']).to_string())
        .unwrap_or_default();
    let logical = cpuinfo
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count();
    let mem_kib = read("/proc/meminfo")
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|l| {
            l.split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(0);
    let load = read("/proc/loadavg");
    json!({
        "kernel": read("/proc/sys/kernel/osrelease").trim(),
        "cpu": model,
        "logical_cpus": logical,
        "mem_total_mib": mem_kib / 1024,
        "loadavg_at_start": load.trim(),
        "profile_debug_assertions": cfg!(debug_assertions),
    })
}

// ---------------------------------------------------------------- startup

/// Timing runs take no measurement between spawn and the first frame: one
/// `Instant` started before `spawn` covers the whole span, and the stage times are
/// read from it. Memory is read after the first frame (`after_first_frame`).
/// The `mem=1` runs read memory at each stage instead, so their latencies are
/// inflated by the readings and are labelled as such.
fn startup(config: &Config, args: &Args) {
    let runs: u32 = args.get("runs", 5);
    let kill: u32 = args.get("kill", 0);
    let mem: u32 = args.get("mem", 0);
    for run in 1..=runs {
        let label = format!("startup{run}");
        let begun = Instant::now();
        let mut l = match helper::launch(config, &label, 1) {
            Ok(l) => l,
            Err(error) => fail(&error),
        };
        let hello_at = begun.elapsed();
        let after_hello = (mem == 1).then(|| l.helper.snapshot().to_json());
        let (open, body) = open_request(1, 1280, 720, 30);
        l.client
            .open_artifact(open, &body)
            .unwrap_or_else(|e| fail(&e.to_string()));
        let opened_at = begun.elapsed();
        let after_open = (mem == 1).then(|| l.helper.snapshot().to_json());
        let streaming_at = begun.elapsed();
        l.client.set_streaming(TabId(1), true).unwrap();
        let first = l.client.wait_frame(Some(Duration::from_secs(30)));
        let first_frame_at = first.as_ref().map(|_| begun.elapsed());
        let after_first_frame = l.helper.snapshot().to_json();
        let stop = if kill == 1 {
            helper::stop_forced(&mut l)
        } else {
            helper::stop_orderly(&mut l)
        };
        emit(json!({
            "scenario": "startup",
            "run": run,
            "mode": if mem == 1 { "memory (latencies inflated by readings)" } else { "timing" },
            "stop_mode": if kill == 1 { "kill" } else { "shutdown" },
            "spawn_call_ms": l.spawn_ms,
            "hello_ms": helper::ms(hello_at),
            "open_artifact_done_ms": helper::ms(opened_at),
            "open_artifact_ms": helper::ms(opened_at - hello_at),
            "streaming_requested_at_ms": helper::ms(streaming_at),
            "first_frame_after_streaming_ms": first_frame_at.map(|f| helper::ms(f - streaming_at)),
            "spawn_to_first_frame_ms": first_frame_at.map(helper::ms),
            "after_hello": after_hello,
            "after_open": after_open,
            "after_first_frame": after_first_frame,
            "stop": stop_json(&stop),
        }));
    }
}

// ----------------------------------------------------------------- stream

#[derive(Default)]
struct Delivery {
    per_tab: BTreeMap<u64, u64>,
    arrival_ms: Vec<f64>,
    age_ms: Vec<f64>,
    first_byte_to_client_ms: Vec<f64>,
    undecodable: u64,
    bytes: u64,
}

/// Reads memory of the helper tree every 2 s on its own thread, so the frame
/// consumer is never stopped by a `/proc` scan. Returns the peak-PSS snapshot.
fn sampler(
    pid: i32,
    initial: Snapshot,
) -> (
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<(Snapshot, u32)>,
) {
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let mut peak = initial;
        let mut samples = 0;
        // `recv_timeout` is the sampling interval and the stop signal in one.
        while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
            stopped.recv_timeout(Duration::from_secs(2))
        {
            let snapshot = procfs::snapshot(pid, pid);
            if snapshot.mem.pss_kib > peak.mem.pss_kib {
                peak = snapshot;
            }
            samples += 1;
        }
        (peak, samples)
    });
    (stop, handle)
}

/// `proxy`: route the frame channel through the pass-through proxy and report
/// the helper's own socket transfer (scenario `transfer`).
fn stream(config: &Config, args: &Args, proxy: bool) {
    let fps: u32 = args.get("fps", 30);
    let secs: u64 = args.get("secs", 20);
    let tabs: u64 = args.get("tabs", 1);
    let (w, h): (u32, u32) = (args.get("w", 1280), args.get("h", 720));
    let warm: u64 = args.get("warm", 10);
    let label = format!("stream{fps}x{tabs}-{w}x{h}");
    let mut l = match helper::launch_with(config, &label, 1, proxy) {
        Ok(l) => l,
        Err(error) => fail(&error),
    };
    let mut open = Vec::new();
    for tab in 1..=tabs {
        open.push(open_tab(&l, tab, w, h, fps as u8).unwrap_or_else(|e| fail(&e.to_string())));
    }
    let after_open = l.helper.snapshot();
    let (stop_sampler, sampler_thread) = sampler(l.helper.pid, after_open.clone());
    for tab in 1..=tabs {
        l.client.set_streaming(TabId(tab), true).unwrap();
    }
    let began = Instant::now();
    let mut delivery = Delivery::default();
    let mut first_frame_ms = None;
    let mut counted_from: Option<(Instant, Snapshot, u64, u64, renderer_ipc::ClientStats)> = None;
    let mut skipped = 0u64;
    let mut window = stats::Window::default();
    let mut last_arrival: Option<Instant> = None;
    let mut window_end: Option<Instant> = None;
    loop {
        if let Some(end) = window_end
            && Instant::now() >= end
        {
            break;
        }
        let Some(frame) = l.client.wait_frame(Some(Duration::from_secs(30))) else {
            fail("no frame within 30 s: the helper stopped delivering");
        };
        let arrived = Instant::now();
        let now_ticks = fixture::now_ticks();
        first_frame_ms.get_or_insert(helper::ms(began.elapsed()));
        let first_seen = l.proxy.as_ref().and_then(|p| {
            let mut seen = p.first_seen.lock().unwrap();
            let first = seen.remove(&frame.seq);
            // Frames replaced before the client took them leave entries behind.
            seen.retain(|seq, _| *seq > frame.seq);
            first
        });
        if skipped < warm {
            skipped += 1;
            l.client.recycle_frame(frame);
            continue;
        }
        if counted_from.is_none() {
            let (own_mem, own_ticks) = procfs::own();
            let snapshot = l.helper.snapshot();
            counted_from = Some((
                Instant::now(),
                snapshot,
                own_ticks,
                own_mem.rss_kib,
                l.client.stats(),
            ));
            window_end = Some(Instant::now() + Duration::from_secs(secs));
            last_arrival = Some(Instant::now());
            window.observe(frame.seq);
            l.client.recycle_frame(frame);
            continue;
        }
        window.observe(frame.seq);
        delivery.bytes += frame.bgra.len() as u64;
        *delivery.per_tab.entry(frame.tab.0).or_default() += 1;
        if let Some(last) = last_arrival {
            delivery
                .arrival_ms
                .push(arrived.duration_since(last).as_secs_f64() * 1000.0);
        }
        last_arrival = Some(arrived);
        if let Some(first) = first_seen {
            delivery
                .first_byte_to_client_ms
                .push(arrived.duration_since(first).as_secs_f64() * 1000.0);
        }
        match fixture::stamp(&frame.bgra, frame.width).and_then(|s| fixture::age_ms(s, now_ticks)) {
            Some(age) => delivery.age_ms.push(age),
            None => delivery.undecodable += 1,
        }
        l.client.recycle_frame(frame);
    }
    let (start, start_snapshot, own_ticks0, own_rss0, stats0) = counted_from.unwrap();
    let wall = start.elapsed().as_secs_f64();
    let end_snapshot = l.helper.snapshot();
    let (own_mem, own_ticks1) = procfs::own();
    let stats1 = l.client.stats();
    let _ = stop_sampler.send(());
    let (peak, samples) = sampler_thread.join().expect("sampler thread");
    let helper_cpu = procfs::cpu_percent(&start_snapshot.ticks, &end_snapshot.ticks, wall);
    let client_cpu = (own_ticks1 - own_ticks0) as f64 / procfs::CLK_TCK / wall * 100.0;
    // The helper's own transfer of each full frame, as seen on the proxy.
    let helper_side = l.proxy.as_ref().map(|p| {
        let samples = p.samples.lock().unwrap();
        let inside: Vec<_> = samples.iter().filter(|s| s.first >= start).collect();
        json!({
            "frames": inside.len(),
            "wire_ms": stats::dist(inside.iter().map(|s| s.wire_ms).collect()),
            "forward_to_client_ms": stats::dist(inside.iter().map(|s| s.forward_ms).collect()),
            "page_to_first_byte_ms": stats::dist(
                inside.iter().filter_map(|s| s.page_to_first_byte_ms).collect()),
        })
    });
    for tab in 1..=tabs {
        let _ = l.client.set_streaming(TabId(tab), false);
    }
    let mut closed = 0;
    for tab in 1..=tabs {
        if l.client.close_tab(TabId(tab)).is_ok() {
            closed += 1;
        }
    }
    let stop = helper::stop_orderly(&mut l);
    emit(json!({
        "scenario": if proxy { "transfer" } else { "stream" },
        "requested_fps": fps,
        "tabs": tabs,
        "viewport": format!("{w}x{h}"),
        "frame_bytes": u64::from(w) * u64::from(h) * 4,
        "window_secs": stats::round(wall),
        "spawn_to_hello_ms": l.hello_ms,
        "open_artifact_ms": open,
        "first_frame_after_streaming_ms": first_frame_ms,
        "delivered_frames": window.frames,
        "delivered_fps": stats::round(window.frames as f64 / wall),
        "delivered_mib_per_s": stats::round(delivery.bytes as f64 / wall / 1048576.0),
        "per_tab_frames": delivery.per_tab,
        "seq_gaps_lost_frames": window.gaps,
        "client_received_frames": stats1.received_frames - stats0.received_frames,
        "client_dropped_frames": stats1.dropped_frames - stats0.dropped_frames,
        "client_stale_frames": stats1.stale_frames - stats0.stale_frames,
        "client_dropped_events": stats1.dropped_events - stats0.dropped_events,
        "inter_arrival_ms": stats::dist(delivery.arrival_ms),
        "page_to_client_age_ms": stats::dist(delivery.age_ms),
        "helper_first_byte_to_client_ms": stats::dist(delivery.first_byte_to_client_ms),
        "proxy": helper_side,
        "undecodable_frames": delivery.undecodable,
        "cpu_percent_one_core": {
            "helper_tree": stats::round(helper_cpu),
            "client_process": stats::round(client_cpu),
        },
        "memory_start": start_snapshot.to_json(),
        "memory_peak_pss_sample": peak.to_json(),
        "memory_end": end_snapshot.to_json(),
        "memory_samples": samples,
        "client_rss_mib": { "start": procfs::mib(own_rss0), "end": procfs::mib(own_mem.rss_kib) },
        "tabs_closed_ok": closed,
        "stop": stop_json(&stop),
    }));
}

// ------------------------------------------------------------------- tabs

fn tabs(config: &Config, args: &Args) {
    let (w, h): (u32, u32) = (args.get("w", 1280), args.get("h", 720));
    let mut l = match helper::launch(config, "tabs", 1) {
        Ok(l) => l,
        Err(error) => fail(&error),
    };
    let mut steps = vec![json!({ "tabs": 0, "memory": l.helper.snapshot().to_json() })];
    for tab in 1..=4u64 {
        let open_ms = open_tab(&l, tab, w, h, 30).unwrap_or_else(|e| fail(&e.to_string()));
        steps.push(json!({
            "tabs": tab,
            "open_artifact_ms": open_ms,
            "memory": l.helper.snapshot().to_json(),
        }));
    }
    let fifth = match open_tab(&l, 5, w, h, 30) {
        Ok(_) => "accepted".to_string(),
        Err(ClientError::Remote(error)) if error.kind == ErrorKind::Busy => "Busy".into(),
        Err(error) => format!("{error}"),
    };
    for tab in 1..=4u64 {
        let _ = l.client.close_tab(TabId(tab));
    }
    let after_close = l.helper.snapshot().to_json();
    let stop = helper::stop_orderly(&mut l);
    emit(json!({
        "scenario": "tabs",
        "viewport": format!("{w}x{h}"),
        "steps": steps,
        "fifth_tab": fifth,
        "after_closing_all_tabs": after_close,
        "stop": stop_json(&stop),
    }));
}

// -------------------------------------------------------------------- ipc

fn ipc(args: &Args) {
    let fps: u32 = args.get("fps", 30);
    let secs: u64 = args.get("secs", 20);
    let tabs: u64 = args.get("tabs", 1);
    let (w, h): (u32, u32) = (args.get("w", 1280), args.get("h", 720));
    let hold_ms: u64 = args.get("hold_ms", 0);
    let warm: u64 = args.get("warm", 10);
    let (control, frames, server) = synthetic::start(fps);
    synthetic::base();
    let (client, _info) =
        RendererClient::connect(control, frames, ClientConfig::new(RendererGeneration(1)))
            .unwrap_or_else(|e| fail(&e.to_string()));
    for tab in 1..=tabs {
        let (open, body) = open_request(tab, w, h, 60);
        client.open_artifact(open, &body).unwrap();
        client.set_streaming(TabId(tab), true).unwrap();
    }
    let (rss0, ticks0) = {
        let (m, t) = procfs::own();
        (m.rss_kib, t)
    };
    let mut latency = Vec::new();
    let mut arrival = Vec::new();
    let mut bytes = 0u64;
    let mut window = stats::Window::default();
    let mut skipped = 0u64;
    let mut begin: Option<(Instant, renderer_ipc::ClientStats, u64)> = None;
    let mut end: Option<Instant> = None;
    let mut last: Option<Instant> = None;
    let mut peak_rss = rss0;
    let mut next_sample = Instant::now();
    loop {
        if let Some(end) = end
            && Instant::now() >= end
        {
            break;
        }
        let Some(frame) = client.wait_frame(Some(Duration::from_secs(30))) else {
            fail("no frame within 30 s");
        };
        let arrived = synthetic::stamp_nanos();
        let now = Instant::now();
        if skipped < warm {
            skipped += 1;
            client.recycle_frame(frame);
            continue;
        }
        if begin.is_none() {
            // The frame that fixes the start is the baseline, not a delivery of
            // the window: counting it would add one frame to `count`.
            begin = Some((now, client.stats(), procfs::own().1));
            end = Some(now + Duration::from_secs(secs));
            last = Some(now);
            window.observe(frame.seq);
            client.recycle_frame(frame);
            continue;
        }
        window.observe(frame.seq);
        bytes += frame.bgra.len() as u64;
        if let Some(sent) = synthetic::read_stamp(&frame.bgra) {
            latency.push(arrived.saturating_sub(sent) as f64 / 1e6);
        }
        arrival.push(now.duration_since(last.unwrap()).as_secs_f64() * 1000.0);
        last = Some(now);
        client.recycle_frame(frame);
        if hold_ms > 0 {
            // A slow consumer: it keeps the frame for a while.
            std::thread::park_timeout(Duration::from_millis(hold_ms));
        }
        if now >= next_sample {
            peak_rss = peak_rss.max(procfs::own().0.rss_kib);
            next_sample = now + Duration::from_secs(2);
        }
    }
    let (start, stats0, ticks_start) = begin.unwrap();
    let wall = start.elapsed().as_secs_f64();
    let stats1 = client.stats();
    let (own_mem, ticks1) = procfs::own();
    let close = client.shutdown(Duration::from_secs(30));
    let exit = server.join().map(|r| format!("{r:?}")).unwrap_or_default();
    let _ = ticks0;
    let (count, gaps) = (window.frames, window.gaps);
    emit(json!({
        "scenario": "ipc",
        "requested_fps": fps,
        "tabs": tabs,
        "hold_ms": hold_ms,
        "viewport": format!("{w}x{h}"),
        "frame_bytes": u64::from(w) * u64::from(h) * 4,
        "window_secs": stats::round(wall),
        "delivered_frames": count,
        "delivered_fps": stats::round(count as f64 / wall),
        "delivered_mib_per_s": stats::round(bytes as f64 / wall / 1048576.0),
        "seq_gaps_lost_frames": gaps,
        "client_received_frames": stats1.received_frames - stats0.received_frames,
        "client_dropped_frames": stats1.dropped_frames - stats0.dropped_frames,
        "publish_to_available_ms": stats::dist(latency),
        "inter_arrival_ms": stats::dist(arrival),
        "cpu_percent_one_core_whole_process": stats::round(
            (ticks1 - ticks_start) as f64 / procfs::CLK_TCK / wall * 100.0),
        "process_rss_mib": { "start": procfs::mib(rss0), "peak_sample": procfs::mib(peak_rss), "end": procfs::mib(own_mem.rss_kib) },
        "close": format!("{close:?}"),
        "server_exit": exit,
    }));
}

fn main() {
    let mut argv = std::env::args().skip(1);
    let (Some(renderer), Some(work_dir), Some(scenario)) = (argv.next(), argv.next(), argv.next())
    else {
        fail("usage: measure-renderer <evorch-renderer> <work-dir> <scenario> [key=value ...]");
    };
    let args = Args(
        argv.filter_map(|a| {
            a.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect(),
    );
    helper::become_subreaper();
    let config = Config {
        renderer: PathBuf::from(renderer),
        work_dir: PathBuf::from(work_dir),
    };
    eprintln!("environment: {}", environment());
    match scenario.as_str() {
        "env" => emit(environment()),
        "startup" => startup(&config, &args),
        "stream" => stream(&config, &args, false),
        "transfer" => stream(&config, &args, true),
        "tabs" => tabs(&config, &args),
        "ipc" => ipc(&args),
        other => fail(&format!("unknown scenario {other}")),
    }
}
