//! The product cells. Chromium and CEF are separate paths: neither stands in
//! for the other. Every cell starts the sealed product core through its own
//! entrance and drives it through the public protocol and a scripted model
//! provider. A cell then SIGKILLs only the core and requires every pinned
//! descendant to die, by pidfd, before any cleanup runs.
//!
//! Modes: `Complete` waits for a navigation that finished; `Navigating`
//! kills the core while the navigation is still in flight; `LaunchFailure`
//! (an external executable that exits at once replaces the renderer through
//! the product's own `EVORCH_RENDERER_EXEC`) and `InitFailure` (the real
//! renderer whose CEF cannot find its resources) kill the core after the
//! product reported the failure. All are P*: the product ELF is real and only
//! external dependencies are replaced.
use super::{
    controls, core_process as core, evidence, fixtures,
    observer::{ProcessTree, bounded},
    procs::{self, check},
    tls,
};
use mock_openai::{ResponseGate, ScriptedResponse, StreamingMockOpenAi};
use rustix::process::Signal;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

const EXIT: Duration = Duration::from_secs(30);
/// The navigating CEF page touches 6 x 64 MiB. The event threshold is below
/// that and far above any other member's resident set (checked on every
/// `cef_p` run, where no script runs).
const PAGE_RSS_KIB: u64 = 300 * 1024;
const CEF_HEADING: &str = "B3-08 CEF fixture";
const REVIEW_APPROVAL: &str = r#"{"approve":true,"reason":"requested navigation","risk_level":"low","procedure_step":"none","authorization_basis":"ordinary_task","authorization_refs":[0],"scope_match":"full","authorization_level":"medium"}"#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine {
    Chromium,
    Cef,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Complete,
    Navigating,
    LaunchFailure,
    InitFailure,
}
struct Spec {
    engine: Engine,
    mode: Mode,
    stopped: bool,
}

pub const CELLS: &[&str] = &[
    "chromium_p",
    "chromium_stopped_p",
    "cef_p",
    "cef_stopped_p",
    "chromium_navigating_p",
    "cef_navigating_p",
    "renderer_launch_failure_p",
    "cef_init_failure_p",
    "classify_control_t",
    "harness_timeout_control_t",
    "harness_panic_control_t",
];

fn spec(cell: &str) -> Option<Spec> {
    use {Engine::*, Mode::*};
    let (engine, mode, stopped) = match cell {
        "chromium_p" => (Chromium, Complete, false),
        "chromium_stopped_p" => (Chromium, Complete, true),
        "cef_p" => (Cef, Complete, false),
        "cef_stopped_p" => (Cef, Complete, true),
        "chromium_navigating_p" => (Chromium, Navigating, false),
        "cef_navigating_p" => (Cef, Navigating, false),
        "renderer_launch_failure_p" => (Cef, LaunchFailure, false),
        "cef_init_failure_p" => (Cef, InitFailure, false),
        _ => return None,
    };
    Some(Spec {
        engine,
        mode,
        stopped,
    })
}

fn tool_results(mock: &StreamingMockOpenAi) -> Vec<String> {
    let requests = mock.recorded_requests();
    let Some(last) = requests
        .iter()
        .rfind(|request| request.path == "/v1/chat/completions")
    else {
        return Vec::new();
    };
    last.body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "tool")
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect()
}

/// `artifact-<32 hex>` that the product sent back to the provider.
fn artifact_id(mock: &StreamingMockOpenAi) -> io::Result<String> {
    for request in mock.recorded_requests() {
        let body = request.body.to_string();
        for (at, _) in body.match_indices("artifact-") {
            let id: String = body[at..].chars().take("artifact-".len() + 32).collect();
            if id.len() == "artifact-".len() + 32
                && id["artifact-".len()..]
                    .chars()
                    .all(|c| c.is_ascii_hexdigit())
            {
                return Ok(id);
            }
        }
    }
    Err(io::Error::other(
        "no request carries the captured artifact id",
    ))
}

/// Releases the provider's held response before the mock is dropped, on every
/// path: normal return, error, whole-cell timeout (future dropped) and panic
/// (unwinding). Declared after the mock so that it is dropped first; the mock's
/// own Drop joins threads that wait on the gate.
struct ReleaseOnDrop(ResponseGate);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

pub async fn run(cell: &str) -> io::Result<()> {
    match cell {
        "classify_control_t" => return controls::classify().await,
        "harness_timeout_control_t" | "harness_panic_control_t" => {
            return controls::recovery(cell).await;
        }
        _ => {}
    }
    let spec = spec(cell).ok_or_else(|| io::Error::other(format!("unknown cell {cell}")))?;
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize()?;
    let (project, home, tmp) = (root.join("project"), root.join("home"), root.join("tmp"));
    for path in [&project, &home, &tmp] {
        fs::create_dir_all(path)?;
    }
    let artifact = test_support::product_artifact::require_from_env();
    let (gate, arrivals) = ResponseGate::new();
    let hold =
        || ScriptedResponse::text_stream("hold", "mock-model", ["held"]).with_gate(gate.clone());
    let mut env: Vec<(&str, OsString)> = vec![
        ("HOME", home.clone().into()),
        ("TMPDIR", tmp.into()),
        ("XDG_STATE_HOME", dir.sockets().join("state").into()),
        ("XDG_RUNTIME_DIR", dir.sockets().into()),
        ("XDG_CACHE_HOME", root.join("cache").into()),
        ("B3_TEST_KEY", "offline-key".into()),
    ];
    let mut fixture = None;
    let mut renderer: Option<PathBuf> = None;
    let (responses, browser_config) = match spec.engine {
        Engine::Chromium => {
            let chrome = fixtures::require_absolute_file("CHROME")?;
            let served = tls::Fixture::start(spec.mode == Mode::Navigating).await?;
            // The cell's own HOME is the only trust the product Chromium gets.
            fixtures::trust_ca(&root, &home, &served.ca_pem).await?;
            let url = served.url();
            fixture = Some(served);
            (
                vec![
                    ScriptedResponse::tool_call(
                        "nav",
                        "mock-model",
                        0,
                        "navigate-call",
                        "browser_navigate",
                        [json!({ "url": url }).to_string()],
                    ),
                    // The product reviews every URL-bearing web tool call with
                    // a model before it runs: this is that reviewer's answer.
                    ScriptedResponse::text_stream("review", "mock-model", [REVIEW_APPROVAL]),
                    hold(),
                ],
                format!(
                    "[tools.browser]\nenabled = true\nexecutable = \"{}\"\n",
                    chrome.display()
                ),
            )
        }
        Engine::Cef => {
            let real = fixtures::require_absolute_file("EVORCH_RENDERER_EXE")?;
            check(
                real.with_file_name("evorch-cef-helper").is_file(),
                "evorch-cef-helper must sit beside the renderer",
            )?;
            // In `Navigating` the page script first makes its renderer
            // process hold SCRIPT_MIB of touched memory (the observable that
            // the page is being loaded) and then never ends, so the load
            // cannot complete while the core is killed.
            let script = if spec.mode == Mode::Navigating {
                "<script>var keep=[];for(var i=0;i<6;i++){var a=new Uint8Array(64*1024*1024);a.fill(i+1);keep.push(a);}while(true){}</script>"
            } else {
                ""
            };
            fs::write(
                project.join("page.html"),
                format!(
                    "<!doctype html><html><body><h1>{CEF_HEADING}</h1><button>Add</button>{script}</body></html>"
                ),
            )?;
            let exec = if spec.mode == Mode::LaunchFailure {
                fixtures::failing_renderer(&root)?
            } else {
                real.clone()
            };
            env.push(("EVORCH_RENDERER_EXEC", exec.into()));
            if spec.mode == Mode::InitFailure {
                env.push(("LD_LIBRARY_PATH", fixtures::cef_link_only(&root, &real)?));
            }
            renderer = Some(real.canonicalize()?);
            (
                vec![
                    ScriptedResponse::tool_call(
                        "capture",
                        "mock-model",
                        0,
                        "capture-call",
                        "render_artifact",
                        [json!({"path": "page.html", "title": "page"}).to_string()],
                    ),
                    ScriptedResponse::text_stream("captured", "mock-model", ["captured"]),
                ],
                "[tools.browser]\nenabled = false\n".to_owned(),
            )
        }
    };
    let mock =
        StreamingMockOpenAi::spawn_with_prompt_cache_models(responses, vec!["mock-model".into()]);
    let _release = ReleaseOnDrop(gate.clone());
    fixtures::write_config(&root, &project, &mock.base_url(), &browser_config)?;
    let mut child = core::spawn(&artifact.exe, &root, &project, &env)?;
    let mut tree = ProcessTree::new(child.id().unwrap() as i32)?;
    let arrival = tokio::task::spawn_blocking(move || arrivals.recv());
    let marker = root.to_str().unwrap().to_owned();

    let result = async {
        let mut core = core::connect(&mut child).await?;
        match spec.engine {
            Engine::Chromium => {
                core::create_execution(&core, "b3", "navigate").await?;
            }
            Engine::Cef => {
                let run = core::create_execution(&core, "b3", "capture").await?;
                core::until_waiting(&mut core, &run).await?;
                let id = artifact_id(&mock)?;
                let mut turn = vec![ScriptedResponse::tool_call(
                    "open",
                    "mock-model",
                    0,
                    "open-call",
                    "inspect_artifact",
                    [json!({"action": "open", "artifact_id": id}).to_string()],
                )];
                if spec.mode == Mode::Complete {
                    turn.push(ScriptedResponse::tool_call(
                        "snapshot",
                        "mock-model",
                        0,
                        "snapshot-call",
                        "inspect_artifact",
                        [json!({"action": "snapshot"}).to_string()],
                    ));
                }
                turn.push(hold());
                mock.enqueue(turn);
                core::send_chat(&core, "b3", "second", "open and read it").await?;
            }
        }
        // The event that fixes the moment of the injection.
        match (spec.mode, spec.engine) {
            (Mode::Navigating, Engine::Chromium) => {
                let served = fixture.as_ref().unwrap();
                bounded(core::LIMIT, "navigation request reached the fixture", async {
                    served.arrived.notified().await;
                    Ok(())
                })
                .await?;
            }
            (Mode::Navigating, Engine::Cef) => {
                wait_for_page_script(&mut tree, renderer.as_deref().unwrap()).await?;
            }
            _ => {
                // The held response arrives only after the tool result of the
                // step reached the provider.
                bounded(core::LIMIT, "tool result delivered (held response arrived)", async {
                    arrival
                        .await
                        .map_err(io::Error::other)?
                        .map_err(io::Error::other)
                })
                .await?;
            }
        }
        let results = tool_results(&mock);
        let needle = if spec.engine == Engine::Chromium {
            tls::HEADING
        } else {
            CEF_HEADING
        };
        let shown = results.iter().any(|text| text.contains(needle));
        let mut requests = Value::Null;
        if spec.engine == Engine::Chromium {
            let served = fixture.as_ref().unwrap().requests.lock().unwrap().clone();
            check(
                served.iter().any(|line| line.starts_with("GET /b3-08")),
                format!("the HTTPS fixture never received the request: {served:?}"),
            )?;
            requests = json!(served);
        }
        match spec.mode {
            Mode::Complete => check(
                shown,
                format!("tool result does not show the loaded page: {results:?}"),
            )?,
            Mode::Navigating => check(
                !shown,
                format!("the navigation completed before the core was killed: {results:?}"),
            )?,
            Mode::LaunchFailure | Mode::InitFailure => {
                check(
                    !shown
                        && results.last().is_some_and(|text| {
                            text.contains("renderer is unavailable")
                                || text.contains("renderer IPC failed")
                        }),
                    format!("expected the product to report the renderer failure: {results:?}"),
                )?;
                check(
                    tree.owner.unchanged(),
                    "the core did not survive the renderer failure",
                )?;
                if spec.mode == Mode::LaunchFailure {
                    check(
                        root.join("renderer-launched").is_file(),
                        "the failing renderer was never launched",
                    )?;
                }
            }
        }
        tree.refresh()?;
        let failure_cell = matches!(spec.mode, Mode::LaunchFailure | Mode::InitFailure);
        let (members, root_pid, guard_json, guard) = if failure_cell {
            let all = tree.descendants.keys().copied().collect::<Vec<_>>();
            (all, None, Value::Null, None)
        } else {
            let root_pid = find_target_root(spec.engine, &tree, renderer.as_deref(), &home)?;
            let (guard, guard_json) = procs::guard_owner(&tree, root_pid)?;
            (
                procs::subtree(&tree, guard),
                Some(root_pid),
                guard_json,
                Some(guard),
            )
        };
        let described = procs::describe(&tree, &members, root_pid.or(members.first().copied()))?;
        let types: Vec<Option<String>> = members
            .iter()
            .map(|pid| procs::process_type(&procs::cmdline(*pid)).map(str::to_owned))
            .collect();
        let count = |kind: &str| types.iter().filter(|t| t.as_deref() == Some(kind)).count();
        if spec.mode == Mode::Complete {
            // A renderer forked from the zygote keeps the zygote's argv, so
            // CEF's renderers are only visible as extra zygote-typed members;
            // the page content itself is shown by the tool result above.
            if spec.engine == Engine::Chromium {
                check(
                    count("renderer") >= 1 && count("gpu-process") + count("utility") >= 1,
                    format!("Chromium has no renderer/service processes: {types:?}"),
                )?;
            } else {
                check(
                    count("zygote") >= 3 && count("utility") >= 1,
                    format!("CEF has no zygote-forked/utility processes: {types:?}"),
                )?;
            }
        }
        if spec.mode == Mode::Complete && spec.engine == Engine::Cef {
            // Control of the navigating observable: with no script, no such
            // member is anywhere near the threshold.
            let peak = members
                .iter()
                .filter(|pid| Some(**pid) != root_pid && Some(**pid) != guard)
                .filter_map(|pid| procs::rss_kib(*pid))
                .max()
                .unwrap_or(0);
            check(
                peak < PAGE_RSS_KIB,
                format!("a member holds {peak} KiB without any page script: the navigating observable would not discriminate"),
            )?;
        }
        // Death comes from the pidfd only. A process whose parent, group or
        // session changed is still alive and is still required to die.
        let classified = procs::classify(&tree, &members).await?;
        if let (Some(root_pid), Some(guard)) = (root_pid, guard) {
            check(
                classified.alive.contains(&root_pid)
                    && classified.alive.contains(&guard)
                    && classified.alive.len() >= 3,
                format!("target tree died before core KILL: {classified:?}"),
            )?;
        }
        let alive = classified.alive.clone();
        let marked = procs::unreachable_marked(&tree, &marker)?;
        // Fault injection of the `_stopped_` cells: a stopped process cannot
        // notice that its browser or host died and exit by itself, so only a
        // kill that reaches it ends it. The root and the guard keep running.
        let paused: Vec<i32> = if spec.stopped {
            alive
                .iter()
                .copied()
                .filter(|pid| Some(*pid) != root_pid && Some(*pid) != guard)
                .collect()
        } else {
            Vec::new()
        };
        for pid in &paused {
            tree.require(*pid)?.signal(Signal::STOP)?;
        }
        evidence::emit(
            cell,
            "ready",
            &json!({
                "cell": cell, "artifact": "P*", "core_owner": true,
                "injection": match spec.mode {
                    Mode::Complete => "none (navigation complete)",
                    Mode::Navigating => "core killed while the navigation is in flight",
                    Mode::LaunchFailure => "external executable replaces the renderer (exits at once)",
                    Mode::InitFailure => "real renderer, CEF resources unavailable (libcef.so link only)",
                },
                "owner": tree.owner.identity, "guard": guard_json, "target_root": root_pid,
                "source_sha": artifact.manifest.source_sha,
                "core_sha256": artifact.manifest.sha256,
                "guard_sha256": artifact.manifest.guard.as_ref().unwrap().sha256,
                "fixture_requests": requests,
                "failure_tool_result": if failure_cell { results.last().map(|t| t.chars().take(300).collect::<String>()) } else { None },
                "members": described, "exited_before_kill": classified.gone,
                "relation_changed_but_alive": classified.relation_changed,
                "stopped_before_kill": paused,
                "unreachable_marked": marked.iter().map(procs::describe_pin).collect::<Vec<_>>(),
            }),
        )?;
        if let Ok(kind) = std::env::var("EVORCH_B3_FAILURE") {
            // Harness failure injection (controls.rs), at the point where the
            // provider's held response is still waiting at its gate.
            let mut held: Vec<_> = alive
                .iter()
                .map(|pid| tree.require(*pid).map(|pin| pin.identity.clone()))
                .collect::<io::Result<_>>()?;
            held.push(tree.owner.identity.clone());
            println!("{}", json!({ "held": held }));
            // The controller pins every held process, then acknowledges on
            // stdin; the failure is injected only after that.
            bounded(core::LIMIT, "controller acknowledgement", async {
                use tokio::io::AsyncBufReadExt;
                let mut ack = String::new();
                tokio::io::BufReader::new(tokio::io::stdin())
                    .read_line(&mut ack)
                    .await?;
                Ok(())
            })
            .await?;
            if kind == "timeout" {
                std::future::pending::<()>().await;
            }
            panic!("injected harness panic");
        }
        tree.owner.signal(Signal::KILL)?;
        bounded(core::LIMIT, "core death", tree.owner.exited()).await?;
        tree.expect_dead(&alive, EXIT).await?;
        for pin in &marked {
            bounded(EXIT, "unreachable marked process death", pin.exited()).await?;
        }
        use std::os::unix::process::ExitStatusExt;
        let status = bounded(EXIT, "core reap", async { child.wait().await }).await?;
        check(
            status.signal() == Some(9),
            format!("core not SIGKILL: {status}"),
        )?;
        evidence::emit(
            cell,
            "death",
            &json!({
                "cell": cell, "core_owner": true, "core_signal": 9,
                "pidfd_dead": alive,
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
        eprintln!("tool results: {:?}", tool_results(&mock));
        for (index, request) in mock.recorded_requests().iter().enumerate() {
            let tail = request.body["messages"].as_array().map(|rows| {
                rows.iter()
                    .filter(|row| row["role"] != "system")
                    .map(|row| row.to_string().chars().take(400).collect::<String>())
                    .collect::<Vec<_>>()
            });
            eprintln!("mock request {index} {}: {tail:?}", request.path);
        }
    }
    // Independent backstop: kill by pidfd, reap, discover adopted descendants,
    // each bounded. The provider's gate is released by `_release` before the
    // mock is dropped, so no helper thread outlives the cell.
    if tree.owner.unchanged() {
        let _ = tree.refresh();
    }
    let cleanup = tree.cleanup(EXIT).await;
    let _ = bounded(EXIT, "core final reap", async { child.wait().await }).await;
    result.and(cleanup)
}

/// The page of `inspect_artifact open` is being loaded and its script runs:
/// a member of the guard subtree, other than the target root and the guard,
/// holds the memory the script touches (`PAGE_RSS_KIB`). Launching the
/// renderer or the engine's helpers does not reach it, so this cannot be
/// satisfied before the core has had the artifact opened. The core's spawn has
/// no event an external observer can await, so the tree is refreshed until
/// the observation holds; the bound only judges failure.
async fn wait_for_page_script(tree: &mut ProcessTree, renderer: &Path) -> io::Result<()> {
    bounded(
        core::LIMIT,
        "the artifact page's script is running",
        async {
            loop {
                if tree.refresh().is_ok()
                    && let Ok(root) =
                        find_target_root(Engine::Cef, tree, Some(renderer), Path::new(""))
                    && let Ok((guard, _)) = procs::guard_owner(tree, root)
                    && procs::subtree(tree, guard)
                        .into_iter()
                        .filter(|pid| *pid != root && *pid != guard)
                        .any(|pid| procs::rss_kib(pid).is_some_and(|rss| rss >= PAGE_RSS_KIB))
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        },
    )
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
            "product Chromium does not run with the cell's HOME (its NSS store would not be the cell's)",
        )?;
    }
    Ok(root)
}
