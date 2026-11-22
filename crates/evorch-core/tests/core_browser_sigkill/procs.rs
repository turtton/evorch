//! Reading and classifying the processes the independent observer pinned.
use super::observer::{Identity, PinnedProcess, ProcessTree};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, io};

pub fn check(ok: bool, what: impl Into<String>) -> io::Result<()> {
    if ok {
        Ok(())
    } else {
        Err(io::Error::other(what.into()))
    }
}

pub fn cmdline(pid: i32) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

pub fn exe(pid: i32) -> Option<std::path::PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// `--type=` of a Chromium/CEF child process; `None` for the browser process.
pub fn process_type(args: &[String]) -> Option<&str> {
    args.iter()
        .flat_map(|a| a.split_whitespace())
        .find_map(|a| a.strip_prefix("--type="))
}

/// The guard that owns `root`: the ancestor that is a direct child of the
/// core and was started as `--launch`/`--launch-pty` for exactly this core.
pub fn guard_owner(tree: &ProcessTree, root: i32) -> io::Result<(i32, Value)> {
    let mut pid = root;
    loop {
        let pin = tree.require(pid)?;
        if pin.identity.parent == tree.owner.identity.pid {
            let args = cmdline(pid);
            check(
                args.get(1)
                    .is_some_and(|a| a == "--launch" || a == "--launch-pty"),
                format!("not guard-owned: {args:?}"),
            )?;
            check(
                args.get(4) == Some(&tree.owner.identity.pid.to_string()),
                "guard owner is not the launched core",
            )?;
            return Ok((
                pid,
                json!({"identity":pin.identity,"mode":args[1],"owner_pid":args[4],"target_root":root}),
            ));
        }
        pid = pin.identity.parent;
    }
}

/// The guard and everything the observer reached below it.
pub fn subtree(tree: &ProcessTree, guard: i32) -> Vec<i32> {
    let mut set = BTreeSet::from([guard]);
    loop {
        let before = set.len();
        for pin in tree.descendants.values() {
            if set.contains(&pin.identity.parent) {
                set.insert(pin.identity.pid);
            }
        }
        if set.len() == before {
            return set.into_iter().collect();
        }
    }
}

/// One row per process, with its relation to the target root: the same
/// process group (guaranteed) or an escaped one (reported, never dismissed).
pub fn describe(tree: &ProcessTree, pids: &[i32], root: Option<i32>) -> io::Result<Vec<Value>> {
    let root_identity = match root {
        Some(root) => Some(tree.require(root)?.identity.clone()),
        None => None,
    };
    pids.iter()
        .map(|pid| {
            let identity: &Identity = &tree.require(*pid)?.identity;
            let args = cmdline(*pid);
            Ok(json!({
                "identity": identity,
                "exe": exe(*pid).map(|p| p.display().to_string()),
                "type": process_type(&args),
                "crashpad_handler": is_crashpad_handler(&args),
                "arg0": args.first().map(|a| a.chars().take(160).collect::<String>()),
                "same_group_as_target_root": root_identity.as_ref().is_some_and(|r| identity.group == r.group),
                "same_session_as_target_root": root_identity.as_ref().is_some_and(|r| identity.session == r.session),
            }))
        })
        .collect()
}

/// Live processes whose command line carries the cell's private directory
/// that the owner-rooted graph did not reach (for example a daemonized
/// helper). They are pinned so their death can be awaited too.
pub fn unreachable_marked(tree: &ProcessTree, marker: &str) -> io::Result<Vec<PinnedProcess>> {
    let mut pins = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let Some(pid) = entry?
            .file_name()
            .to_str()
            .and_then(|v| v.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == tree.owner.identity.pid
            || pid == i32::try_from(std::process::id()).unwrap_or(-1)
            || tree.descendants.contains_key(&pid)
        {
            continue;
        }
        // Identity first, then the marker test, then a pin that re-reads and
        // compares the same identity: a PID reused in between cannot be pinned.
        let Ok(identity) = Identity::read(pid) else {
            continue;
        };
        if !cmdline(pid).iter().any(|a| a.contains(marker)) {
            continue;
        }
        if let Ok(pin) = PinnedProcess::pin(&identity) {
            pins.push(pin);
        }
    }
    Ok(pins)
}

pub fn is_crashpad_handler(args: &[String]) -> bool {
    args.first().is_some_and(|a| {
        a.split_whitespace()
            .next()
            .is_some_and(|p| p.ends_with("crashpad_handler"))
    })
}

/// Evidence row for a pinned process outside the owner-rooted graph.
pub fn describe_pin(pin: &PinnedProcess) -> Value {
    let args = cmdline(pin.identity.pid);
    json!({
        "identity": pin.identity,
        "exe": exe(pin.identity.pid).map(|p| p.display().to_string()),
        "crashpad_handler": is_crashpad_handler(&args),
        "arg0": args.first().map(|a| a.chars().take(160).collect::<String>()),
    })
}

/// A process is dead when its pidfd is readable; polled once, never waited.
pub async fn is_dead(pin: &PinnedProcess) -> bool {
    use std::{future::poll_fn, pin::pin, task::Poll};
    let mut exited = pin!(pin.exited());
    poll_fn(|cx| Poll::Ready(std::future::Future::poll(exited.as_mut(), cx).is_ready())).await
}

#[derive(Debug, Default)]
pub struct Classified {
    /// Alive by pidfd. A changed PPID/PGID/SID does not make a process dead.
    pub alive: Vec<i32>,
    /// Dead by pidfd.
    pub gone: Vec<i32>,
    /// Alive, but its parent/group/session no longer match the pinned identity.
    pub relation_changed: Vec<i32>,
}

/// Death comes only from the pidfd; a relationship change is reported apart.
pub async fn classify(tree: &ProcessTree, pids: &[i32]) -> io::Result<Classified> {
    let mut out = Classified::default();
    for pid in pids {
        let pin = tree.require(*pid)?;
        if is_dead(pin).await {
            out.gone.push(*pid);
        } else {
            out.alive.push(*pid);
            if !pin.unchanged() {
                out.relation_changed.push(*pid);
            }
        }
    }
    Ok(out)
}
