//! The three measured cells. Measurement code lives only here: the products
//! are driven through their public entrances and nothing is added to them.
use super::{
    arm::{CoreArm, parent_of},
    gate::{GuardPss, KillSample, PairSample, Spec},
};
use sandbox::owned::{GuardRuntime, OwnedChild, OwnedCommand};
use serde_json::{Value, json};
use std::{
    io,
    path::Path,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

fn read(path: &str) -> io::Result<String> {
    std::fs::read_to_string(path).map(|s| s.trim().to_owned())
}

/// One probe: a user message whose reply is a shell tool call. The RTT is the
/// controller clock from "tool call written to the core" to "request that
/// carries the tool result arrived", so it covers the core's tool dispatch,
/// the real `sh -c`, output handling and the result's way back.
async fn probe(arm: &CoreArm, workload: usize, kind: char, n: u32) -> io::Result<u64> {
    let marker = format!("perf:probe:{workload}:{kind}{n}");
    let mark = arm
        .turn(&arm.probe_thread(), &marker, format!("{marker} run"), 1)
        .await?;
    // A sample is only valid if the shell really ran the workload: exit code 0,
    // and `grep -F -- D-7` must have printed its matching lines.
    let result = mark.tool_result.as_deref().unwrap_or_default();
    let ran = result.starts_with("exit_code: 0\n") && (workload != 0 || result.contains("D-7"));
    if !ran {
        return Err(io::Error::other(format!(
            "{marker}: the shell workload did not run as intended: {:?}",
            result.chars().take(200).collect::<String>()
        )));
    }
    match (mark.tool_sent, mark.followup) {
        (Some(sent), Some(back)) if back > sent => Ok(back.duration_since(sent).as_nanos() as u64),
        other => Err(io::Error::other(format!(
            "{marker}: unusable stamps {other:?}"
        ))),
    }
}

/// Warm-up, then `pairs` paired samples per workload, always A then B (ABAB).
/// Samples are appended as they are taken, so a failure keeps what was measured.
pub async fn latency(
    spec: &Spec,
    a: &Arc<CoreArm>,
    b: &Arc<CoreArm>,
    out: &mut Vec<PairSample>,
) -> io::Result<()> {
    for (w, workload) in spec.workloads.iter().enumerate() {
        for i in 0..spec.warmup_per_arm {
            probe(a, w, 'w', i).await?;
            probe(b, w, 'w', i).await?;
        }
        for index in 0..spec.pairs {
            let a_ns = probe(a, w, 'm', index).await?;
            let b_ns = probe(b, w, 'm', index).await?;
            out.push(PairSample {
                workload: workload.name.clone(),
                index,
                a_ns,
                b_ns,
            });
        }
    }
    Ok(())
}

// ------------------------------------------------------------ guard owners

fn runtime(bundle: &Path) -> io::Result<GuardRuntime> {
    GuardRuntime::from_bundle(bundle)
}

fn owned(script: &str, runtime: &GuardRuntime) -> OwnedCommand {
    let mut command = OwnedCommand::new("/bin/sh");
    command
        .args(["-c", script])
        .guard_runtime(runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// ForceKill request -> root exit event. The root ignores TERM and has a
/// same-group child, like a stuck tool; every sample is a fresh owned root.
pub async fn force_kill(spec: &Spec, bundle: &Path, out: &mut Vec<KillSample>) -> io::Result<()> {
    let runtime = runtime(bundle)?;
    let budget = Duration::from_nanos(spec.force_kill.observe_budget_ns);
    for index in 0..spec.force_kill.samples_min {
        let mut child = OwnedChild::spawn(&mut owned("trap '' TERM; sleep 300 & wait", &runtime))?;
        let started = Instant::now();
        child.kill()?;
        let seen = tokio::time::timeout(budget, child.exited()).await;
        let elapsed = started.elapsed();
        let ns = match seen {
            Ok(Ok(())) => Some(elapsed.as_nanos() as u64),
            Ok(Err(error)) => return Err(error),
            Err(_) => None,
        };
        child.wait().await?;
        out.push(KillSample { index, ns });
    }
    Ok(())
}

fn pss_kib(pid: u32) -> Option<u64> {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("Pss:"))
        .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse().ok())
}

/// `guards` owned targets that only sleep; after `hold` the PSS of each
/// guard process (the target's parent) is read. The owner here is this
/// runner: its own and the targets' PSS are recorded apart, never judged.
pub async fn guard_pss(
    guards: u32,
    hold: Duration,
    bundle: &Path,
) -> io::Result<(Vec<GuardPss>, Value, u32)> {
    let runtime = runtime(bundle)?;
    let mut children = Vec::new();
    for _ in 0..guards {
        children.push(OwnedChild::spawn(&mut owned("exec sleep 3600", &runtime))?);
    }
    let held = Instant::now();
    tokio::time::sleep(hold).await;
    let held_secs = held.elapsed().as_secs() as u32;
    let own = std::process::id();
    let mut readings = Vec::new();
    let mut detail = Vec::new();
    for child in &children {
        let target = child
            .id()
            .ok_or_else(|| io::Error::other("owned target already gone"))?;
        let guard = parent_of(target).ok_or_else(|| io::Error::other("target has no parent"))?;
        let comm = read(&format!("/proc/{guard}/comm")).unwrap_or_default();
        if parent_of(guard) != Some(own) {
            return Err(io::Error::other(format!(
                "guard {guard} ({comm}) is not a child of the owner"
            )));
        }
        let pss = pss_kib(guard);
        readings.push(GuardPss {
            pid: guard,
            pss_kib: pss,
        });
        detail.push(
            json!({"guard": guard, "comm": comm, "guard_pss_kib": pss, "target": target,
            "target_pss_kib": pss_kib(target)}),
        );
    }
    let observations = json!({"owner_pss_kib": pss_kib(own), "guards": detail});
    for mut child in children {
        child.kill()?;
        child.wait().await?;
    }
    Ok((readings, observations, held_secs))
}
