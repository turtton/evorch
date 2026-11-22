//! External controller. Success is decided by stdout events and pidfds; the
//! only clock uses are (a) the causal lower bound of the real ten-second
//! watchdog and (b) the observation window that proves a negative, which
//! cannot be an event. Every wait is bounded and cleanup never waits without
//! a bound.
use super::{
    fixture, fixtures,
    observer::{PinnedProcess, ProcessTree, bounded},
    procs::{self, check},
    tls,
};
use rustix::process::Signal;
use serde_json::{Value, json};
use std::{
    fs, io,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
    sync::mpsc,
};

/// Failure watchdogs only (never a success condition, never a speed gate).
const NAVIGATED: Duration = Duration::from_secs(100);
const BLOCKED: Duration = Duration::from_secs(60);
const EXIT: Duration = Duration::from_secs(30);
const BACKSTOP: Duration = Duration::from_secs(30);
const CELL: Duration = Duration::from_secs(300);
const CLEANUP: Duration = Duration::from_secs(30);
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(5);
/// The product constant under test.
const EXPIRY: Duration = Duration::from_secs(10);
/// A negative has no event: stay observably alive longer than the watchdog.
const WINDOW: Duration = Duration::from_secs(11);
/// Same observable as B3-08: only the page's script produces it.
const PAGE_RSS_KIB: u64 = 300 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine {
    Chromium,
    Cef,
}

fn spec(cell: &str) -> Option<(Engine, bool)> {
    Some(match cell {
        "expiry_chromium_t" => (Engine::Chromium, false),
        "expiry_chromium_stopped_t" => (Engine::Chromium, true),
        "expiry_cef_t" => (Engine::Cef, false),
        "expiry_cef_stopped_t" => (Engine::Cef, true),
        _ => return None,
    })
}

pub fn run_cell(cell: &str) -> io::Result<()> {
    check(
        !cfg!(debug_assertions),
        "core_browser_expiry must be built with --release (the release test binary is the artifact)",
    )?;
    ProcessTree::enable_controller_subreaper()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(bounded(CELL, "whole-cell failure watchdog", run(cell)))
    }));
    let cleanup = runtime.block_on(ProcessTree::cleanup_controller(CLEANUP));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
    match result {
        Ok(result) => result.and(cleanup),
        Err(_) => {
            cleanup?;
            Err(io::Error::other(
                "cell panicked; controller reclaimed descendants",
            ))
        }
    }
}

fn emit(cell: &str, kind: &str, value: &Value) -> io::Result<()> {
    println!("{value}");
    if let Some(dir) = std::env::var_os("EVORCH_B3_EVIDENCE_DIR") {
        fs::create_dir_all(&dir)?;
        fs::write(
            Path::new(&dir).join(format!(
                "browser-expiry-{cell}-{}-{kind}.json",
                std::process::id()
            )),
            serde_json::to_vec_pretty(value)?,
        )?;
    }
    Ok(())
}

/// The next worker stdout line with `prefix`, within `limit`.
async fn event(
    lines: &mut mpsc::UnboundedReceiver<String>,
    prefix: &str,
    limit: Duration,
) -> io::Result<String> {
    bounded(limit, &format!("worker event {prefix}"), async {
        loop {
            let line = lines
                .recv()
                .await
                .ok_or_else(|| io::Error::other(format!("worker ended before {prefix}")))?;
            if line.starts_with(prefix) {
                return Ok(line);
            }
        }
    })
    .await
}

fn find_target_root(
    engine: Engine,
    tree: &ProcessTree,
    renderer: Option<&Path>,
    home: &Path,
) -> io::Result<i32> {
    let mut found: Vec<i32> = tree
        .descendants
        .values()
        .map(|pin| pin.identity.pid)
        .filter(|pid| {
            let args = procs::cmdline(*pid);
            if engine == Engine::Chromium {
                args.iter().any(|a| a == "--remote-debugging-port=0")
                    && procs::process_type(&args).is_none()
            } else {
                procs::exe(*pid).is_some_and(|exe| Some(exe.as_path()) == renderer)
            }
        })
        .collect();
    check(
        found.len() == 1,
        format!("expected exactly one target root, found {found:?}"),
    )?;
    let root = found.pop().unwrap();
    if engine == Engine::Chromium {
        let environ = fs::read(format!("/proc/{root}/environ"))?;
        let expected = format!("HOME={}", home.display());
        check(
            environ.split(|b| *b == 0).any(|e| e == expected.as_bytes()),
            "Chromium does not run with the cell's HOME (its NSS store would not be the cell's)",
        )?;
    }
    Ok(root)
}

async fn run(cell: &str) -> io::Result<()> {
    let (engine, stopped) =
        spec(cell).ok_or_else(|| io::Error::other(format!("unknown cell {cell}")))?;
    let artifact = test_support::product_artifact::require_from_env();
    let bundle = artifact
        .guard
        .as_ref()
        .ok_or_else(|| io::Error::other("sealed artifact has no guard bundle"))?
        .bundle
        .clone();
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize()?;
    let sockets = dir.sockets().to_path_buf();
    let (home, tmp, worker_root) = (root.join("home"), root.join("tmp"), root.join("worker"));
    for path in [&home, &tmp, &worker_root] {
        fs::create_dir_all(path)?;
    }
    fs::create_dir(sockets.join("ipc"))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--watchdog-child")
        .env("EVORCH_WATCHDOG_CASE", "expiry_capture_first")
        .env("EVORCH_WATCHDOG_ROOT", &worker_root)
        .env("EVORCH_WATCHDOG_SOCKETS", &sockets)
        .env("EVORCH_PROCESS_GUARD_BUNDLE", &bundle)
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("XDG_RUNTIME_DIR", &sockets)
        .env("XDG_STATE_HOME", worker_root.join("state"))
        .env("XDG_CACHE_HOME", worker_root.join("cache"))
        .env("TOKIO_WORKER_THREADS", "2");
    let mut served = None;
    let mut renderer: Option<PathBuf> = None;
    match engine {
        Engine::Chromium => {
            let chrome = fixtures::require_absolute_file("CHROME")?;
            let fixture = tls::Fixture::start(false).await?;
            fixtures::trust_ca(&root, &home, &fixture.ca_pem).await?;
            command
                .env("EVORCH_EXPIRY_BROWSER", "chromium")
                .env("EVORCH_EXPIRY_BROWSER_URL", fixture.url())
                .env("CHROME", chrome);
            served = Some(fixture);
        }
        Engine::Cef => {
            let real = fixtures::require_absolute_file("EVORCH_RENDERER_EXE")?;
            check(
                real.with_file_name("evorch-cef-helper").is_file(),
                "evorch-cef-helper must sit beside the renderer",
            )?;
            command
                .env("EVORCH_EXPIRY_BROWSER", "cef")
                .env("EVORCH_EXPIRY_PROFILES", root.join("profiles"))
                .env("EVORCH_RENDERER_EXE", &real);
            renderer = Some(real.canonicalize()?);
        }
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut tree = ProcessTree::new(child.id().unwrap() as i32)?;
    let _stdin = child.stdin.take();
    let (tx, mut lines) = mpsc::unbounded_channel();
    let stdout = child.stdout.take().unwrap();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stderr = child.stderr.take().unwrap();
    let stderr = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });
    let marker = root.to_str().unwrap().to_owned();
    let mut marked: Vec<PinnedProcess> = Vec::new();
    let result = async {
        // 1. The navigation is complete: the worker read the page back (its
        //    own engine read) and, for each engine, an observation of the
        //    controller's own confirms it independently of the worker.
        let (navigated, heading) = if engine == Engine::Chromium {
            ("NAVIGATED chromium", fixture::CHROMIUM_HEADING)
        } else {
            ("NAVIGATED cef", fixture::CEF_HEADING)
        };
        let line = event(&mut lines, "NAVIGATED ", NAVIGATED).await?;
        check(
            line.starts_with(navigated) && line.contains(heading),
            format!("unexpected navigation event: {line}"),
        )?;
        let mut requests = Value::Null;
        if let Some(fixture) = &served {
            let served = fixture.requests.lock().unwrap().clone();
            check(
                served.iter().any(|line| line.starts_with("GET /b3-08")),
                format!("the HTTPS fixture never received the request: {served:?}"),
            )?;
            requests = json!(served);
        }
        // 2. Irreversible capture is published after the navigation (the
        //    worker prints BLOCKED once the stage-11 stall is entered).
        event(&mut lines, "BLOCKED", BLOCKED).await?;
        tree.refresh()?;
        let root_pid = find_target_root(engine, &tree, renderer.as_deref(), &home)?;
        let (guard, guard_json) = procs::guard_owner(&tree, root_pid)?;
        let members = procs::subtree(&tree, guard);
        let described = procs::describe(&tree, &members, Some(root_pid))?;
        let types: Vec<Option<String>> = members
            .iter()
            .map(|pid| procs::process_type(&procs::cmdline(*pid)).map(str::to_owned))
            .collect();
        let count = |kind: &str| types.iter().filter(|t| t.as_deref() == Some(kind)).count();
        if engine == Engine::Chromium {
            check(
                count("renderer") >= 1 && count("gpu-process") + count("utility") >= 1,
                format!("Chromium has no renderer/service processes: {types:?}"),
            )?;
        } else {
            check(
                count("zygote") >= 3 && count("utility") >= 1,
                format!("CEF has no zygote-forked/utility processes: {types:?}"),
            )?;
            // Independent of the worker's claim: the page's script ran in a
            // guard-owned member (only it holds this much memory).
            let peak = members
                .iter()
                .filter(|pid| **pid != root_pid && **pid != guard)
                .filter_map(|pid| procs::rss_kib(*pid))
                .max()
                .unwrap_or(0);
            check(
                peak >= PAGE_RSS_KIB,
                format!("no guard-owned member holds the page script's memory ({peak} KiB): the page was not loaded"),
            )?;
        }
        let classified = procs::classify(&tree, &members).await?;
        check(
            classified.alive.contains(&root_pid)
                && classified.alive.contains(&guard)
                && classified.alive.len() >= 3,
            format!("target tree is not alive after navigation: {classified:?}"),
        )?;
        marked = procs::unreachable_marked(&tree, &marker)?;
        // Fault injection of the `_stopped_` cells: a stopped member cannot
        // notice that its browser or host died and exit by itself, so only a
        // kill that reaches it ends it. The root and the guard keep running.
        let paused: Vec<i32> = if stopped {
            classified
                .alive
                .iter()
                .copied()
                .filter(|pid| *pid != root_pid && *pid != guard)
                .collect()
        } else {
            Vec::new()
        };
        for pid in &paused {
            tree.require(*pid)?.signal(Signal::STOP)?;
        }
        check(
            !stopped || !paused.is_empty(),
            "the stopped cell has nothing to stop",
        )?;
        emit(
            cell,
            "ready",
            &json!({
                "cell": cell, "class": "T", "artifact": "release test binary (not the product ELF)",
                "engine": if engine == Engine::Chromium { "chromium" } else { "cef" },
                "navigation_event": line, "fixture_requests": requests,
                "owner": tree.owner.identity, "guard": guard_json, "target_root": root_pid,
                "members": described, "exited_before_expiry": classified.gone,
                "relation_changed_but_alive": classified.relation_changed,
                "stopped_before_expiry": paused,
                "unreachable_marked": marked.iter().map(procs::describe_pin).collect::<Vec<_>>(),
            }),
        )?;
        // 3. Capture alone, after a finished navigation, must not fire.
        let tree_ref = &tree;
        let paused_ref = &paused;
        let alive = |what: &'static str| async move {
            let (tree, paused) = (tree_ref, paused_ref);
            check(tree.owner.unchanged(), format!("{what}: worker changed"))?;
            for pid in [root_pid, guard] {
                check(
                    tree.require(pid)?.unchanged() && !procs::is_dead(tree.require(pid)?).await?,
                    format!("{what}: {pid} died or changed"),
                )?;
            }
            for pid in paused {
                check(
                    !procs::is_dead(tree.require(*pid)?).await?,
                    format!("{what}: stopped member {pid} died by itself"),
                )?;
            }
            Ok::<_, io::Error>(())
        };
        check(
            tokio::time::timeout(WINDOW, tree.owner.exited())
                .await
                .is_err(),
            "capture alone fired the watchdog",
        )?;
        alive("after the capture-only window").await?;
        // 4. The explicit shutdown signal completes the arming condition.
        let sent = Instant::now();
        tree.owner.signal(Signal::TERM)?;
        bounded(BACKSTOP, "exit 74 not observed", tree.owner.exited()).await?;
        let elapsed = sent.elapsed();
        check(
            elapsed >= EXPIRY,
            format!("worker ended {elapsed:?} after the signal, before ten real seconds"),
        )?;
        let status = bounded(EXIT, "worker reap", async { child.wait().await }).await?;
        let text = bounded(EXIT, "worker stderr", async {
            (&mut { stderr }).await.map_err(io::Error::other)
        })
        .await
        .unwrap_or_default();
        check(
            status.code() == Some(74) && status.signal().is_none(),
            format!("expected exit 74, got {status}: {text}"),
        )?;
        check(
            text.contains("planned shutdown exceeded 10s"),
            format!("missing fixed diagnostic: {text}"),
        )?;
        // 5. Death by pidfd only, before any cleanup: the guard reaps the
        //    owned group; escaped processes are observed, not guaranteed.
        tree.expect_dead(&members, EXIT).await?;
        for pin in &marked {
            bounded(EXIT, "unreachable marked process death", pin.exited()).await?;
        }
        let survivors = procs::unreachable_marked(&tree, &marker)?;
        check(
            survivors.is_empty(),
            format!(
                "processes of the cell survived: {:?}",
                survivors.iter().map(procs::describe_pin).collect::<Vec<_>>()
            ),
        )?;
        emit(
            cell,
            "death",
            &json!({
                "cell": cell, "exit": 74, "elapsed_ms_after_signal": elapsed.as_millis(),
                "worker": tree.owner.identity, "pidfd_dead": members,
                "unreachable_marked_dead": marked.iter().map(|p| p.identity.pid).collect::<Vec<_>>(),
                "cleanup_not_started": true,
            }),
        )
    }
    .await;
    if result.is_err() {
        let _ = tree.refresh();
        for pin in tree.descendants.values() {
            eprintln!(
                "live diagnostic {:?}: {:?}",
                pin.identity,
                procs::cmdline(pin.identity.pid)
            );
        }
    }
    // Independent backstop: kill by pidfd, reap, discover adopted descendants,
    // each bounded.
    if tree.owner.unchanged() {
        let _ = tree.refresh();
    }
    for pin in &marked {
        let _ = pin.signal(Signal::KILL);
    }
    let cleanup = tree.cleanup(CLEANUP).await;
    let _ = bounded(EXIT, "worker final reap", async { child.wait().await }).await;
    result.and(cleanup)
}
