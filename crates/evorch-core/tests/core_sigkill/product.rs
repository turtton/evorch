use super::observer::{ProcessTree, bounded};
use rustix::process::Signal;
use serde_json::{Value, json};
use std::{fs, io, os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, net::UnixListener};

pub(super) const READY: Duration = Duration::from_secs(30);
pub(super) const EXIT: Duration = Duration::from_secs(30);

pub(super) fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
pub(super) fn script(path: &Path, body: &str) -> io::Result<()> {
    fs::write(path, format!("#!/bin/sh\n{body}\n"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}
pub(super) fn check(ok: bool, what: impl Into<String>) -> io::Result<()> {
    if ok {
        Ok(())
    } else {
        Err(io::Error::other(what.into()))
    }
}

pub async fn run(cell: &str) -> io::Result<()> {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize()?;
    let project = root.join("project");
    fs::create_dir(&project)?;
    // The replay's tool sandbox is rooted in its own workspace and does not show
    // the original project. It does mount the host's Cargo caches read-only,
    // so the replay's peer and its control socket live in one (see
    // evaluation::REPLAY_PEER_DIR).
    let peer_dir = if cell == "eval_cli" {
        let dir = root.join(super::evaluation::REPLAY_PEER_DIR);
        fs::create_dir_all(&dir)?;
        dir
    } else {
        project.clone()
    };
    let socket = peer_dir.join(".peer-socket");
    let listener = UnixListener::bind(&socket)?;
    let own = peer_dir.join(".peer-bin");
    fs::copy(std::env::current_exe()?, &own)?;
    if cell.ends_with("_t") {
        return super::t_controller::run(cell, &root, &own, &socket, &listener).await;
    }
    if cell.starts_with("observer") {
        return super::observer_cells::run(cell, &own, &socket, &listener).await;
    }
    let super::fixtures::Fixture { bindir, cli, mock } =
        super::fixtures::prepare(cell, &root, &project, &own, &socket).await?;
    let artifact = test_support::product_artifact::require_from_env();
    let trace = root.join("exec-trace");
    let mut command = super::trace::command(&artifact.exe, Some(trace.as_path()));
    command.env_clear();
    for name in ["PATH", "LD_LIBRARY_PATH"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let path = std::env::join_paths(
        std::iter::once(bindir.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    command
        .args(["--config-dir", root.to_str().unwrap(), "--foreground"])
        .env("PATH", path)
        .env("HOME", &root)
        .env("XDG_STATE_HOME", dir.sockets().join("state"))
        .env("XDG_RUNTIME_DIR", dir.sockets())
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("B3_TEST_KEY", "offline-key")
        .current_dir(&project)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .stdin(Stdio::null());
    if let Some(cli) = &cli {
        command.env("EVORCH_JS_WORKER", cli);
    }
    let mut child = command.spawn()?;
    let mut tree = ProcessTree::new(child.id().unwrap() as i32)?;
    let mut client_hold = None;
    let result = async {
        let peer = if !matches!(cell, "bwrap_probe" | "git_read") {
            let (peer, client) = super::entries::enter_caller(
                cell,
                child.stdout.take().unwrap(),
                &listener,
                &project,
            )
            .await?;
            client_hold = Some(client);
            peer
        } else {
            peer_ready(&listener).await?
        };
        tree.refresh()?;
        let root_identity = &tree.require(peer)?.identity;
        let guard = guard_owner(&tree, peer)?;
        let tracer = super::trace::tracer()?;
        check(
            tree.descendants
                .values()
                .any(|p| p.identity.parent == peer && p.identity.group == root_identity.group),
            "READY root missing its live same-PGID grandchild",
        )?;
        let mut trace_worker = None;
        let mut owned: Vec<_> = tree
            .descendants
            .values()
            .filter(|p| p.identity.group == root_identity.group)
            .map(|p| p.identity.pid)
            .collect();
        if cell == "js_worker" {
            let workers: Vec<_> = tree
                .descendants
                .values()
                .filter(|p| {
                    fs::read(format!("/proc/{}/cmdline", p.identity.pid)).is_ok_and(|args| {
                        args.split(|b| *b == 0).next()
                            == cli.as_ref().map(|p| p.as_os_str().as_encoded_bytes())
                    })
                })
                .collect();
            check(workers.len() == 1, "actual evorch js-worker missing")?;
            let worker = workers[0].identity.pid;
            check(
                !tree
                    .descendants
                    .values()
                    .any(|p| p.identity.parent == worker),
                "js-worker spawned another process",
            )?;
            super::trace::assert_no_process_spawn(&trace, worker, false)?;
            check(
                !root.join("bin/guard-bundle.json").exists(),
                "worker test accidentally supplies installed guard bundle",
            )?;
            super::evidence::emit(
                cell,
                "worker",
                &json!({
                    "worker":workers[0].identity,
                    "sha256":super::evidence::digest(Path::new(&format!("/proc/{worker}/exe")))?,
                    "no_installed_bundle":true,"host_call_peer":peer
                }),
            )?;
            trace_worker = Some(worker);
            owned.push(worker);
        }
        let mut cli_guard = None;
        if cell == "eval_cli" {
            let (pid, cli_owner, group) = super::evaluation::observe_cli(
                &tree,
                &bindir.join("evorch"),
                &bindir.join("evorch-core"),
                &mock.recorded_requests(),
            )?;
            let identity = &tree.require(pid)?.identity;
            super::evidence::emit(
                cell,
                "cli",
                &json!({
                    "cell":cell,"external_cli_fixture":false,"provider":"mock",
                    "cli":identity,"cli_guard":cli_owner,"cli_children":0,
                    "cli_sha256":super::evidence::digest(Path::new(&format!("/proc/{pid}/exe")))?,
                    "replay_peer_root":peer,
                }),
            )?;
            owned.extend(group);
            cli_guard = Some(cli_owner);
        }
        check(
            owned.len() >= 2,
            format!("missing same-PGID root/grandchild: {owned:?}"),
        )?;
        super::evidence::emit(
            cell,
            "ready",
            &json!({
                "cell":cell,"artifact":if matches!(cell,"bwrap_probe"|"git_read") {"P"} else {"P*"},
                "core_owner":true,"owner":tree.owner.identity,"guard":guard,
                "source_sha":artifact.manifest.source_sha,"core_sha256":artifact.manifest.sha256,
                "guard_sha256":artifact.manifest.guard.as_ref().unwrap().sha256,
                "tree":tree.descendants.values().map(|p| &p.identity).collect::<Vec<_>>()
            }),
        )?;
        for pid in &owned {
            check(
                tree.require(*pid)?.unchanged(),
                format!("target {pid} died/changed before core KILL"),
            )?;
        }
        tree.owner.signal(Signal::KILL)?;
        bounded(EXIT, "core death", tree.owner.exited()).await?;
        tree.expect_dead(&owned, EXIT).await?;
        use std::os::unix::process::ExitStatusExt;
        let status = child.wait().await?;
        check(
            status.signal() == Some(9),
            format!("core not SIGKILL: {status}"),
        )?;
        super::trace::finish(cell, &trace, trace_worker, &tracer, &guard).await?;
        if let Some(cli_guard) = &cli_guard {
            super::trace::finish("eval_cli_client", &trace, None, &tracer, cli_guard).await?;
        }
        super::evidence::emit(
            cell,
            "death",
            &json!({
                "cell":cell,"core_owner":true,"pidfd_dead":owned,"core_signal":9,
                "cleanup_not_started":true
            }),
        )?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tree.refresh();
        for pin in tree.descendants.values() {
            eprintln!(
                "live diagnostic {:?}: {:?}",
                pin.identity,
                fs::read_to_string(format!("/proc/{}/cmdline", pin.identity.pid))
                    .unwrap_or_default()
            );
        }
        if cell == "eval_cli" {
            for log in glob_logs(&root.join("eval/.groups")) {
                eprintln!(
                    "CLI log {}: {}",
                    log.display(),
                    fs::read_to_string(&log).unwrap_or_default()
                );
            }
        }
        for (index, request) in mock.recorded_requests().iter().enumerate().rev().take(3) {
            let messages = request.body["messages"].as_array().map(|rows| {
                rows.iter()
                    .filter(|v| v["role"] != "system")
                    .collect::<Vec<_>>()
            });
            eprintln!(
                "mock {index}: {}",
                serde_json::to_string(&messages)
                    .unwrap()
                    .chars()
                    .take(6000)
                    .collect::<String>()
            );
        }
    }
    if tree.owner.unchanged() {
        let _ = tree.refresh();
    }
    let cleanup = tree.cleanup(EXIT).await;
    let _ = child.wait().await;
    result.and(cleanup)
}

/// The CLI's redirected stdout/stderr files of every evaluation group.
fn glob_logs(groups: &Path) -> Vec<std::path::PathBuf> {
    let mut logs = Vec::new();
    for group in fs::read_dir(groups).into_iter().flatten().flatten() {
        for file in fs::read_dir(group.path()).into_iter().flatten().flatten() {
            if file.path().extension().is_some_and(|e| e == "log") {
                logs.push(file.path());
            }
        }
    }
    logs
}

pub(super) async fn peer_ready(listener: &UnixListener) -> io::Result<i32> {
    bounded(READY, "caller/descendant READY", async {
        let (mut stream, _) = listener.accept().await?;
        let mut line = [0; 6];
        stream.read_exact(&mut line).await?;
        check(line == *b"READY\n", "invalid peer READY")?;
        stream
            .peer_cred()?
            .pid()
            .ok_or_else(|| io::Error::other("peer has no PID"))
    })
    .await
}

pub(super) fn guard_owner(tree: &ProcessTree, root: i32) -> io::Result<Value> {
    let mut pid = root;
    let mut target_root = root;
    loop {
        let pin = tree.require(pid)?;
        if pin.identity.parent == tree.owner.identity.pid {
            let bytes = fs::read(format!("/proc/{pid}/cmdline"))?;
            let args = bytes
                .split(|b| *b == 0)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>();
            check(
                args.get(1)
                    .is_some_and(|a| a == "--launch" || a == "--launch-pty"),
                format!("not guard-owned: {args:?}"),
            )?;
            check(
                args.get(4) == Some(&tree.owner.identity.pid.to_string()),
                "guard owner is not the launched core",
            )?;
            return Ok(
                json!({"identity":pin.identity,"mode":args[1],"owner_pid":args[4],"target_root":target_root}),
            );
        }
        target_root = pid;
        pid = pin.identity.parent;
    }
}
