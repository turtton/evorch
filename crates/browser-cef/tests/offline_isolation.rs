//! End-to-end checks against a real CEF runtime. They need the CEF
//! distribution on the library path, so they are ignored by default; see
//! `docs/browser.md` for how to run them.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use browser_cef::devtools::DevTools;
use browser_cef::offline::ArtifactBundle;
use browser_cef::{ArtifactContext, CefRuntime, NetLog, OsrBrowser, RuntimeConfig};
use serde_json::{Value, json};

const LEAK_PAGE: &str = include_str!("fixtures/leak.html");

fn loopback_listener() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (port, hits)
}

fn open(bundle: ArtifactBundle) -> (OsrBrowser, Arc<NetLog>) {
    let context = ArtifactContext::create(bundle).expect("artifact context");
    let log = Arc::clone(&context.log);
    let browser = OsrBrowser::create(
        context,
        &ArtifactContext::url("index.html"),
        [800, 600],
        2.0,
    )
    .expect("browser");
    (browser, log)
}

fn parent_pid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))?
        .trim()
        .parse()
        .ok()
}

fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|byte| *byte == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

fn is_sandboxed_zygote(pid: u32) -> bool {
    let args = cmdline(pid);
    args.iter().any(|arg| arg == "--type=zygote")
        && !args.iter().any(|arg| arg == "--no-zygote-sandbox")
}

/// Renderers keep the zygote command line, so they are found by position:
/// children of the sandboxed zygote's PID-namespace init, below this process.
fn own_renderers() -> Vec<u32> {
    let me = std::process::id();
    let ancestors =
        |pid| std::iter::successors(Some(pid), |pid| parent_pid(*pid).filter(|ppid| *ppid > 1));
    std::fs::read_dir("/proc")
        .expect("read /proc")
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let chain: Vec<u32> = ancestors(*pid).collect();
            chain.len() > 3
                && chain[..3].iter().all(|pid| is_sandboxed_zygote(*pid))
                && chain.contains(&me)
        })
        .collect()
}

fn leak_probes_stay_offline() {
    let (port, hits) = loopback_listener();
    let page = LEAK_PAGE.replace("__PORT__", &port.to_string());
    let (browser, log) = open(ArtifactBundle::html(page));
    let navigate = format!("http://127.0.0.1:{port}/navigate");

    browser.wait_for(|status| {
        status.title.starts_with('{')
            && status.blocked_navigations.contains(&navigate)
            && browser.frame_seq() > 0
    });

    let status = browser.status().clone();
    let report: Value = serde_json::from_str(&status.title).expect("probe report");
    assert_eq!(report["leaked"], json!([]), "{report}");
    assert!(
        report["violations"].as_u64() > Some(0),
        "CSP was not applied: {report}"
    );
    assert!(
        status.url.starts_with("https://artifact.evorch.invalid/"),
        "{}",
        status.url
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0, "a probe reached loopback");
    let served = log.served.lock().unwrap().clone();
    assert_eq!(served, ["https://artifact.evorch.invalid/index.html"]);

    let webrtc = DevTools::attach(&browser)
        .expect("devtools")
        .call(
            "Runtime.evaluate",
            json!({ "expression": "typeof RTCPeerConnection", "returnByValue": true }),
            // nextest's slow-timeout bounds the wait; the reply has no other deadline.
            Duration::MAX,
        )
        .expect("Runtime.evaluate");
    assert_eq!(webrtc["result"]["value"], "undefined");
}

fn renderer_crash_reloads_the_artifact() {
    let (browser, _log) = open(ArtifactBundle::html(
        "<!doctype html><title>alive</title><p>crash me</p>",
    ));
    browser.wait_for(|status| status.title == "alive" && status.load_end_count > 0);
    let loads = browser.status().load_end_count;

    // SIGKILL rather than CDP `Page.crash`: a trapped renderer can stall for
    // minutes in the host's core dump handler before CEF reports it.
    let renderers = own_renderers();
    assert!(!renderers.is_empty(), "no renderer found");
    for pid in renderers {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }

    browser.wait_for(|status| status.renderer_crashes == 1 && status.load_end_count > loads);
    assert_eq!(browser.status().title, "alive");
}

/// CEF initializes once per process, so the scenarios share one runtime.
#[test]
#[ignore = "needs the CEF runtime on LD_LIBRARY_PATH; see docs/browser.md"]
fn cef_preview_is_offline_and_recovers() {
    let runtime = CefRuntime::initialize(RuntimeConfig {
        helper_path: PathBuf::from(env!("CARGO_BIN_EXE_evorch-cef-helper")),
        ..RuntimeConfig::offline()
    })
    .expect("CEF runtime");

    leak_probes_stay_offline();
    renderer_crash_reloads_the_artifact();

    drop(runtime);
}
