//! Negative and boundary tests of the B3-09 performance check aggregator.
//!
//! Every test starts from evidence that passes, damages one thing and requires
//! the matching rejection. The "would have hidden it" assertions prove that the
//! damaged evidence passes under the shortcut the check must not take (pooling
//! rounds, trimming outliers), so a test cannot succeed vacuously.
#[path = "../shared-test-support/perf_gate/mod.rs"]
mod perf_gate;

use perf_gate::*;

const COND: &str = "c0ffee0000000000000000000000000000000000000000000000000000000000";

/// A's RTT in a good round: 1 ms plus a small spread, so p50(A) is 1.03 ms.
fn a_ns(index: u32) -> u64 {
    1_000_000 + u64::from(index % 7) * 10_000
}

/// Every pair has B = A * milli / 1000.
fn latency_round(spec: &Spec, round: u32, milli: u64) -> LatencyRound {
    let mut samples = Vec::new();
    for workload in &spec.workloads {
        for index in 0..spec.pairs {
            samples.push(PairSample {
                workload: workload.name.clone(),
                index,
                a_ns: a_ns(index),
                b_ns: a_ns(index) * milli / 1000,
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
        samples,
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
            .map(|r| latency_round(spec, r, 1_200))
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
    assert_eq!(summary.latency.len(), 3);
    assert_eq!(summary.force_kill.len(), 1);
    assert_eq!(summary.pss.len(), 1);
    assert!(summary.latency.iter().all(|v| v.ratio_p50_milli == 1_200));
}

#[test]
fn frozen_spec_keeps_the_approved_values() {
    let s = Spec::frozen();
    assert_eq!((s.rounds, s.warmup_per_arm, s.pairs), (1, 5, 50));
    assert_eq!(s.latency_limits.p50_ratio_milli, 1_500);
    assert_eq!(s.force_kill.samples_min, 200);
    assert_eq!(s.force_kill.p99_limit_ns, 1_000_000_000);
    assert_eq!((s.pss.guards, s.pss.hold_secs), (100, 180));
    assert_eq!((s.pss.median_limit_kib, s.pss.p95_limit_kib), (4096, 8192));
    let names: Vec<_> = s.workloads.iter().map(|w| w.command.as_str()).collect();
    assert_eq!(
        names,
        [
            "grep -F -- D-7 fixture.txt",
            "git diff --quiet -- tracked.txt",
            ":"
        ]
    );
    assert_eq!(
        (s.state.idle_threads, s.state.messages_per_thread),
        (20, 50)
    );
    assert_eq!((s.state.repo_files, s.state.repo_file_bytes), (1000, 16384));
    assert_eq!(s.state.active_agents, 2);
}

#[test]
fn nearest_rank_uses_the_ceiling_rank() {
    let v: Vec<u32> = (1..=10).collect();
    assert_eq!(nearest_rank(&v, 500), Some(5));
    assert_eq!(nearest_rank(&v, 501), Some(6));
    assert_eq!(nearest_rank(&v, 950), Some(10));
    assert_eq!(nearest_rank(&v, 1000), Some(10));
    assert_eq!(nearest_rank(&v, 1), Some(1));
    assert_eq!(nearest_rank::<u32>(&[], 500), None);
    assert_eq!(nearest_rank(&[7u32], 990), Some(7));
    let shuffled = [9u32, 1, 8, 2, 7, 3, 6, 4, 5, 10];
    assert_eq!(nearest_rank(&shuffled, 500), Some(5));
}

#[test]
fn latency_increase_is_rejected() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    ev.latency[0] = latency_round(&spec, 1, 2_000);
    let summary = rejected(&spec, &ev, FailureKind::LatencyRatio);
    assert!(summary.latency.iter().all(|v| !v.pass));
}

#[test]
fn the_ratio_limit_is_exact_and_one_nanosecond_over_fails() {
    let spec = Spec::frozen();
    let mut at_limit = good(&spec);
    for s in &mut at_limit.latency[0].samples {
        s.a_ns = 1_000_000;
        s.b_ns = 1_500_000;
    }
    assert!(evaluate(&spec, &at_limit).pass);
    for s in &mut at_limit.latency[0].samples {
        s.b_ns = 1_500_001;
    }
    let summary = rejected(&spec, &at_limit, FailureKind::LatencyRatio);
    assert!(summary.latency.iter().all(|v| v.ratio_p50_milli == 1_500));
}

#[test]
fn a_faster_b_passes_and_a_slow_a_does_not_loosen_the_limit() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    ev.latency[0] = latency_round(&spec, 1, 600);
    assert!(evaluate(&spec, &ev).pass);
    for s in &mut ev.latency[0].samples {
        s.a_ns = 10_000_000;
        s.b_ns = 15_000_001;
    }
    rejected(&spec, &ev, FailureKind::LatencyRatio);
}

#[test]
fn outliers_are_never_removed_so_a_slow_majority_cannot_be_trimmed_away() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    let n = spec.pairs as usize;
    // 26 of 50 pairs of one workload are slow: the median moves, and removing the
    // slowest "outliers" would hide it.
    let mut slow = 0;
    for s in ev.latency[0]
        .samples
        .iter_mut()
        .filter(|s| s.workload == "colon")
    {
        if slow < n / 2 + 1 {
            s.b_ns = s.a_ns * 5;
            slow += 1;
        }
    }
    let summary = rejected(&spec, &ev, FailureKind::LatencyRatio);
    let failing: Vec<_> = summary
        .failures
        .iter()
        .filter_map(|f| f.workload.as_deref())
        .collect();
    assert_eq!(failing, ["colon"]);
    let mut trimmed = ev.clone();
    for s in trimmed.latency[0]
        .samples
        .iter_mut()
        .filter(|s| s.workload == "colon" && s.b_ns > s.a_ns * 2)
    {
        s.b_ns = s.a_ns;
    }
    assert!(
        evaluate(&spec, &trimmed).pass,
        "trimming would have hidden it"
    );
}

#[test]
fn a_fast_tail_does_not_fail_and_only_the_median_decides() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    for s in ev.latency[0]
        .samples
        .iter_mut()
        .filter(|s| s.workload == "grep_fixed" && s.index % 10 == 0)
    {
        s.b_ns = s.a_ns * 40;
    }
    let summary = evaluate(&spec, &ev);
    assert!(summary.pass, "{:?}", summary.failures);
    let v = summary
        .latency
        .iter()
        .find(|v| v.workload == "grep_fixed")
        .unwrap();
    assert!(v.paired_delta_p95_ns > 0 && v.marginal_delta_p95_ns > 0);
}

#[test]
fn both_delta_definitions_are_recorded_and_never_gate() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    for (i, s) in ev.latency[0]
        .samples
        .iter_mut()
        .filter(|s| s.workload == "colon")
        .enumerate()
    {
        // B is a permutation of A's values, so the marginals are equal while the paired
        // differences are not.
        s.b_ns = a_ns((i as u32 + 3) % spec.pairs);
    }
    let summary = evaluate(&spec, &ev);
    assert!(summary.pass, "{:?}", summary.failures);
    let v = summary
        .latency
        .iter()
        .find(|v| v.workload == "colon")
        .unwrap();
    assert_eq!(v.marginal_delta_p50_ns, 0);
    assert_ne!(
        (v.paired_delta_p50_ns, v.paired_delta_p95_ns),
        (v.marginal_delta_p50_ns, v.marginal_delta_p95_ns)
    );
}

#[test]
fn one_workload_failing_fails_the_check() {
    let spec = Spec::frozen();
    let mut ev = good(&spec);
    for s in ev.latency[0]
        .samples
        .iter_mut()
        .filter(|s| s.workload == "git_diff_quiet")
    {
        s.b_ns = s.a_ns * 3;
    }
    let summary = rejected(&spec, &ev, FailureKind::LatencyRatio);
    assert_eq!(summary.latency.iter().filter(|v| v.pass).count(), 2);
}

#[test]
fn a_round_that_exceeds_cannot_be_hidden_by_pooling_rounds() {
    let mut spec = Spec::frozen();
    spec.rounds = 2;
    let mut ev = good(&spec);
    ev.latency[1] = latency_round(&spec, 2, 1_900);
    let summary = rejected(&spec, &ev, FailureKind::LatencyRatio);
    assert!(summary.failures.iter().all(|f| f.round == Some(2)));
    let mut pooled = ev.latency[0].clone();
    pooled.samples.extend(ev.latency[1].samples.clone());
    let (mut a, mut b): (Vec<u64>, Vec<u64>) = (vec![], vec![]);
    for s in &pooled.samples {
        a.push(s.a_ns);
        b.push(s.b_ns);
    }
    let (a50, b50) = (
        nearest_rank(&a, 500).unwrap(),
        nearest_rank(&b, 500).unwrap(),
    );
    assert!(b50 * 1000 <= a50 * 1500, "pooling would have hidden it");
}

#[test]
fn missing_duplicate_and_unexpected_rounds_are_rejected() {
    let spec = Spec::frozen();
    let mut missing = good(&spec);
    missing.latency.clear();
    rejected(&spec, &missing, FailureKind::MissingRound);
    let mut missing_pss = good(&spec);
    missing_pss.pss.clear();
    rejected(&spec, &missing_pss, FailureKind::MissingRound);
    let mut dup = good(&spec);
    dup.latency.push(latency_round(&spec, 1, 1_000));
    rejected(&spec, &dup, FailureKind::DuplicateRound);
    let mut extra = good(&spec);
    extra.pss.push(pss_round(&spec, 2));
    rejected(&spec, &extra, FailureKind::UnexpectedRound);
    let mut zero = good(&spec);
    zero.latency[0].round = 0;
    rejected(&spec, &zero, FailureKind::UnexpectedRound);
}

#[test]
fn failed_and_timed_out_rounds_are_rejected() {
    let spec = Spec::frozen();
    let mut failed = good(&spec);
    failed.latency[0].status = CellStatus::Failed {
        reason: "core exited".into(),
    };
    rejected(&spec, &failed, FailureKind::RoundFailed);
    let mut timed_out = good(&spec);
    timed_out.latency[0].status = CellStatus::TimedOut {
        reason: "round exceeded its time budget".into(),
    };
    let summary = rejected(&spec, &timed_out, FailureKind::RoundFailed);
    assert!(
        summary.latency.is_empty(),
        "a timed-out round is not judged"
    );
    let mut pss_timeout = good(&spec);
    pss_timeout.pss[0].status = CellStatus::TimedOut {
        reason: "hold".into(),
    };
    rejected(&spec, &pss_timeout, FailureKind::RoundFailed);
}

#[test]
fn smoke_and_mismatched_evidence_never_passes() {
    let spec = Spec::frozen();
    let mut smoke = good(&spec);
    smoke.latency[0].evidence = false;
    rejected(&spec, &smoke, FailureKind::NotEvidence);
    let mut smoke_pss = good(&spec);
    smoke_pss.pss[0].evidence = false;
    rejected(&spec, &smoke_pss, FailureKind::NotEvidence);
    let mut schema = good(&spec);
    schema.latency[0].schema = 1;
    rejected(&spec, &schema, FailureKind::SchemaMismatch);
    let mut spec_sha = good(&spec);
    spec_sha.pss[0].spec_sha256 = "0".repeat(64);
    rejected(&spec, &spec_sha, FailureKind::SpecMismatch);
    let mut cond = good(&spec);
    cond.latency[0].conditions_sha256 = "0".repeat(64);
    rejected(&spec, &cond, FailureKind::ConditionsMismatch);
}

#[test]
fn short_or_malformed_latency_samples_are_rejected() {
    let spec = Spec::frozen();
    let mut short = good(&spec);
    short.latency[0].samples.pop();
    rejected(&spec, &short, FailureKind::SampleCount);
    let mut gap = good(&spec);
    gap.latency[0].samples[3].index = 99;
    rejected(&spec, &gap, FailureKind::InvalidSample);
    let mut zero = good(&spec);
    zero.latency[0].samples[3].b_ns = 0;
    rejected(&spec, &zero, FailureKind::InvalidSample);
    let mut unknown = good(&spec);
    unknown.latency[0].samples[0].workload = "other".into();
    rejected(&spec, &unknown, FailureKind::UnknownWorkload);
}

#[test]
fn force_kill_over_one_second_is_rejected_and_a_lost_sample_counts_as_never() {
    let spec = Spec::frozen();
    let mut slow = good(&spec);
    for k in slow.latency[0].force_kill.iter_mut().take(3) {
        k.ns = Some(1_000_000_000);
    }
    // 3 of 200 at 1 s: the nearest-rank p99 is the 198th value.
    let summary = rejected(&spec, &slow, FailureKind::ForceKillP99);
    assert_eq!(summary.force_kill[0].p99_ns, Some(1_000_000_000));
    let mut lost = good(&spec);
    lost.latency[0].force_kill[10].ns = None;
    let summary = rejected(&spec, &lost, FailureKind::ForceKillTimeout);
    assert_eq!(summary.force_kill[0].samples, 200);
    let mut many_lost = good(&spec);
    for k in many_lost.latency[0].force_kill.iter_mut().take(3) {
        k.ns = None;
    }
    let summary = rejected(&spec, &many_lost, FailureKind::ForceKillP99);
    assert_eq!(summary.force_kill[0].p99_ns, Some(u64::MAX));
    let mut just_under = good(&spec);
    for k in just_under.latency[0].force_kill.iter_mut().take(2) {
        k.ns = Some(999_999_999);
    }
    assert!(evaluate(&spec, &just_under).pass);
}

#[test]
fn too_few_or_invalid_force_kill_samples_are_rejected() {
    let spec = Spec::frozen();
    let mut few = good(&spec);
    few.latency[0].force_kill.pop();
    rejected(&spec, &few, FailureKind::SampleCount);
    let mut over_budget = good(&spec);
    over_budget.latency[0].force_kill[0].ns = Some(spec.force_kill.observe_budget_ns + 1);
    rejected(&spec, &over_budget, FailureKind::ForceKillTimeout);
    let mut zero = good(&spec);
    zero.latency[0].force_kill[0].ns = Some(0);
    rejected(&spec, &zero, FailureKind::InvalidSample);
}

#[test]
fn guard_pss_over_the_limits_is_rejected() {
    let spec = Spec::frozen();
    let mut median = good(&spec);
    for g in &mut median.pss[0].guards {
        g.pss_kib = Some(4097);
    }
    rejected(&spec, &median, FailureKind::PssMedian);
    let mut at_median = good(&spec);
    for g in &mut at_median.pss[0].guards {
        g.pss_kib = Some(4096);
    }
    assert!(evaluate(&spec, &at_median).pass);
    let mut tail = good(&spec);
    for g in tail.pss[0].guards.iter_mut().take(6) {
        g.pss_kib = Some(9_000);
    }
    // 6 of 100: the nearest-rank p95 is the 95th value, so the tail shows.
    let summary = rejected(&spec, &tail, FailureKind::PssP95);
    assert!(
        !kinds(&summary).contains(&FailureKind::PssMedian),
        "the median alone would have hidden the tail"
    );
}

#[test]
fn guard_pss_unreadable_short_or_incomplete_is_rejected() {
    let spec = Spec::frozen();
    let mut unreadable = good(&spec);
    unreadable.pss[0].guards[7].pss_kib = None;
    rejected(&spec, &unreadable, FailureKind::PssMissingReading);
    let mut count = good(&spec);
    count.pss[0].guards.pop();
    rejected(&spec, &count, FailureKind::PssGuardCount);
    let mut hold = good(&spec);
    hold.pss[0].hold_secs = spec.pss.hold_secs - 1;
    rejected(&spec, &hold, FailureKind::PssHoldTooShort);
}

fn conditions() -> serde_json::Value {
    let hex = |c: char, n: usize| c.to_string().repeat(n);
    serde_json::json!({
        "spec_sha256": Spec::sha256(),
        "host": {"uname": "Linux", "kernel": "7", "cpu_model": "x", "cpu_count": 24,
                 "governor": "performance", "mem_total_kib": 1, "loadavg": "0 0 0",
                 "top_cpu": "1.0 x"},
        "toolchain": {"rustc": "1", "cargo": "1", "profile": "release", "libc": "glibc",
                      "rustflags_set": false},
        "tools": {"sh": "/bin/sh", "grep": "grep", "git": "git"},
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
    assert_eq!(check_conditions(&spec, &conditions()), Vec::<String>::new());
}

#[test]
fn incomplete_or_altered_conditions_fail_the_check() {
    let spec = Spec::frozen();
    type Damage = fn(&mut serde_json::Value);
    let damage: [(&str, Damage); 5] = [
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
        ("/host/top_cpu", |c| c["host"]["top_cpu"] = "".into()),
        ("/spec_sha256", |c| c["spec_sha256"] = "0".repeat(64).into()),
        ("/host/loadavg", |c| {
            c["host"].as_object_mut().unwrap().remove("loadavg");
        }),
    ];
    for (pointer, apply) in damage {
        let mut c = conditions();
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
