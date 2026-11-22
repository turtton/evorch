//! The three measured cells. Measurement code lives only here: the products
//! are driven through their public entrances and nothing is added to them.
use super::{
    arm::{CoreArm, parent_of},
    gate::{Arm, GuardPss, KillSample, PairSample, ScopeReadback, Spec, expected_first},
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

fn limit(text: &str) -> u64 {
    text.parse().unwrap_or(u64::MAX)
}

fn command_line(exe: &str, arg: &str) -> Option<String> {
    let out = std::process::Command::new(exe).arg(arg).output().ok()?;
    let text = [out.stdout, out.stderr].concat();
    String::from_utf8_lossy(&text)
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_owned())
}

/// What the kernel enforces on the cgroup this process runs in, and the
/// stress-ng that runs next to it (the scope script starts it inside).
pub fn scope_readback() -> io::Result<ScopeReadback> {
    let own = read("/proc/self/cgroup")?;
    let path = own
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| io::Error::other("not in a cgroup v2 hierarchy"))?;
    let dir = format!("/sys/fs/cgroup{path}");
    let mut stress_ng = None;
    for pid in read(&format!("{dir}/cgroup.procs"))
        .unwrap_or_default()
        .lines()
    {
        let comm = read(&format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.contains("stress-ng")
            && let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe"))
        {
            let exe = exe.display().to_string();
            let version = command_line(&exe, "--version").unwrap_or_default();
            stress_ng = Some(format!("{exe} | {version}"));
            break;
        }
    }
    Ok(ScopeReadback {
        cpu_max: read(&format!("{dir}/cpu.max"))?,
        memory_max: limit(&read(&format!("{dir}/memory.max"))?),
        swap_max: limit(&read(&format!("{dir}/memory.swap.max"))?),
        pids_max: limit(&read(&format!("{dir}/pids.max"))?),
        stress_ng,
    })
}

// ------------------------------------------------------------------ latency

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

/// Warm-up, then `pairs` paired samples per workload in the frozen ABBA order.
/// Samples are appended as they are taken, so a failure keeps what was measured.
pub async fn latency(
    spec: &Spec,
    round: u32,
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
            let first = expected_first(spec.seed, round, w, index);
            let (mut a_ns, mut b_ns) = (0, 0);
            for arm in [first, if first == Arm::A { Arm::B } else { Arm::A }] {
                if arm == Arm::A {
                    a_ns = probe(a, w, 'm', index).await?;
                } else {
                    b_ns = probe(b, w, 'm', index).await?;
                }
            }
            out.push(PairSample {
                workload: workload.name.clone(),
                index,
                first,
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
