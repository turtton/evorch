//! The two cells. Chromium and CEF are separate paths: neither stands in for
//! the other. Both start the sealed product core through its own entrance, are
//! driven through the public protocol and a scripted model provider, wait for
//! an explicit event that proves the navigation completed, then SIGKILL only
//! the core and require every observed descendant to die before any cleanup.
use super::{
    core_process as core, evidence, fixtures,
    observer::{ProcessTree, bounded},
    procs::{self, check},
    tls,
};
use mock_openai::{ResponseGate, ScriptedResponse, StreamingMockOpenAi};
use rustix::process::Signal;
use serde_json::{Value, json};
use std::{ffi::OsString, fs, io, time::Duration};

const EXIT: Duration = Duration::from_secs(30);
const CEF_HEADING: &str = "B3-08 CEF fixture";
const REVIEW_APPROVAL: &str = r#"{"approve":true,"reason":"requested navigation","risk_level":"low","procedure_step":"none","authorization_basis":"ordinary_task","authorization_refs":[0],"scope_match":"full","authorization_level":"medium"}"#;

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

pub async fn run(cell: &str) -> io::Result<()> {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize()?;
    let (project, home, tmp) = (root.join("project"), root.join("home"), root.join("tmp"));
    for path in [&project, &home, &tmp] {
        fs::create_dir_all(path)?;
    }
    let artifact = test_support::product_artifact::require_from_env();
    let (gate, arrivals) = ResponseGate::new();
    let mut env: Vec<(&str, OsString)> = vec![
        ("HOME", home.clone().into()),
        ("TMPDIR", tmp.into()),
        ("XDG_STATE_HOME", dir.sockets().join("state").into()),
        ("XDG_RUNTIME_DIR", dir.sockets().into()),
        ("XDG_CACHE_HOME", root.join("cache").into()),
        ("B3_TEST_KEY", "offline-key".into()),
    ];
    let mut fixture = None;
    let mut renderer = None;
    let (responses, browser_config) = match cell {
        "chromium_p" | "chromium_stopped_p" => {
            let chrome = fixtures::require_absolute_file("CHROME")?;
            let served = tls::Fixture::start().await?;
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
                    ScriptedResponse::text_stream("hold", "mock-model", ["held"])
                        .with_gate(gate.clone()),
                ],
                format!(
                    "[tools.browser]\nenabled = true\nexecutable = \"{}\"\n",
                    chrome.display()
                ),
            )
        }
        "cef_p" | "cef_stopped_p" => {
            let exe = fixtures::require_absolute_file("EVORCH_RENDERER_EXE")?;
            let helper = exe.with_file_name("evorch-cef-helper");
            check(
                helper.is_file(),
                "evorch-cef-helper must sit beside the renderer",
            )?;
            fs::write(
                project.join("page.html"),
                format!(
                    "<!doctype html><html><body><h1>{CEF_HEADING}</h1><button>Add</button></body></html>"
                ),
            )?;
            env.push(("EVORCH_RENDERER_EXEC", exe.clone().into()));
            renderer = Some(exe.canonicalize()?);
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
        other => return Err(io::Error::other(format!("unknown cell {other}"))),
    };
    let mock =
        StreamingMockOpenAi::spawn_with_prompt_cache_models(responses, vec!["mock-model".into()]);
    fixtures::write_config(&root, &project, &mock.base_url(), &browser_config)?;
    let mut child = core::spawn(&artifact.exe, &root, &project, &env)?;
    let mut tree = ProcessTree::new(child.id().unwrap() as i32)?;
    let arrival = tokio::task::spawn_blocking(move || arrivals.recv());
    let marker = root.to_str().unwrap().to_owned();

    let result = async {
        let mut core = core::connect(&mut child).await?;
        match cell {
            "chromium_p" | "chromium_stopped_p" => {
                core::create_execution(&core, "b3", "navigate").await?;
            }
            _ => {
                let run = core::create_execution(&core, "b3", "capture").await?;
                core::until_waiting(&mut core, &run).await?;
                let id = artifact_id(&mock)?;
                mock.enqueue([
                    ScriptedResponse::tool_call(
                        "open",
                        "mock-model",
                        0,
                        "open-call",
                        "inspect_artifact",
                        [json!({"action": "open", "artifact_id": id}).to_string()],
                    ),
                    ScriptedResponse::tool_call(
                        "snapshot",
                        "mock-model",
                        0,
                        "snapshot-call",
                        "inspect_artifact",
                        [json!({"action": "snapshot"}).to_string()],
                    ),
                    ScriptedResponse::text_stream("hold", "mock-model", ["held"])
                        .with_gate(gate.clone()),
                ]);
                core::send_chat(&core, "b3", "second", "open and read it").await?;
            }
        }
        // The held response arrives only after the tool result of the
        // navigation reached the provider: the navigation is complete.
        bounded(core::LIMIT, "navigation complete (held response arrived)", async {
            arrival
                .await
                .map_err(io::Error::other)?
                .map_err(io::Error::other)
        })
        .await?;
        let results = tool_results(&mock);
        let (needle, requests) = match cell {
            "chromium_p" | "chromium_stopped_p" => {
                let served = fixture.as_ref().unwrap().requests.lock().unwrap().clone();
                check(
                    served.iter().any(|line| line.starts_with("GET /b3-08")),
                    format!("the HTTPS fixture never received the request: {served:?}"),
                )?;
                (tls::HEADING, json!(served))
            }
            _ => (CEF_HEADING, Value::Null),
        };
        check(
            results.iter().any(|text| text.contains(needle)),
            format!("tool result does not show the loaded page: {results:?}"),
        )?;
        tree.refresh()?;
        let root_pid = find_target_root(cell, &tree, renderer.as_deref(), &home)?;
        let (guard, guard_json) = procs::guard_owner(&tree, root_pid)?;
        let members = procs::subtree(&tree, guard);
        let described = procs::describe(&tree, &members, root_pid)?;
        let types: Vec<Option<String>> = members
            .iter()
            .map(|pid| procs::process_type(&procs::cmdline(*pid)).map(str::to_owned))
            .collect();
        let count = |kind: &str| types.iter().filter(|t| t.as_deref() == Some(kind)).count();
        // A renderer forked from the zygote keeps the zygote's argv, so CEF's
        // renderers are only visible as extra zygote-typed members; the page
        // content itself is shown by the tool result checked above.
        if cell.starts_with("chromium") {
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
        let (alive, gone): (Vec<i32>, Vec<i32>) = members
            .iter()
            .copied()
            .partition(|pid| tree.require(*pid).is_ok_and(|pin| pin.unchanged()));
        check(
            alive.contains(&root_pid) && alive.contains(&guard) && alive.len() >= 3,
            format!("target tree changed before core KILL: alive {alive:?} gone {gone:?}"),
        )?;
        let marked = procs::unreachable_marked(&tree, &marker)?;
        // Fault injection of the `_stopped_` cells: a stopped process cannot
        // notice that its browser or host died and exit by itself, so only a
        // kill that reaches it ends it. The root and the guard keep running.
        let paused: Vec<i32> = if cell.contains("_stopped_") {
            alive
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
        evidence::emit(
            cell,
            "ready",
            &json!({
                "cell": cell, "artifact": "P*", "core_owner": true,
                "entrance": if cell.starts_with("chromium") {
                    "Conversation CreateExecution -> agent tool browser_navigate -> real Chromium (https loopback, NSS-trusted CA)"
                } else {
                    "Conversation turns -> render_artifact -> inspect_artifact open/snapshot -> evorch-renderer + real CEF"
                },
                "owner": tree.owner.identity, "guard": guard_json, "target_root": root_pid,
                "source_sha": artifact.manifest.source_sha,
                "core_sha256": artifact.manifest.sha256,
                "guard_sha256": artifact.manifest.guard.as_ref().unwrap().sha256,
                "fixture_requests": requests,
                "members": described, "exited_before_kill": gone, "stopped_before_kill": paused,
                "unreachable_marked": marked.iter().map(procs::describe_pin).collect::<Vec<_>>(),
            }),
        )?;
        tree.owner.signal(Signal::KILL)?;
        bounded(core::LIMIT, "core death", tree.owner.exited()).await?;
        tree.expect_dead(&alive, EXIT).await?;
        for pin in &marked {
            bounded(EXIT, "unreachable marked process death", pin.exited()).await?;
        }
        use std::os::unix::process::ExitStatusExt;
        let status = bounded(EXIT, "core reap", async { child.wait().await }).await?;
        check(status.signal() == Some(9), format!("core not SIGKILL: {status}"))?;
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
    // each bounded; then the provider is released and dropped so that no
    // helper thread outlives the cell.
    gate.release();
    if tree.owner.unchanged() {
        let _ = tree.refresh();
    }
    let cleanup = tree.cleanup(EXIT).await;
    let _ = bounded(EXIT, "core final reap", async { child.wait().await }).await;
    drop(gate);
    drop(mock);
    result.and(cleanup)
}

fn find_target_root(
    cell: &str,
    tree: &ProcessTree,
    renderer: Option<&std::path::Path>,
    home: &std::path::Path,
) -> io::Result<i32> {
    let mut found: Vec<i32> = tree
        .descendants
        .values()
        .map(|pin| pin.identity.pid)
        .filter(|pid| {
            let args = procs::cmdline(*pid);
            if cell.starts_with("chromium") {
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
    if cell.starts_with("chromium") {
        let environ = fs::read(format!("/proc/{root}/environ"))?;
        let expected = format!("HOME={}", home.display());
        check(
            environ.split(|b| *b == 0).any(|e| e == expected.as_bytes()),
            "product Chromium does not run with the cell's HOME (its NSS store would not be the cell's)",
        )?;
    }
    Ok(root)
}
