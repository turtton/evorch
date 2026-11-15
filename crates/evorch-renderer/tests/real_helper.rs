//! The real `evorch-renderer` binary with real CEF, driven through the real
//! client. The CEF tests are `#[ignore]`d like `browser-cef`'s: they need the
//! CEF distribution, `evorch-cef-helper` built next to the renderer, and
//! unprivileged user namespaces for CEF's sandbox (CI runs them with
//! `--run-ignored all`).

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use renderer_ipc::*;
use rustix::process::{Pid, Signal, kill_process_group};
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

/// Stops the helper's whole process group, however the test ends: CEF starts
/// descendants of its own that nobody else would reap.
struct GroupGuard(i32);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = Pid::from_raw(self.0) {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
}

struct Spawned {
    child: Child,
    client: RendererClient,
    _group: GroupGuard,
    _dir: tempfile::TempDir,
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
        .stdout(channels.helper_stdout)
        .process_group(0);
    let child = command.spawn().expect("spawn evorch-renderer");
    let group = GroupGuard(child.id() as i32);
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
        child,
        client,
        _group: group,
        _dir: dir,
    }
}

fn open(client: &RendererClient, width: u32, height: u32) {
    let (open, body) = OpenArtifact::with_files(
        SessionId(1),
        TabId(1),
        Viewport { width, height },
        30,
        30_000,
        vec![(
            "index.html".into(),
            "text/html".into(),
            HTML.as_bytes().to_vec(),
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
    let status = spawned.child.wait().unwrap();
    assert!(status.success(), "{status:?}");
}

#[test]
#[ignore = "needs the CEF distribution and evorch-cef-helper"]
fn a_helper_killed_without_warning_is_seen_as_a_closed_connection() {
    let mut spawned = spawn_helper();
    open(&spawned.client, 160, 120);
    spawned.child.kill().unwrap();
    spawned.child.wait().unwrap();
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
    let status = spawned.child.wait().unwrap();
    assert!(status.success(), "{status:?}");
}
