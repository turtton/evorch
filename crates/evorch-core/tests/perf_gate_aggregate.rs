//! Negative and boundary tests of the B3-09 performance gate aggregator.
//!
//! Every test starts from evidence that passes, damages one thing and requires
//! the matching rejection. The "would have hidden it" assertions prove that the
//! damaged evidence passes under the shortcut the gate must not take (pooling
//! rounds, trimming outliers), so a test cannot succeed vacuously.
#[path = "../shared-test-support/perf_gate/mod.rs"]
mod perf_gate;

use perf_gate::*;

const COND: &str = "c0ffee0000000000000000000000000000000000000000000000000000000000";

fn scope() -> ScopeReadback {
    ScopeReadback {
        cpu_max: "200000 100000".into(),
        memory_max: 2_147_483_648,
        swap_max: 0,
        pids_max: 512,
        stress_ng: Some("stress-ng 0.19.0 /nix/store/x/bin/stress-ng".into()),
    }
}

fn latency_round(spec: &Spec, round: u32, delta_ns: i64) -> LatencyRound {
    let mut samples = Vec::new();
    for (w, workload) in spec.workloads.iter().enumerate() {
        for index in 0..spec.pairs {
            let a_ns = 1_000_000 + u64::from(index % 7) * 10_000;
            samples.push(PairSample {
                workload: workload.name.clone(),
                index,
                first: expected_first(spec.seed, round, w, index),
                a_ns,
                b_ns: (a_ns as i64 + delta_ns) as u64,
            });
        }
    }
    LatencyRound {
        schema: SCHEMA,
        evidence: true,
        round,
        conditions_sha256: COND.into(),
        spec_sha256: Spec::sha256(),
        status: CellStatus::Ok,
        scope: scope(),
        samples,
        c_samples: Vec::new(),
        force_kill: (0..spec.force_kill.samples_min)
            .map(|index| KillSample {
                index,
                ns: Some(20_000_000 + u64::from(index) * 1_000),
            })
            .collect(),
    }
}

fn pss_round(spec: &Spec, round: u32) -> PssRound {
    PssRound {
        schema: SCHEMA,
        evidence: true,
        round,
        conditions_sha256: COND.into(),
        spec_sha256: Spec::sha256(),
        status: CellStatus::Ok,
        scope: scope(),
        hold_secs: spec.pss.hold_secs,
        guards: (0..spec.pss.guards)
            .map(|i| GuardPss {
                pid: 1000 + i,
                pss_kib: Some(3000 + u64::from(i)),
            })
            .collect(),
    }
}

fn good(spec: &Spec) -> Evidence {
    Evidence {
        conditions_sha256: COND.into(),
        conditions_problems: Vec::new(),
        latency: (1..=spec.rounds)
            .map(|r| latency_round(spec, r, 500_000))
            .collect(),
        pss: (1..=spec.rounds).map(|r| pss_round(spec, r)).collect(),
    }
}

fn kinds(summary: &Summary) -> Vec<FailureKind> {
    summary.failures.iter().map(|f| f.kind).collect()
}

fn rejected(spec: &Spec, evidence: &Evidence, kind: FailureKind) -> Summary {
    let summary = evaluate(spec, evidence);
    assert!(!summary.pass, "must be rejected: {kind:?}");
    assert!(
        kinds(&summary).contains(&kind),
        "{kind:?} missing in {:?}",
        summary.failures
    );
    summary
}

#[test]
fn baseline_evidence_passes() {
    let spec = Spec::frozen();
    let summary = evaluate(&spec, &good(&spec));
    assert!(summary.pass, "{:?}", summary.failures);
    assert_eq!(summary.latency.len(), 9);
    assert_eq!(summary.force_kill.len(), 3);
    assert_eq!(summary.pss.len(), 3);
}

#[test]
fn frozen_spec_keeps_the_approved_values() {
    let s = Spec::frozen();
    assert_eq!((s.rounds, s.warmup_per_arm, s.pairs), (3, 20, 200));
    assert_eq!(s.latency_limits.p50_delta_ns, 2_000_000);
    assert_eq!(s.latency_limits.p95_delta_ns, 5_000_000);
    assert_eq!(s.force_kill.samples_min, 200);
    assert_eq!(s.force_kill.p99_limit_ns, 1_000_000_000);
    assert_eq!((s.pss.guards, s.pss.hold_secs), (100, 180));
    assert_eq!(
        (s.pss.median_limit_kib, s.pss.p95_limit_kib),
        (4 * 1024, 8 * 1024)
    );
    assert_eq!(
        (
            s.scope.cpus,
            s.scope.memory_bytes,
            s.scope.tasks,
            s.scope.timeout_secs,
            s.scope.stress_cpus
        ),
        (2, 2 << 30, 512, 600, 1)
    );
    assert_eq!(
        (
            s.state.idle_threads,
            s.state.messages_per_thread,
            s.state.message_bytes
        ),
        (20, 50, 8192)
    );
    assert_eq!(
        (
            s.state.repo_files,
            s.state.repo_file_bytes,
            s.state.active_agents
        ),
        (1000, 16384, 2)
    );
    let commands: Vec<_> = s.workloads.iter().map(|w| w.command.as_str()).collect();
    assert_eq!(
        commands,
        [
            "grep -F -- D-7 fixture.txt",
            "git diff --quiet -- tracked.txt",
            ":"
        ]
    );
}

#[test]
fn nearest_rank_uses_the_ceiling_rank() {
    let v: Vec<u64> = (1..=200).collect();
    assert_eq!(nearest_rank(&v, 500), Some(100));
    assert_eq!(nearest_rank(&v, 950), Some(190));
    assert_eq!(nearest_rank(&v, 990), Some(198));
    assert_eq!(nearest_rank(&v, 1000), Some(200));
    let w: Vec<u64> = (1..=100).rev().collect();
    assert_eq!(nearest_rank(&w, 500), Some(50));
    assert_eq!(nearest_rank(&w, 950), Some(95));
    assert_eq!(nearest_rank(&[7u64], 990), Some(7));
    assert_eq!(nearest_rank(&[1u64, 2, 3], 500), Some(2));
    assert_eq!(nearest_rank::<u64>(&[], 500), None);
}

#[test]
fn abba_order_is_balanced_and_deterministic() {
    let spec = Spec::frozen();
    for round in 1..=spec.rounds {
        for w in 0..spec.workloads.len() {
            let seq: Vec<Arm> = (0..spec.pairs)
                .map(|i| expected_first(spec.seed, round, w, i))
                .collect();
            assert_eq!(seq.iter().filter(|a| **a == Arm::A).count(), 100);
            for group in seq.chunks(4) {
                assert_eq!(group[0], group[3]);
                assert_ne!(group[0], group[1]);
                assert_eq!(group[1], group[2]);
            }
            let again: Vec<Arm> = (0..spec.pairs)
                .map(|i| expected_first(spec.seed, round, w, i))
                .collect();
            assert_eq!(seq, again);
        }
    }
}

#[test]
fn latency_increase_is_rejected_at_p50() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    ev.latency[1] = latency_round(&spec, 2, 2_500_000);
    let summary = rejected(&spec, &ev, FailureKind::LatencyP50);
    assert!(
        summary.failures.iter().all(|f| f.round == Some(2)),
        "{:?}",
        summary.failures
    );
}

#[test]
fn latency_exactly_at_the_limit_passes_and_one_ns_over_fails() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    ev.latency[0] = latency_round(&spec, 1, 2_000_000);
    assert!(
        evaluate(&spec, &ev)
            .failures
            .iter()
            .all(|f| f.kind != FailureKind::LatencyP50)
    );
    ev.latency[0] = latency_round(&spec, 1, 2_000_001);
    rejected(&spec, &ev, FailureKind::LatencyP50);
}

#[test]
fn tail_increase_is_rejected_at_p95_even_when_p50_is_clean() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    // 6% of the B samples are 10 ms slower: nearest-rank p95 lands on one of them.
    for s in ev.latency[2]
        .samples
        .iter_mut()
        .filter(|s| s.workload == "colon")
        .take(12)
    {
        s.b_ns += 10_000_000;
    }
    let summary = rejected(&spec, &ev, FailureKind::LatencyP95);
    assert!(!kinds(&summary).contains(&FailureKind::LatencyP50));
}

#[test]
fn removing_outliers_would_hide_the_tail_but_the_gate_does_not() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    for s in ev.latency[0]
        .samples
        .iter_mut()
        .filter(|s| s.workload == "colon")
        .take(12)
    {
        s.b_ns += 10_000_000;
    }
    // The shortcut: drop the slowest 10% of B before judging. It would pass.
    let mut trimmed = ev.clone();
    let mut b: Vec<u64> = trimmed.latency[0]
        .samples
        .iter()
        .filter(|s| s.workload == "colon")
        .map(|s| s.b_ns)
        .collect();
    b.sort_unstable();
    let cutoff = b[b.len() * 9 / 10];
    for s in trimmed.latency[0]
        .samples
        .iter_mut()
        .filter(|s| s.workload == "colon")
    {
        s.b_ns = s.b_ns.min(cutoff);
    }
    assert!(
        evaluate(&spec, &trimmed).pass,
        "the trimmed copy must pass, or this test proves nothing"
    );
    rejected(&spec, &ev, FailureKind::LatencyP95);
}

#[test]
fn a_round_that_exceeds_cannot_be_hidden_by_pooling_rounds() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    ev.latency[0] = latency_round(&spec, 1, -500_000);
    ev.latency[1] = latency_round(&spec, 2, -500_000);
    ev.latency[2] = latency_round(&spec, 3, 2_800_000);
    let pooled: Vec<i64> = ev
        .latency
        .iter()
        .flat_map(|r| r.samples.iter())
        .filter(|s| s.workload == "colon")
        .map(|s| s.b_ns as i64 - s.a_ns as i64)
        .collect();
    assert!(
        nearest_rank(&pooled, 500).unwrap() <= spec.latency_limits.p50_delta_ns,
        "pooled p50 would pass"
    );
    let summary = rejected(&spec, &ev, FailureKind::LatencyP50);
    assert!(summary.failures.iter().all(|f| f.round == Some(3)));
}

#[test]
fn one_workload_failing_fails_the_gate() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    for s in ev.latency[0]
        .samples
        .iter_mut()
        .filter(|s| s.workload == "git_diff_quiet")
    {
        s.b_ns += 4_000_000;
    }
    let summary = rejected(&spec, &ev, FailureKind::LatencyP50);
    assert_eq!(
        summary.failures[0].workload.as_deref(),
        Some("git_diff_quiet")
    );
}

#[test]
fn both_delta_definitions_are_reported_and_only_the_chosen_one_gates() {
    let mut spec = Spec::frozen();
    let ev = good(&spec);
    let a = evaluate(&spec, &ev);
    spec.delta_definition = DeltaDefinition::PairedDifferenceQuantile;
    let b = evaluate(&spec, &ev);
    assert_eq!(a.latency[0].delta_p50_ns, b.latency[0].other_delta_p50_ns);
    assert_eq!(a.latency[0].other_delta_p95_ns, b.latency[0].delta_p95_ns);
}

#[test]
fn missing_duplicate_and_unexpected_rounds_are_rejected() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    ev.latency.pop();
    let s = rejected(&spec, &ev, FailureKind::MissingRound);
    assert!(
        s.failures
            .iter()
            .any(|f| f.cell == "latency" && f.round == Some(3))
    );

    let mut ev = good(&spec);
    ev.pss.remove(0);
    let s = rejected(&spec, &ev, FailureKind::MissingRound);
    assert!(
        s.failures
            .iter()
            .any(|f| f.cell == "pss" && f.round == Some(1))
    );

    let mut ev = good(&spec);
    let copy = ev.latency[0].clone();
    ev.latency[1] = copy;
    rejected(&spec, &ev, FailureKind::DuplicateRound);

    let mut ev = good(&spec);
    ev.latency[2] = latency_round(&spec, 4, 0);
    rejected(&spec, &ev, FailureKind::UnexpectedRound);

    let mut ev = good(&spec);
    ev.latency.clear();
    ev.pss.clear();
    assert!(!evaluate(&spec, &ev).pass);
}

#[test]
fn failed_and_timed_out_rounds_are_rejected_and_listed() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    ev.latency[1].status = CellStatus::TimedOut {
        reason: "scope timeout 600 s".into(),
    };
    ev.pss[0].status = CellStatus::Failed {
        reason: "OOM kill".into(),
    };
    let s = rejected(&spec, &ev, FailureKind::RoundFailed);
    let rounds: Vec<_> = s
        .failures
        .iter()
        .filter(|f| f.kind == FailureKind::RoundFailed)
        .collect();
    assert_eq!(rounds.len(), 2);
    assert!(
        rounds
            .iter()
            .any(|f| f.cell == "latency" && f.round == Some(2) && f.detail.contains("timed out"))
    );
    assert!(
        rounds
            .iter()
            .any(|f| f.cell == "pss" && f.round == Some(1) && f.detail.contains("OOM"))
    );
}

#[test]
fn missing_pairs_wrong_order_zero_and_unknown_workloads_are_rejected() {
    let spec = Spec::frozen();

    let mut ev = good(&spec);
    let at = ev.latency[0]
        .samples
        .iter()
        .position(|s| s.workload == "colon")
        .unwrap();
    ev.latency[0].samples.remove(at);
    rejected(&spec, &ev, FailureKind::SampleCount);

    let mut ev = good(&spec);
    let s = &mut ev.latency[0].samples[0];
    s.first = if s.first == Arm::A { Arm::B } else { Arm::A };
    rejected(&spec, &ev, FailureKind::OrderMismatch);

    let mut ev = good(&spec);
    ev.latency[0].samples[5].b_ns = 0;
    rejected(&spec, &ev, FailureKind::InvalidSample);

    let mut ev = good(&spec);
    ev.latency[0].samples.push(PairSample {
        workload: "extra".into(),
        index: 0,
        first: Arm::A,
        a_ns: 1,
        b_ns: 1,
    });
    rejected(&spec, &ev, FailureKind::UnknownWorkload);
}

#[test]
fn smoke_and_mismatched_evidence_never_passes() {
    let spec = Spec::frozen();

    let mut ev = good(&spec);
    ev.latency[0].evidence = false;
    rejected(&spec, &ev, FailureKind::NotEvidence);

    let mut ev = good(&spec);
    ev.pss[2].evidence = false;
    rejected(&spec, &ev, FailureKind::NotEvidence);

    let mut ev = good(&spec);
    ev.latency[0].conditions_sha256 = "00".repeat(32);
    rejected(&spec, &ev, FailureKind::ConditionsMismatch);

    let mut ev = good(&spec);
    ev.pss[0].spec_sha256 = "11".repeat(32);
    rejected(&spec, &ev, FailureKind::SpecMismatch);

    let mut ev = good(&spec);
    ev.latency[0].schema = 99;
    rejected(&spec, &ev, FailureKind::SchemaMismatch);
}

#[test]
fn a_scope_that_is_not_the_gate_scope_is_rejected() {
    let spec = Spec::frozen();
    for damage in [
        (|s: &mut ScopeReadback| s.cpu_max = "100000 100000".into()) as fn(&mut ScopeReadback),
        |s| s.cpu_max = "max 100000".into(),
        |s| s.memory_max = 4 << 30,
        |s| s.swap_max = 1,
        |s| s.pids_max = 4096,
        |s| s.stress_ng = None,
    ] {
        let mut ev = good(&spec);
        damage(&mut ev.latency[0].scope);
        rejected(&spec, &ev, FailureKind::ScopeMismatch);
        let mut ev = good(&spec);
        damage(&mut ev.pss[1].scope);
        rejected(&spec, &ev, FailureKind::ScopeMismatch);
    }
}

#[test]
fn force_kill_p99_must_be_strictly_below_one_second() {
    let spec = Spec::frozen();
    let slow = |count: usize, ns: u64| {
        let mut ev = good(&spec);
        for k in ev.latency[0].force_kill.iter_mut().take(count) {
            k.ns = Some(ns);
        }
        ev
    };
    // 2 of 200 slow: nearest-rank p99 is rank 198, still a normal sample.
    assert!(
        evaluate(&spec, &slow(2, 1_500_000_000))
            .failures
            .iter()
            .all(|f| f.kind != FailureKind::ForceKillP99)
    );
    rejected(&spec, &slow(3, 1_500_000_000), FailureKind::ForceKillP99);
    rejected(&spec, &slow(3, 1_000_000_000), FailureKind::ForceKillP99);
    assert!(
        evaluate(&spec, &slow(3, 999_999_999))
            .failures
            .iter()
            .all(|f| f.kind != FailureKind::ForceKillP99)
    );
}

#[test]
fn force_kill_timeouts_few_samples_and_zero_are_rejected() {
    let spec = Spec::frozen();

    let mut ev = good(&spec);
    ev.latency[0].force_kill[10].ns = None;
    let s = rejected(&spec, &ev, FailureKind::ForceKillTimeout);
    // The lost sample is not dropped from the distribution either.
    assert!(s.force_kill[0].p99_ns.is_some());

    let mut ev = good(&spec);
    ev.latency[0].force_kill.truncate(199);
    rejected(&spec, &ev, FailureKind::SampleCount);

    let mut ev = good(&spec);
    ev.latency[0].force_kill[0].ns = Some(spec.force_kill.observe_budget_ns + 1);
    rejected(&spec, &ev, FailureKind::ForceKillTimeout);

    let mut ev = good(&spec);
    ev.latency[0].force_kill[0].ns = Some(0);
    rejected(&spec, &ev, FailureKind::InvalidSample);

    let mut ev = good(&spec);
    for k in &mut ev.latency[0].force_kill {
        k.ns = None;
    }
    rejected(&spec, &ev, FailureKind::ForceKillP99);
}

#[test]
fn pss_over_the_median_or_p95_limit_is_rejected() {
    let spec = Spec::frozen();

    let mut ev = good(&spec);
    for g in &mut ev.pss[0].guards {
        g.pss_kib = Some(4096);
    }
    assert!(evaluate(&spec, &ev).pss[0].pass, "exactly 4 MiB passes");
    for g in &mut ev.pss[0].guards {
        g.pss_kib = Some(4097);
    }
    rejected(&spec, &ev, FailureKind::PssMedian);

    // 5 of 100 guards at 9 MiB: p95 is rank 95, a normal guard.
    let mut ev = good(&spec);
    for g in ev.pss[1].guards.iter_mut().take(5) {
        g.pss_kib = Some(9 * 1024);
    }
    assert!(
        evaluate(&spec, &ev)
            .failures
            .iter()
            .all(|f| f.kind != FailureKind::PssP95)
    );
    for g in ev.pss[1].guards.iter_mut().take(6) {
        g.pss_kib = Some(9 * 1024);
    }
    let s = rejected(&spec, &ev, FailureKind::PssP95);
    assert!(!kinds(&s).contains(&FailureKind::PssMedian));
}

#[test]
fn pss_with_missing_guards_readings_or_a_short_hold_is_rejected() {
    let spec = Spec::frozen();

    let mut ev = good(&spec);
    ev.pss[0].guards.pop();
    rejected(&spec, &ev, FailureKind::PssGuardCount);

    let mut ev = good(&spec);
    ev.pss[0].guards[3].pss_kib = None;
    rejected(&spec, &ev, FailureKind::PssMissingReading);

    let mut ev = good(&spec);
    ev.pss[0].hold_secs = 179;
    rejected(&spec, &ev, FailureKind::PssHoldTooShort);
}

#[test]
fn evidence_and_summary_survive_a_json_round_trip() {
    let spec = Spec::frozen();
    let ev = good(&spec);
    let back: Evidence = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
    assert_eq!(back, ev);
    let summary = evaluate(&spec, &ev);
    let back: Summary = serde_json::from_str(&serde_json::to_string(&summary).unwrap()).unwrap();
    assert_eq!(back, summary);
}

fn conditions(spec: &Spec) -> serde_json::Value {
    let hex = |c: char, n: usize| c.to_string().repeat(n);
    let mut order = serde_json::Map::new();
    for round in 1..=spec.rounds {
        let mut per = serde_json::Map::new();
        for (w, workload) in spec.workloads.iter().enumerate() {
            per.insert(workload.name.clone(), order_string(spec, round, w).into());
        }
        order.insert(round.to_string(), per.into());
    }
    serde_json::json!({
        "spec_sha256": Spec::sha256(),
        "seed": spec.seed,
        "order": order,
        "host": {"uname": "Linux", "kernel": "7", "cpu_model": "x", "cpu_count": 24,
                 "governor": "performance", "mem_total_kib": 1, "loadavg": "0 0 0"},
        "toolchain": {"rustc": "1", "cargo": "1", "profile": "release", "libc": "glibc",
                      "rustflags_set": false},
        "tools": {"sh": "/bin/sh", "grep": "grep", "git": "git"},
        "stress_ng": "stress-ng 0.19 /nix/store/x",
        "fixtures": {"repo_commit": hex('a', 40)},
        "runner": {"sha256": hex('b', 64)},
        "arms": {
            "a": {"source_sha": hex('1', 40), "tree_sha": hex('2', 40), "build_recipe": "r",
                  "exe_path": "/a", "exe_sha256": hex('3', 64)},
            "b": {"source_sha": hex('4', 40), "tree_sha": hex('5', 40), "build_recipe": "r",
                  "exe_path": "/b", "exe_sha256": hex('6', 64),
                  "helper_sha256": hex('7', 64), "bundle_sha256": hex('8', 64)}
        }
    })
}

#[test]
fn complete_conditions_have_no_problems() {
    let spec = Spec::frozen();
    assert_eq!(
        check_conditions(&spec, &conditions(&spec)),
        Vec::<String>::new()
    );
}

#[test]
fn incomplete_or_altered_conditions_fail_the_gate() {
    let spec = Spec::frozen();
    type Damage = fn(&mut serde_json::Value);
    let damage: [(&str, Damage); 6] = [
        ("/arms/a/source_sha", |c| {
            c["arms"]["a"]["source_sha"] = "abc".into()
        }),
        ("/arms/b/helper_sha256", |c| {
            c["arms"]["b"]
                .as_object_mut()
                .unwrap()
                .remove("helper_sha256")
                .map(drop)
                .unwrap()
        }),
        ("/stress_ng", |c| c["stress_ng"] = "".into()),
        ("/spec_sha256", |c| c["spec_sha256"] = "0".repeat(64).into()),
        ("/seed", |c| c["seed"] = 1.into()),
        ("/order/2/colon", |c| {
            c["order"]["2"]["colon"] = "A".repeat(200).into()
        }),
    ];
    for (pointer, apply) in damage {
        let mut c = conditions(&spec);
        apply(&mut c);
        let problems = check_conditions(&spec, &c);
        assert!(
            problems.iter().any(|p| p.starts_with(pointer)),
            "{pointer}: {problems:?}"
        );
        let mut ev = good(&spec);
        ev.conditions_problems = problems;
        rejected(&spec, &ev, FailureKind::ConditionsMismatch);
    }
}

fn with_c(spec: &Spec, evidence: &mut Evidence, c_delta_ns: i64) {
    for round in &mut evidence.latency {
        round.c_samples = round
            .samples
            .iter()
            .map(|s| CSample {
                workload: s.workload.clone(),
                index: s.index,
                slot: (s.index % 3) as u8,
                c_ns: (s.b_ns as i64 + c_delta_ns) as u64,
            })
            .collect();
    }
    assert_eq!(
        evidence.latency[0].c_samples.len(),
        (spec.pairs * 3) as usize
    );
}

#[test]
fn arm_c_is_reported_but_a_slow_c_cannot_fail_the_gate() {
    let spec = Spec::frozen();
    let mut evidence = good(&spec);
    with_c(&spec, &mut evidence, 50_000_000);
    let summary = evaluate(&spec, &evidence);
    assert!(summary.pass, "{:?}", summary.failures);
    assert_eq!(summary.c_report.len(), 9);
    assert!(
        summary
            .c_report
            .iter()
            .all(|r| r.delta_p50_ns == 50_000_000)
    );
}

#[test]
fn a_fast_c_cannot_rescue_a_failing_ab_comparison() {
    let spec = Spec::frozen();
    let mut evidence = good(&spec);
    evidence.latency[1] = latency_round(&spec, 2, 2_000_001);
    with_c(&spec, &mut evidence, -900_000);
    rejected(&spec, &evidence, FailureKind::LatencyP50);
}

#[test]
fn misaligned_c_samples_are_left_out_of_the_report_and_never_fail() {
    let spec = Spec::frozen();
    let mut evidence = good(&spec);
    with_c(&spec, &mut evidence, 1_000);
    evidence.latency[0].c_samples.pop();
    let summary = evaluate(&spec, &evidence);
    assert!(summary.pass, "{:?}", summary.failures);
    assert_eq!(summary.c_report.len(), 8);
}
