//! D-3a2 B3-09 performance gate runner and aggregator (measurement only).
//!
//! Subcommands (see crates/evorch-core/perf-gate/README.md):
//!   conditions   write conditions.json before any sample
//!   latency      one round: shell RTT of A and B, then ForceKill (runs in the scope)
//!   pss          one round: 100 guards held, PSS read (runs in the scope)
//!   stub         record a round the scope killed or that could not start
//!   aggregate    judge a result directory (nearest-rank, per round, no trimming)
//!   print-order  the frozen A/B order of one (round, workload)
//!
//! `--smoke` shrinks every count and marks the output `evidence: false`; such
//! a file can never pass the aggregator.
#[cfg(target_os = "linux")]
mod arm;
#[cfg(target_os = "linux")]
mod cells;
#[cfg(target_os = "linux")]
mod conditions;
#[cfg(target_os = "linux")]
mod fixture;
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../../shared-test-support/perf_gate/mod.rs"]
mod gate;
#[cfg(target_os = "linux")]
mod provider;

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the performance gate runs on Linux only");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{arm, cells, conditions, gate::*};
    use serde_json::{Value, json};
    use std::{
        collections::HashMap,
        io,
        path::{Path, PathBuf},
        process::ExitCode,
        time::Duration,
    };

    struct Opts {
        values: HashMap<String, String>,
        smoke: bool,
    }
    impl Opts {
        fn parse(args: &[String]) -> Opts {
            let mut values = HashMap::new();
            let mut smoke = false;
            let mut iter = args.iter();
            while let Some(arg) = iter.next() {
                match arg.strip_prefix("--") {
                    Some("smoke") => smoke = true,
                    Some(key) => {
                        values.insert(key.to_owned(), iter.next().cloned().unwrap_or_default());
                    }
                    None => {}
                }
            }
            Opts { values, smoke }
        }
        fn need(&self, key: &str) -> io::Result<&str> {
            self.values
                .get(key)
                .map(String::as_str)
                .ok_or_else(|| io::Error::other(format!("--{key} is required")))
        }
        fn path(&self, key: &str) -> io::Result<PathBuf> {
            self.need(key).map(PathBuf::from)
        }
        fn round(&self) -> io::Result<u32> {
            self.need("round")?.parse().map_err(io::Error::other)
        }
    }

    /// The frozen spec, shrunk for `--smoke` (never evidence).
    /// Diagnostic switches change what the arms log and trace; evidence runs refuse them.
    fn clean_environment(opts: &Opts) -> io::Result<()> {
        for name in ["RUST_LOG", "PERF_GATE_TRACE", "PERF_GATE_LOG_DIR"] {
            if !opts.smoke && std::env::var_os(name).is_some() {
                return Err(io::Error::other(format!(
                    "{name} is set: not allowed outside --smoke"
                )));
            }
        }
        Ok(())
    }

    fn plan(opts: &Opts) -> Spec {
        let mut spec = Spec::frozen();
        if opts.smoke {
            spec.warmup_per_arm = 2;
            spec.pairs = 8;
            spec.force_kill.samples_min = 10;
            spec.pss.guards = 10;
            spec.pss.hold_secs = 5;
            spec.state.idle_threads = 3;
            spec.state.messages_per_thread = 6;
            spec.state.repo_files = 50;
        }
        spec
    }

    fn write_json(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(value)?)
    }

    fn conditions_sha(opts: &Opts) -> io::Result<String> {
        match opts.values.get("conditions") {
            Some(path) => conditions::sha256_file(Path::new(path)),
            None if opts.smoke => Ok("smoke".into()),
            None => Err(io::Error::other("--conditions is required")),
        }
    }

    fn scope(opts: &Opts) -> io::Result<ScopeReadback> {
        match cells::scope_readback() {
            Ok(scope) => Ok(scope),
            Err(_) if opts.smoke => Ok(ScopeReadback {
                cpu_max: "unreadable".into(),
                memory_max: 0,
                swap_max: 0,
                pids_max: 0,
                stress_ng: None,
            }),
            Err(error) => Err(error),
        }
    }

    fn status_of(result: Result<io::Result<()>, tokio::time::error::Elapsed>) -> CellStatus {
        match result {
            Ok(Ok(())) => CellStatus::Ok,
            Ok(Err(error)) => CellStatus::Failed {
                reason: error.to_string(),
            },
            Err(_) => CellStatus::TimedOut {
                reason: "round exceeded its time budget".into(),
            },
        }
    }

    fn budget(spec: &Spec) -> Duration {
        Duration::from_secs(u64::from(spec.scope.timeout_secs).saturating_sub(25))
    }

    /// Diagnostic, not a measurement: one thread, `--turns` turns, no rotation; prints where the
    /// core fails the run and what it reports. `--mode` read-big (16 KiB read per turn),
    /// read-small (a few bytes) or text (8 KiB messages, no tool).
    async fn run_length(opts: &Opts) -> io::Result<ExitCode> {
        let mut spec = Spec::frozen();
        spec.state.repo_files = 50;
        let turns: u32 = opts
            .values
            .get("turns")
            .map_or(Ok(120), |v| v.parse())
            .map_err(io::Error::other)?;
        let mode = opts.values.get("mode").map_or("read-small", String::as_str);
        let arm = arm::CoreArm::start("X", &opts.path("exe")?, &spec).await?;
        let mut failed = None;
        for n in 0..turns {
            let (kind, bytes) = match mode {
                "read-big" => ("tick", 64),
                "read-small" => ("small", 64),
                "text" => ("prep", 8192),
                other => return Err(io::Error::other(format!("unknown --mode {other}"))),
            };
            let marker = format!("perf:{kind}:0:{n}");
            if let Err(error) = arm
                .turn("runlen", &marker, super::fixture::padded(&marker, bytes), 1)
                .await
            {
                println!(
                    "mode {mode}: FAILED at turn {n}: {error}\n{}",
                    arm.diagnose("runlen").await
                );
                failed = Some(n);
                break;
            }
        }
        if failed.is_none() {
            println!("mode {mode}: {turns} turns completed on one thread");
        }
        arm.shutdown().await;
        Ok(ExitCode::SUCCESS)
    }

    async fn latency_round(opts: &Opts) -> io::Result<ExitCode> {
        clean_environment(opts)?;
        let spec = plan(opts);
        let round = opts.round()?;
        let out = opts.path("out")?;
        let (a_exe, b_exe) = (opts.path("a-exe")?, opts.path("b-exe")?);
        let conditions_sha256 = conditions_sha(opts)?;
        let fixture_commit = opts
            .values
            .get("conditions")
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|c| {
                c.pointer("/fixtures/repo_commit")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let scope_start = scope(opts)?;
        let mut samples = Vec::new();
        let mut kills = Vec::new();
        let mut observations: Vec<Value> = Vec::new();
        let bundle = b_exe.with_file_name("guard-bundle.json");
        let body = async {
            let (a, b) = tokio::try_join!(
                arm::CoreArm::start("A", &a_exe, &spec),
                arm::CoreArm::start("B", &b_exe, &spec)
            )?;
            let run = async {
                if a.commit != b.commit || fixture_commit.as_deref().is_some_and(|c| c != a.commit)
                {
                    return Err(io::Error::other(format!(
                        "fixture repositories differ: A {} B {} conditions {fixture_commit:?}",
                        a.commit, b.commit
                    )));
                }
                tokio::try_join!(
                    arm::populate(&a, &spec.state),
                    arm::populate(&b, &spec.state)
                )?;
                let agents = (
                    arm::Agents::start(&a, &spec.state),
                    arm::Agents::start(&b, &spec.state),
                );
                tokio::time::sleep(Duration::from_secs(3)).await;
                if agents.0.failed() || agents.1.failed() {
                    return Err(io::Error::other(
                        "an active agent stopped before LOAD_READY",
                    ));
                }
                observations.push(json!({"at": "load_ready", "a": a.observe(), "b": b.observe()}));
                let measured = cells::latency(&spec, round, &a, &b, &mut samples).await;
                let killed = if measured.is_ok() && (bundle.exists() || !opts.smoke) {
                    cells::force_kill(&spec, &bundle, &mut kills).await
                } else {
                    measured
                };
                observations.push(json!({"at": "end", "a": a.observe(), "b": b.observe()}));
                let ticks = (agents.0.stop().await, agents.1.stop().await);
                observations.push(json!({"agent_ticks": format!("{ticks:?}")}));
                killed?;
                ticks.0?;
                ticks.1?;
                Ok(())
            };
            let result = run.await;
            tokio::join!(a.shutdown(), b.shutdown());
            result
        };
        let result = tokio::time::timeout(budget(&spec), body).await;
        let mut status = status_of(result);
        let scope_end = scope(opts)?;
        if matches!(status, CellStatus::Ok)
            && (scope_end != scope_start || (scope_end.stress_ng.is_none() && !opts.smoke))
        {
            status = CellStatus::Failed {
                reason: format!(
                    "scope changed or stress stopped: {scope_start:?} -> {scope_end:?}"
                ),
            };
        }
        let ok = matches!(status, CellStatus::Ok);
        write_json(
            &out,
            &LatencyRound {
                schema: SCHEMA,
                evidence: !opts.smoke,
                round,
                conditions_sha256,
                spec_sha256: Spec::sha256(),
                status,
                scope: scope_start,
                samples,
                force_kill: kills,
            },
        )?;
        write_json(&out.with_extension("observations.json"), &observations)?;
        Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }

    async fn pss_round(opts: &Opts) -> io::Result<ExitCode> {
        clean_environment(opts)?;
        let spec = plan(opts);
        let round = opts.round()?;
        let out = opts.path("out")?;
        let bundle = opts.path("b-exe")?.with_file_name("guard-bundle.json");
        let conditions_sha256 = conditions_sha(opts)?;
        let scope_start = scope(opts)?;
        let mut guards = Vec::new();
        let mut held = 0;
        let mut observations = Value::Null;
        let result = tokio::time::timeout(budget(&spec), async {
            let (g, o, h) = cells::guard_pss(
                spec.pss.guards,
                Duration::from_secs(u64::from(spec.pss.hold_secs)),
                &bundle,
            )
            .await?;
            (guards, observations, held) = (g, o, h);
            Ok(())
        })
        .await;
        let status = status_of(result);
        let ok = matches!(status, CellStatus::Ok);
        write_json(
            &out,
            &PssRound {
                schema: SCHEMA,
                evidence: !opts.smoke,
                round,
                conditions_sha256,
                spec_sha256: Spec::sha256(),
                status,
                scope: scope_start,
                hold_secs: held,
                guards,
            },
        )?;
        write_json(&out.with_extension("observations.json"), &observations)?;
        Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }

    /// A round that produced no record of its own (the scope killed it, or it
    /// could not start) is still recorded, as failed, and fails the gate.
    fn stub(opts: &Opts) -> io::Result<ExitCode> {
        let round = opts.round()?;
        let out = opts.path("out")?;
        let reason = opts.need("reason")?.to_owned();
        let status = if opts.values.get("status").map(String::as_str) == Some("timed_out") {
            CellStatus::TimedOut { reason }
        } else {
            CellStatus::Failed { reason }
        };
        let scope = ScopeReadback {
            cpu_max: "unrecorded".into(),
            memory_max: 0,
            swap_max: 0,
            pids_max: 0,
            stress_ng: None,
        };
        let (conditions_sha256, spec_sha256) = (conditions_sha(opts)?, Spec::sha256());
        match opts.need("kind")? {
            "latency" => write_json(
                &out,
                &LatencyRound {
                    schema: SCHEMA,
                    evidence: true,
                    round,
                    conditions_sha256,
                    spec_sha256,
                    status,
                    scope,
                    samples: vec![],
                    force_kill: vec![],
                },
            )?,
            "pss" => write_json(
                &out,
                &PssRound {
                    schema: SCHEMA,
                    evidence: true,
                    round,
                    conditions_sha256,
                    spec_sha256,
                    status,
                    scope,
                    hold_secs: 0,
                    guards: vec![],
                },
            )?,
            other => return Err(io::Error::other(format!("unknown --kind {other}"))),
        }
        Ok(ExitCode::SUCCESS)
    }

    fn load<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
        serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))
    }

    fn aggregate(opts: &Opts) -> io::Result<ExitCode> {
        let dir = opts.path("dir")?;
        let spec = Spec::frozen();
        let conditions_path = dir.join("conditions.json");
        let conditions: Value = load(&conditions_path)?;
        let mut evidence = Evidence {
            conditions_sha256: conditions::sha256_file(&conditions_path)?,
            conditions_problems: conditions::problems(&conditions),
            latency: Vec::new(),
            pss: Vec::new(),
        };
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with("latency-round-")
                && name.ends_with(".json")
                && !name.contains("observations")
            {
                evidence.latency.push(load(&path)?);
            } else if name.starts_with("pss-round-")
                && name.ends_with(".json")
                && !name.contains("observations")
            {
                evidence.pss.push(load(&path)?);
            }
        }
        evidence.latency.sort_by_key(|r: &LatencyRound| r.round);
        evidence.pss.sort_by_key(|r: &PssRound| r.round);
        let summary = evaluate(&spec, &evidence);
        write_json(&dir.join("summary.json"), &summary)?;
        let mut samples = String::from("round,workload,index,first,a_ns,b_ns\n");
        let mut kills = String::from("round,index,ns\n");
        let mut pss = String::from("round,guard_pid,pss_kib\n");
        for r in &evidence.latency {
            for s in &r.samples {
                samples += &format!(
                    "{},{},{},{:?},{},{}\n",
                    r.round, s.workload, s.index, s.first, s.a_ns, s.b_ns
                );
            }
            for k in &r.force_kill {
                kills += &format!(
                    "{},{},{}\n",
                    r.round,
                    k.index,
                    k.ns.map_or("none".into(), |n| n.to_string())
                );
            }
        }
        for r in &evidence.pss {
            for g in &r.guards {
                pss += &format!(
                    "{},{},{}\n",
                    r.round,
                    g.pid,
                    g.pss_kib.map_or("none".into(), |n| n.to_string())
                );
            }
        }
        std::fs::write(dir.join("samples.csv"), samples)?;
        std::fs::write(dir.join("force_kill.csv"), kills)?;
        std::fs::write(dir.join("pss.csv"), pss)?;
        for v in &summary.latency {
            println!(
                "latency r{} {:<15} dp50={:>9} ns dp95={:>9} ns {}",
                v.round,
                v.workload,
                v.delta_p50_ns,
                v.delta_p95_ns,
                if v.pass { "pass" } else { "FAIL" }
            );
        }
        for v in &summary.force_kill {
            println!(
                "force_kill r{} n={} p99={:?} ns {}",
                v.round,
                v.samples,
                v.p99_ns,
                if v.pass { "pass" } else { "FAIL" }
            );
        }
        for v in &summary.pss {
            println!(
                "pss r{} guards={} median={:?} KiB p95={:?} KiB {}",
                v.round,
                v.guards,
                v.median_kib,
                v.p95_kib,
                if v.pass { "pass" } else { "FAIL" }
            );
        }
        for f in &summary.failures {
            println!(
                "failure {:?} {} round={:?} workload={:?}: {}",
                f.kind, f.cell, f.round, f.workload, f.detail
            );
        }
        println!("verdict: {}", if summary.pass { "PASS" } else { "FAIL" });
        Ok(if summary.pass {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }

    pub fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let Some((command, rest)) = args.split_first() else {
            eprintln!(
                "usage: perf_gate conditions|latency|pss|stub|aggregate|print-order [--key value ...] [--smoke]"
            );
            return ExitCode::from(2);
        };
        let opts = Opts::parse(rest);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let result = runtime.block_on(async {
            match command.as_str() {
                "latency" => latency_round(&opts).await,
                "pss" => pss_round(&opts).await,
                "run-length" => run_length(&opts).await,
                "stub" => stub(&opts),
                "aggregate" => aggregate(&opts),
                "conditions" => {
                    let value = conditions::collect(&opts.values)?;
                    let problems = conditions::problems(&value);
                    write_json(&opts.path("out")?, &value)?;
                    for problem in &problems {
                        eprintln!("conditions problem: {problem}");
                    }
                    Ok(if problems.is_empty() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    })
                }
                "print-order" => {
                    let spec = Spec::frozen();
                    let w: usize = opts.need("workload")?.parse().map_err(io::Error::other)?;
                    println!("{}", order_string(&spec, opts.round()?, w));
                    Ok(ExitCode::SUCCESS)
                }
                other => Err(io::Error::other(format!("unknown subcommand {other}"))),
            }
        });
        // The arms and guards own processes; reap this runner's tokio state before exiting.
        drop(runtime);
        match result {
            Ok(code) => code,
            Err(error) => {
                eprintln!("perf_gate {command}: {error}");
                ExitCode::from(2)
            }
        }
    }
}
