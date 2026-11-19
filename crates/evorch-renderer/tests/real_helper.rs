//! The real `evorch-renderer` binary with real CEF, driven through the real
//! client. The CEF tests are `#[ignore]`d like `browser-cef`'s: they need the
//! CEF distribution, `evorch-cef-helper` built next to the renderer, and
//! unprivileged user namespaces for CEF's sandbox (CI runs them with
//! `--run-ignored all`).
//!
//! Linux only: the harness reaps the helper's process group with a child
//! subreaper and checks it through `/proc`.

#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};

use renderer_ipc::*;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use serde_json::json;

const GENERATION: RendererGeneration = RendererGeneration(1);

const HTML: &str = r#"<!doctype html><html><body style="margin:0;overflow:hidden">
<canvas id="c" width="800" height="600"></canvas>
<img src="missing.png">
<script>
const g = document.getElementById('c').getContext('2d');
let n = 0;
(function draw() {
  n += 1;
  g.fillStyle = 'rgb(' + (n % 200) + ',80,160)';
  g.fillRect(0, 0, 800, 600);
  requestAnimationFrame(draw);
})();
</script></body></html>"#;

fn renderer_exe() -> PathBuf {
    PathBuf::from(
        std::env::var("CARGO_BIN_EXE_evorch-renderer").expect("cargo sets the binary path"),
    )
}

/// Owns the helper process and its whole process group: CEF starts
/// descendants of its own that nobody else would reap. However the test ends
/// (also by a failed assertion, or a helper that never came up), dropping this
/// stops the group, reaps every member and checks that none is left.
///
/// The test process becomes a child subreaper, so descendants orphaned by the
/// death of their parent come to it instead of to init and can be waited for.
/// A process that leaves the group (`setsid`, `setpgid`) is outside this
/// guarantee; the check below would not see it either.
struct HelperProcess {
    child: Child,
    group: Pid,
}

impl HelperProcess {
    fn spawn(command: &mut Command) -> Self {
        rustix::process::set_child_subreaper(Some(rustix::process::getpid()))
            .expect("become a child subreaper");
        let child = command
            .process_group(0)
            .spawn()
            .expect("spawn evorch-renderer");
        let group = Pid::from_raw(child.id() as i32).expect("a child has a pid");
        Self { child, group }
    }

    fn wait(&mut self) -> ExitStatus {
        self.child.wait().expect("wait for the helper")
    }

    /// Blocks until every other member of the group has exited and been
    /// reaped. A descendant that outlives the helper keeps this waiting, which
    /// the runner's hang detection reports.
    fn reap_descendants(&self) {
        loop {
            match waitid(WaitId::Pgid(Some(self.group)), WaitIdOptions::EXITED) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(error) => {
                    assert_eq!(error, Errno::CHILD, "waiting for the group: {error}");
                    return;
                }
            }
        }
    }

    /// Process ids that still belong to the group and are not zombies.
    fn live_members(&self) -> Vec<i32> {
        let group = self.group.as_raw_nonzero().get();
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            else {
                continue;
            };
            // `pid (comm) state ppid pgrp ...`; comm may hold spaces and parentheses.
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let (state, pgrp) = (fields.next(), fields.nth(1));
            if pgrp.and_then(|p| p.parse::<i32>().ok()) == Some(group) && state != Some("Z") {
                found.push(pid);
            }
        }
        found
    }
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        let _ = kill_process_group(self.group, Signal::KILL);
        let _ = self.child.wait();
        self.reap_descendants();
        let left = self.live_members();
        if std::thread::panicking() {
            if !left.is_empty() {
                eprintln!("processes left in the helper's group: {left:?}");
            }
        } else {
            assert!(
                left.is_empty(),
                "processes left in the helper's group: {left:?}"
            );
        }
    }
}

/// Field order is drop order: the client closes its channels first (a helper
/// still running then stops by itself), then the group is stopped and reaped,
/// and the profile directory goes last.
struct Spawned {
    client: RendererClient,
    process: HelperProcess,
    dir: tempfile::TempDir,
}

fn spawn_helper() -> Spawned {
    let exe = renderer_exe();
    let cef_helper = exe.with_file_name("evorch-cef-helper");
    assert!(
        cef_helper.is_file(),
        "build it first: cargo build -p browser-cef --features cef --bin evorch-cef-helper"
    );
    let dir = tempfile::Builder::new().prefix("er-").tempdir().unwrap();
    let channels = stdio_channels().unwrap();
    let mut command = Command::new(&exe);
    command
        .arg("--cache-dir")
        .arg(dir.path().join("profile"))
        .arg("--log-file")
        .arg(dir.path().join("cef.log"))
        // Chromium binds a Unix socket under TMPDIR, whose path limit a long
        // scratch directory exceeds; the working directory is that scratch.
        .current_dir(dir.path())
        .env("TMPDIR", "/proc/self/cwd")
        .env("EVORCH_CEF_HELPER", &cef_helper)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XDG_SESSION_TYPE")
        .stdin(channels.helper_stdin)
        .stdout(channels.helper_stdout);
    let process = HelperProcess::spawn(&mut command);
    // `command` still owns the helper's ends; dropping it closes the core's
    // copies, so the helper's exit reads as a closed channel.
    drop(command);
    let (client, info) = RendererClient::connect(
        channels.control,
        channels.frames,
        ClientConfig::new(GENERATION),
    )
    .unwrap_or_else(|error| panic!("helper did not come up: {error}"));
    assert_eq!(info.protocol, PROTOCOL_VERSION);
    Spawned {
        client,
        process,
        dir,
    }
}

fn open(client: &RendererClient, width: u32, height: u32) {
    open_html(client, HTML, width, height);
}

fn open_html(client: &RendererClient, html: &str, width: u32, height: u32) {
    let (open, body) = OpenArtifact::with_files(
        SessionId(1),
        TabId(1),
        Viewport { width, height },
        30,
        30_000,
        vec![(
            "index.html".into(),
            "text/html".into(),
            html.as_bytes().to_vec(),
        )],
    );
    let viewport = client.open_artifact(open, &body).unwrap();
    assert_eq!((viewport.width, viewport.height), (width, height));
}

#[test]
fn a_helper_that_cannot_use_its_channels_exits_with_a_diagnostic() {
    // stdin and stdout are not sockets here, so it must refuse to start
    // instead of waiting on them.
    let output = Command::new(renderer_exe())
        .args(["--cache-dir", "/nonexistent-evorch-renderer-test"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a socket"));
}

#[test]
fn bad_arguments_exit_with_status_two() {
    for args in [&[][..], &["--cache-dir", "relative"][..], &["--bogus"][..]] {
        let output = Command::new(renderer_exe())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[test]
#[ignore = "needs the CEF distribution and evorch-cef-helper"]
fn an_artifact_renders_offline_streams_frames_and_is_operated_through_devtools() {
    let mut spawned = spawn_helper();
    let client = &spawned.client;
    open(client, 320, 240);
    let tab = TabId(1);

    client.set_streaming(tab, true).unwrap();
    let frame = client.wait_frame(None).expect("a frame arrives");
    assert_eq!((frame.tab, frame.width, frame.height), (tab, 320, 240));
    assert_eq!(frame.bgra.len(), 320 * 240 * 4);
    assert_eq!(frame.bgra[3], 255, "the page is opaque");
    client.recycle_frame(frame);

    let evaluated = client
        .cdp(
            tab,
            "Runtime.evaluate",
            json!({"expression": "1 + 1", "returnByValue": true}),
            10_000,
        )
        .unwrap();
    assert_eq!(evaluated["result"]["value"], 2);

    let png = client.capture_png(tab, 10_000).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");

    let notices = client.take_notices(tab).unwrap();
    assert!(
        notices.blocked.iter().any(|n| n.contains("missing.png")),
        "{notices:?}"
    );

    match client.cdp(
        tab,
        "Page.navigate",
        json!({"url": "https://example.com/"}),
        1000,
    ) {
        Err(ClientError::Remote(error)) => assert_eq!(error.kind, ErrorKind::Unsupported),
        other => panic!("{other:?}"),
    }

    client
        .resize(
            tab,
            Viewport {
                width: 400,
                height: 300,
            },
        )
        .unwrap();
    // The page keeps animating, so frames of the new size follow.
    loop {
        let frame = client.wait_frame(None).expect("frames continue");
        let resized = (frame.width, frame.height) == (400, 300);
        client.recycle_frame(frame);
        if resized {
            break;
        }
    }

    client.close_tab(tab).unwrap();
    assert_eq!(
        client.shutdown(std::time::Duration::from_secs(60)),
        CloseReason::ShutdownAcknowledged
    );
    let status = spawned.process.wait();
    assert!(status.success(), "{status:?}");
    // A normal stop leaves no CEF process behind.
    spawned.process.reap_descendants();
    assert_eq!(spawned.process.live_members(), Vec::<i32>::new());
}

#[test]
#[ignore = "needs the CEF distribution and evorch-cef-helper"]
fn a_helper_killed_without_warning_is_seen_as_a_closed_connection() {
    let mut spawned = spawn_helper();
    open(&spawned.client, 160, 120);
    spawned.process.child.kill().unwrap();
    spawned.process.wait();
    // Only the control channel's close decides this: a CEF descendant that
    // inherited a channel would keep it open and make this wait forever.
    assert_eq!(spawned.client.wait_closed(), CloseReason::PeerClosed);
    assert!(matches!(
        spawned.client.settle(TabId(1), 1000),
        Err(ClientError::Closed(CloseReason::PeerClosed))
    ));
}

#[test]
#[ignore = "needs the CEF distribution and evorch-cef-helper"]
fn closing_the_connection_makes_the_helper_exit_by_itself() {
    let mut spawned = spawn_helper();
    open(&spawned.client, 160, 120);
    spawned.client.close();
    let status = spawned.process.wait();
    assert!(status.success(), "{status:?}");
    // A normal stop leaves no CEF process behind.
    spawned.process.reap_descendants();
    assert_eq!(spawned.process.live_members(), Vec::<i32>::new());
}

const FILE_INPUT_HTML: &str = r#"<!doctype html><html><body>
<input type="file" id="f">
<script>
window.readChosen = async () => {
  const files = document.getElementById('f').files;
  return files.length ? await files[0].text() : null;
};
</script></body></html>"#;

/// A file outside the artifact must stay out of reach, however DevTools is
/// asked: without the exact-name allowlist, `DOM.setFileInputFiles` selected
/// it for the page's file input and the page could read its contents.
#[test]
#[ignore = "needs the CEF distribution and evorch-cef-helper"]
fn a_host_file_cannot_be_given_to_the_page_through_devtools() {
    const SECRET: &str = "evorch-sentinel-must-not-be-readable";
    let spawned = spawn_helper();
    let client = &spawned.client;
    let sentinel = spawned.dir.path().join("sentinel.txt");
    std::fs::write(&sentinel, SECRET).unwrap();
    open_html(client, FILE_INPUT_HTML, 200, 100);
    let tab = TabId(1);

    // A real node of the page, so the refusals below are the allowlist
    // answering and not a malformed request.
    let document = client
        .cdp(tab, "DOM.getDocument", json!({"depth": 0}), 10_000)
        .unwrap();
    let root = document["root"]["nodeId"].clone();
    let input = client
        .cdp(
            tab,
            "DOM.querySelector",
            json!({"nodeId": root, "selector": "#f"}),
            10_000,
        )
        .unwrap();
    let node = input["nodeId"].clone();
    assert!(node.as_i64().is_some_and(|id| id > 0), "{input}");

    let refused =
        |method: &str, params: serde_json::Value| match client.cdp(tab, method, params, 10_000) {
            Err(ClientError::Remote(error)) => {
                assert_eq!(error.kind, ErrorKind::Unsupported, "{method}")
            }
            other => panic!("{method} was not refused: {other:?}"),
        };
    refused(
        "DOM.setFileInputFiles",
        json!({"nodeId": node, "files": [sentinel.to_str().unwrap()]}),
    );
    refused(
        "Input.dispatchDragEvent",
        json!({"type": "drop", "x": 10, "y": 10, "data": {"items": [], "files": [sentinel.to_str().unwrap()], "dragOperationsMask": 1}}),
    );
    refused(
        "Page.setInterceptFileChooserDialog",
        json!({"enabled": true}),
    );
    refused("DOM.getFileInfo", json!({"objectId": "1"}));

    let read = client
        .cdp(
            tab,
            "Runtime.evaluate",
            json!({"expression": "window.readChosen()", "awaitPromise": true, "returnByValue": true}),
            10_000,
        )
        .unwrap();
    assert!(
        read["result"]["value"].is_null(),
        "the page read a host file: {read}"
    );
    let count = client
        .cdp(
            tab,
            "Runtime.evaluate",
            json!({"expression": "document.getElementById('f').files.length", "returnByValue": true}),
            10_000,
        )
        .unwrap();
    assert_eq!(count["result"]["value"], 0);
}

/// Inodes of the sockets that listen (TCP) or are bound without a peer (UDP), from /proc/net.
fn listening_socket_inodes() -> std::collections::HashSet<u64> {
    let mut found = std::collections::HashSet::new();
    for (file, state) in [("tcp", "0A"), ("tcp6", "0A"), ("udp", "07"), ("udp6", "07")] {
        let Ok(text) = std::fs::read_to_string(format!("/proc/net/{file}")) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.get(3) == Some(&state)
                && let Some(inode) = fields.get(9).and_then(|i| i.parse().ok())
            {
                found.insert(inode);
            }
        }
    }
    found
}

/// Pids of this process and of every descendant, whatever group it is in.
fn this_process_tree() -> Vec<i32> {
    let mut parents = std::collections::HashMap::new();
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
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        if let Some(parent) = rest
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse::<i32>().ok())
        {
            parents.insert(pid, parent);
        }
    }
    let me = std::process::id() as i32;
    parents
        .keys()
        .copied()
        .filter(|&pid| {
            let mut at = pid;
            while let Some(&parent) = parents.get(&at) {
                if parent == me || at == me {
                    return true;
                }
                if parent <= 1 {
                    return false;
                }
                at = parent;
            }
            at == me
        })
        .chain(std::iter::once(me))
        .collect()
}

/// The legacy GUI's in-process CEF (`browser_cef::CefRuntime`, started by `html_preview`) and
/// the core's renderer helper run side by side, as they do while the old GUI and the core
/// coexist (until stages 19 and 20 remove the old owners). They must share no profile
/// directory, no listening port and no lock: the legacy runtime keeps its cache under
/// `temp_dir/evorch-cef-<pid>`, the helper under the core's private
/// `renderer-profiles/<uuid>` (here `--cache-dir`), both use pipes and headless
/// off-screen rendering, and neither opens a TCP or UDP socket.
#[test]
#[ignore = "needs the CEF distribution and evorch-cef-helper"]
fn the_helper_and_the_in_process_cef_of_the_legacy_gui_share_no_profile_port_or_lock() {
    let exe = renderer_exe();
    let legacy_config = browser_cef::RuntimeConfig {
        helper_path: exe.with_file_name("evorch-cef-helper"),
        ..browser_cef::RuntimeConfig::offline()
    };
    let legacy_cache = legacy_config.root_cache_path.clone();
    let legacy = browser_cef::CefRuntime::initialize(legacy_config).expect("legacy CEF runtime");
    assert!(legacy_cache.is_dir(), "the legacy runtime owns its cache");

    let mut spawned = spawn_helper();
    let helper_cache = spawned.dir.path().join("profile");
    assert!(
        !helper_cache.starts_with(&legacy_cache) && !legacy_cache.starts_with(&helper_cache),
        "profiles: {legacy_cache:?} and {helper_cache:?}"
    );
    open(&spawned.client, 320, 240);
    let tab = TabId(1);
    spawned.client.set_streaming(tab, true).unwrap();
    let frame = spawned
        .client
        .wait_frame(None)
        .expect("the helper streams while the legacy CEF runs");
    assert_eq!((frame.width, frame.height), (320, 240));
    spawned.client.recycle_frame(frame);

    // No process of either side listens on a TCP or UDP port.
    let listening = listening_socket_inodes();
    for pid in this_process_tree() {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let target = target.to_string_lossy().into_owned();
            if let Some(inode) = target
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|inode| inode.parse::<u64>().ok())
            {
                assert!(
                    !listening.contains(&inode),
                    "process {pid} listens on a port ({target})"
                );
            }
        }
    }

    spawned.client.close_tab(tab).unwrap();
    assert_eq!(
        spawned.client.shutdown(std::time::Duration::from_secs(60)),
        CloseReason::ShutdownAcknowledged
    );
    assert!(spawned.process.wait().success());
    spawned.process.reap_descendants();
    assert_eq!(spawned.process.live_members(), Vec::<i32>::new());
    assert!(
        legacy_cache.is_dir(),
        "the helper's end left the legacy cache alone"
    );
    drop(legacy);
}
