//! D-3a2 B3-09 performance gate: result schema and aggregator.
//!
//! Pure data and arithmetic. No process, clock, file or product code, so the
//! rules that decide pass/fail are unit-tested apart from the runner
//! (tests/perf_gate_aggregate.rs) and the runner cannot soften them.
//!
//! Rules (the plan, section 5; design v3 section 8.2):
//! - nearest-rank quantiles on integers (per-mille, no float rounding);
//! - every round is judged alone; rounds are never pooled and an invalid,
//!   missing, duplicated or failed round fails the gate;
//! - no outlier is removed: every sample the runner recorded is used;
//! - the A/B order of every pair must equal the sequence derived from the
//!   spec seed, so a result cannot be re-ordered after the fact;
//! - a sample that timed out (no exit observed) is a failure, not a skip.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The frozen thresholds and counts. Read by the runner and the aggregator.
pub const SPEC_JSON: &str = include_str!("../../perf-gate/spec.json");
pub const SCHEMA: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub schema: u32,
    pub rounds: u32,
    pub warmup_per_arm: u32,
    pub pairs: u32,
    pub seed: u64,
    pub delta_definition: DeltaDefinition,
    pub workloads: Vec<Workload>,
    pub latency_limits: LatencyLimits,
    pub force_kill: ForceKillSpec,
    pub pss: PssSpec,
    pub scope: ScopeSpec,
    pub state: StateSpec,
    #[serde(default)]
    pub source: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaDefinition {
    /// pXX(B) - pXX(A) of the two marginal distributions.
    QuantileDifference,
    /// pXX of the per-pair differences B - A.
    PairedDifferenceQuantile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workload {
    pub name: String,
    pub command: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyLimits {
    pub p50_delta_ns: i64,
    pub p95_delta_ns: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForceKillSpec {
    pub samples_min: u32,
    /// The measured p99 must be strictly below this.
    pub p99_limit_ns: u64,
    /// A kill whose root exit is not seen within this is a failed sample.
    pub observe_budget_ns: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PssSpec {
    pub guards: u32,
    pub hold_secs: u32,
    pub median_limit_kib: u64,
    pub p95_limit_kib: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeSpec {
    pub cpus: u32,
    pub memory_bytes: u64,
    pub tasks: u64,
    pub timeout_secs: u32,
    pub stress_cpus: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSpec {
    pub idle_threads: u32,
    pub messages_per_thread: u32,
    pub message_bytes: u32,
    pub repo_files: u32,
    pub repo_file_bytes: u32,
    pub active_agents: u32,
    pub tick_ms: u32,
    pub fixture_txt_bytes: u32,
}

impl Spec {
    pub fn frozen() -> Spec {
        serde_json::from_str(SPEC_JSON).expect("perf-gate/spec.json is valid")
    }
    pub fn sha256() -> String {
        hex(&Sha256::digest(SPEC_JSON.as_bytes()))
    }
}
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// ---------------------------------------------------------------- statistics

/// Nearest-rank quantile: the value at 1-based rank ceil(p * n), `p` in
/// per-mille. `None` for an empty input. The input need not be sorted.
pub fn nearest_rank<T: Copy + Ord>(values: &[T], per_mille: u32) -> Option<T> {
    if values.is_empty() || per_mille == 0 || per_mille > 1000 {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len() as u128;
    let rank = (u128::from(per_mille) * n).div_ceil(1000).max(1);
    Some(sorted[(rank - 1) as usize])
}

// -------------------------------------------------------------------- order

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arm {
    A,
    B,
}
fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}
/// The arm measured first in pair `index` of `workload` in `round`: ABBA
/// alternation (A B | B A | A B | ...) whose starting arm is derived from the
/// frozen seed per (round, workload).
pub fn expected_first(seed: u64, round: u32, workload: usize, index: u32) -> Arm {
    let start = splitmix(seed ^ (u64::from(round) << 32) ^ ((workload as u64) << 8)) & 1;
    let phase = (u64::from(index) % 4 == 1 || u64::from(index) % 4 == 2) as u64;
    if (start ^ phase) == 0 { Arm::A } else { Arm::B }
}

/// The A/B order of one (round, workload) as the string stored in conditions.json.
pub fn order_string(spec: &Spec, round: u32, workload: usize) -> String {
    (0..spec.pairs)
        .map(|i| {
            if expected_first(spec.seed, round, workload, i) == Arm::A {
                'A'
            } else {
                'B'
            }
        })
        .collect()
}

/// What conditions.json must carry before any sample is taken: host, toolchain,
/// both arms' source and artifact identities, the fixture, the runner, the
/// stress tool and the frozen order. Returns every problem found.
pub fn check_conditions(spec: &Spec, c: &serde_json::Value) -> Vec<String> {
    let mut problems = Vec::new();
    let text = |pointer: &str| {
        c.pointer(pointer)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
    };
    let hex = |pointer: &str, len: usize, problems: &mut Vec<String>| match text(pointer) {
        Some(s) if s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit()) => {}
        _ => problems.push(format!("{pointer}: expected {len} hex digits")),
    };
    for pointer in [
        "/host/uname",
        "/host/kernel",
        "/host/cpu_model",
        "/host/governor",
        "/host/loadavg",
        "/toolchain/rustc",
        "/toolchain/cargo",
        "/toolchain/profile",
        "/toolchain/libc",
        "/tools/sh",
        "/tools/grep",
        "/tools/git",
        "/stress_ng",
        "/arms/a/build_recipe",
        "/arms/b/build_recipe",
        "/arms/a/exe_path",
        "/arms/b/exe_path",
    ] {
        if text(pointer).is_none() {
            problems.push(format!("{pointer}: missing or empty"));
        }
    }
    for pointer in [
        "/host/cpu_count",
        "/host/mem_total_kib",
        "/toolchain/rustflags_set",
        "/seed",
    ] {
        if c.pointer(pointer).is_none() {
            problems.push(format!("{pointer}: missing"));
        }
    }
    hex("/spec_sha256", 64, &mut problems);
    if text("/spec_sha256") != Some(Spec::sha256().as_str()) {
        problems.push("/spec_sha256: differs from the frozen spec".into());
    }
    for pointer in [
        "/arms/a/source_sha",
        "/arms/a/tree_sha",
        "/arms/b/source_sha",
        "/arms/b/tree_sha",
        "/fixtures/repo_commit",
    ] {
        hex(pointer, 40, &mut problems);
    }
    for pointer in [
        "/arms/a/exe_sha256",
        "/arms/b/exe_sha256",
        "/arms/b/helper_sha256",
        "/arms/b/bundle_sha256",
        "/runner/sha256",
    ] {
        hex(pointer, 64, &mut problems);
    }
    if c.pointer("/seed").and_then(|v| v.as_u64()) != Some(spec.seed) {
        problems.push("/seed: differs from the spec seed".into());
    }
    for round in 1..=spec.rounds {
        for (w, workload) in spec.workloads.iter().enumerate() {
            let pointer = format!("/order/{round}/{}", workload.name);
            if text(&pointer) != Some(order_string(spec, round, w).as_str()) {
                problems.push(format!(
                    "{pointer}: missing or not the frozen ABBA sequence"
                ));
            }
        }
    }
    problems
}

// ------------------------------------------------------------------- inputs

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CellStatus {
    Ok,
    Failed { reason: String },
    TimedOut { reason: String },
}

/// What the runner read back from the cgroup of the process that measured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeReadback {
    pub cpu_max: String,
    pub memory_max: u64,
    pub swap_max: u64,
    pub pids_max: u64,
    /// `stress-ng --version` plus its resolved path, or `None`.
    pub stress_ng: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairSample {
    pub workload: String,
    pub index: u32,
    pub first: Arm,
    pub a_ns: u64,
    pub b_ns: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillSample {
    pub index: u32,
    /// Request sent -> root exit event; `None` when none was seen in budget.
    pub ns: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyRound {
    pub schema: u32,
    /// `false` for a smoke run. A smoke file can never pass the gate.
    pub evidence: bool,
    pub round: u32,
    pub conditions_sha256: String,
    pub spec_sha256: String,
    pub status: CellStatus,
    pub scope: ScopeReadback,
    pub samples: Vec<PairSample>,
    pub force_kill: Vec<KillSample>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardPss {
    pub pid: u32,
    /// `Pss` of /proc/<guard>/smaps_rollup; `None` when it could not be read.
    pub pss_kib: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PssRound {
    pub schema: u32,
    pub evidence: bool,
    pub round: u32,
    pub conditions_sha256: String,
    pub spec_sha256: String,
    pub status: CellStatus,
    pub scope: ScopeReadback,
    pub hold_secs: u32,
    pub guards: Vec<GuardPss>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub conditions_sha256: String,
    /// Findings of [`check_conditions`] on conditions.json; any makes the gate fail.
    #[serde(default)]
    pub conditions_problems: Vec<String>,
    pub latency: Vec<LatencyRound>,
    pub pss: Vec<PssRound>,
}

// ------------------------------------------------------------------ outputs

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    NotEvidence,
    SchemaMismatch,
    SpecMismatch,
    ConditionsMismatch,
    ScopeMismatch,
    MissingRound,
    DuplicateRound,
    UnexpectedRound,
    RoundFailed,
    SampleCount,
    UnknownWorkload,
    OrderMismatch,
    InvalidSample,
    LatencyP50,
    LatencyP95,
    ForceKillTimeout,
    ForceKillP99,
    PssGuardCount,
    PssMissingReading,
    PssHoldTooShort,
    PssMedian,
    PssP95,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub kind: FailureKind,
    pub cell: String,
    pub round: Option<u32>,
    pub workload: Option<String>,
    pub detail: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadVerdict {
    pub round: u32,
    pub workload: String,
    pub pairs: u32,
    pub a_p50_ns: u64,
    pub a_p95_ns: u64,
    pub b_p50_ns: u64,
    pub b_p95_ns: u64,
    pub delta_p50_ns: i64,
    pub delta_p95_ns: i64,
    /// Reported for the definition the spec did not choose; never gates.
    pub other_delta_p50_ns: i64,
    pub other_delta_p95_ns: i64,
    pub pass: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillVerdict {
    pub round: u32,
    pub samples: u32,
    pub p99_ns: Option<u64>,
    pub pass: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PssVerdict {
    pub round: u32,
    pub guards: u32,
    pub median_kib: Option<u64>,
    pub p95_kib: Option<u64>,
    pub pass: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub schema: u32,
    pub spec_sha256: String,
    pub conditions_sha256: String,
    pub pass: bool,
    pub latency: Vec<WorkloadVerdict>,
    pub force_kill: Vec<KillVerdict>,
    pub pss: Vec<PssVerdict>,
    pub failures: Vec<Failure>,
}

struct Ctx<'a> {
    spec: &'a Spec,
    failures: Vec<Failure>,
}
impl Ctx<'_> {
    fn fail(
        &mut self,
        kind: FailureKind,
        cell: &'static str,
        round: u32,
        workload: Option<&str>,
        detail: impl Into<String>,
    ) {
        self.failures.push(Failure {
            kind,
            cell: cell.to_owned(),
            round: Some(round),
            workload: workload.map(str::to_owned),
            detail: detail.into(),
        });
    }
    fn envelope(
        &mut self,
        cell: &'static str,
        round: u32,
        meta: (u32, bool, &str, &str, &CellStatus, &ScopeReadback),
        conditions: &str,
    ) -> bool {
        let (schema, evidence, spec_sha, cond_sha, status, scope) = meta;
        let before = self.failures.len();
        if !evidence {
            self.fail(
                FailureKind::NotEvidence,
                cell,
                round,
                None,
                "smoke or non-evidence file",
            );
        }
        if schema != SCHEMA {
            self.fail(
                FailureKind::SchemaMismatch,
                cell,
                round,
                None,
                format!("schema {schema}"),
            );
        }
        if spec_sha != Spec::sha256() {
            self.fail(
                FailureKind::SpecMismatch,
                cell,
                round,
                None,
                "spec digest differs from the frozen spec",
            );
        }
        if cond_sha != conditions {
            self.fail(
                FailureKind::ConditionsMismatch,
                cell,
                round,
                None,
                "conditions digest differs",
            );
        }
        match status {
            CellStatus::Ok => {}
            CellStatus::Failed { reason } => {
                self.fail(
                    FailureKind::RoundFailed,
                    cell,
                    round,
                    None,
                    format!("failed: {reason}"),
                );
            }
            CellStatus::TimedOut { reason } => {
                self.fail(
                    FailureKind::RoundFailed,
                    cell,
                    round,
                    None,
                    format!("timed out: {reason}"),
                );
            }
        }
        let s = &self.spec.scope;
        let want_cpu = format!("{} 100000", u64::from(s.cpus) * 100_000);
        if scope.cpu_max != want_cpu
            || scope.memory_max != s.memory_bytes
            || scope.swap_max != 0
            || scope.pids_max != s.tasks
            || scope.stress_ng.is_none()
        {
            self.fail(
                FailureKind::ScopeMismatch,
                cell,
                round,
                None,
                format!("scope readback {scope:?}"),
            );
        }
        self.failures.len() == before
    }
}

fn rounds_present<T>(
    ctx: &mut Ctx<'_>,
    cell: &'static str,
    items: &[T],
    round_of: impl Fn(&T) -> u32,
) {
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        let round = round_of(item);
        if !seen.insert(round) {
            ctx.fail(
                FailureKind::DuplicateRound,
                cell,
                round,
                None,
                "round given twice",
            );
        }
        if round == 0 || round > ctx.spec.rounds {
            ctx.fail(
                FailureKind::UnexpectedRound,
                cell,
                round,
                None,
                "round outside 1..=rounds",
            );
        }
    }
    for round in 1..=ctx.spec.rounds {
        if !seen.contains(&round) {
            ctx.fail(FailureKind::MissingRound, cell, round, None, "round absent");
        }
    }
}

fn latency(ctx: &mut Ctx<'_>, conditions: &str, round: &LatencyRound, out: &mut Summary) {
    let r = round.round;
    let spec = ctx.spec;
    let ok = ctx.envelope(
        "latency",
        r,
        (
            round.schema,
            round.evidence,
            &round.spec_sha256,
            &round.conditions_sha256,
            &round.status,
            &round.scope,
        ),
        conditions,
    );
    if !ok {
        return;
    }
    for (w_idx, workload) in spec.workloads.iter().enumerate() {
        let samples: Vec<&PairSample> = round
            .samples
            .iter()
            .filter(|s| s.workload == workload.name)
            .collect();
        let name = Some(workload.name.as_str());
        if samples.len() != spec.pairs as usize {
            ctx.fail(
                FailureKind::SampleCount,
                "latency",
                r,
                name,
                format!("{} pairs, expected {}", samples.len(), spec.pairs),
            );
            continue;
        }
        let mut order_bad = None;
        let mut invalid = None;
        for (i, s) in samples.iter().enumerate() {
            if s.index as usize != i {
                order_bad = Some(format!("pair {i} carries index {}", s.index));
            } else if s.first != expected_first(spec.seed, r, w_idx, s.index) {
                order_bad = Some(format!("pair {i} measured {:?} first", s.first));
            }
            if s.a_ns == 0 || s.b_ns == 0 {
                invalid = Some(format!("pair {i} has a zero duration"));
            }
        }
        if let Some(detail) = order_bad {
            ctx.fail(FailureKind::OrderMismatch, "latency", r, name, detail);
            continue;
        }
        if let Some(detail) = invalid {
            ctx.fail(FailureKind::InvalidSample, "latency", r, name, detail);
            continue;
        }
        let a: Vec<u64> = samples.iter().map(|s| s.a_ns).collect();
        let b: Vec<u64> = samples.iter().map(|s| s.b_ns).collect();
        let diffs: Vec<i64> = samples
            .iter()
            .map(|s| s.b_ns as i64 - s.a_ns as i64)
            .collect();
        let q = |v: &[u64], p| nearest_rank(v, p).expect("non-empty");
        let (a50, a95, b50, b95) = (q(&a, 500), q(&a, 950), q(&b, 500), q(&b, 950));
        let marginal = (b50 as i64 - a50 as i64, b95 as i64 - a95 as i64);
        let paired = (
            nearest_rank(&diffs, 500).expect("non-empty"),
            nearest_rank(&diffs, 950).expect("non-empty"),
        );
        let (d50, d95, o50, o95) = match spec.delta_definition {
            DeltaDefinition::QuantileDifference => (marginal.0, marginal.1, paired.0, paired.1),
            DeltaDefinition::PairedDifferenceQuantile => {
                (paired.0, paired.1, marginal.0, marginal.1)
            }
        };
        let p50_ok = d50 <= spec.latency_limits.p50_delta_ns;
        let p95_ok = d95 <= spec.latency_limits.p95_delta_ns;
        if !p50_ok {
            ctx.fail(
                FailureKind::LatencyP50,
                "latency",
                r,
                name,
                format!("delta p50 {d50} ns"),
            );
        }
        if !p95_ok {
            ctx.fail(
                FailureKind::LatencyP95,
                "latency",
                r,
                name,
                format!("delta p95 {d95} ns"),
            );
        }
        out.latency.push(WorkloadVerdict {
            round: r,
            workload: workload.name.clone(),
            pairs: spec.pairs,
            a_p50_ns: a50,
            a_p95_ns: a95,
            b_p50_ns: b50,
            b_p95_ns: b95,
            delta_p50_ns: d50,
            delta_p95_ns: d95,
            other_delta_p50_ns: o50,
            other_delta_p95_ns: o95,
            pass: p50_ok && p95_ok,
        });
    }
    for s in &round.samples {
        if !spec.workloads.iter().any(|w| w.name == s.workload) {
            ctx.fail(
                FailureKind::UnknownWorkload,
                "latency",
                r,
                Some(&s.workload),
                "not in the spec",
            );
        }
    }
    kill(ctx, round, out);
}

fn kill(ctx: &mut Ctx<'_>, round: &LatencyRound, out: &mut Summary) {
    let r = round.round;
    let spec = &ctx.spec.force_kill;
    let n = round.force_kill.len() as u32;
    let mut pass = true;
    if n < spec.samples_min {
        ctx.fail(
            FailureKind::ForceKillTimeout,
            "force_kill",
            r,
            None,
            format!("{n} samples, at least {} required", spec.samples_min),
        );
        ctx.failures.last_mut().expect("just pushed").kind = FailureKind::SampleCount;
        pass = false;
    }
    let lost = round.force_kill.iter().filter(|k| k.ns.is_none()).count();
    let over = round
        .force_kill
        .iter()
        .filter(|k| k.ns.is_some_and(|v| v > spec.observe_budget_ns))
        .count();
    if lost + over > 0 {
        ctx.fail(
            FailureKind::ForceKillTimeout,
            "force_kill",
            r,
            None,
            format!("{lost} samples without a root exit, {over} beyond the observe budget"),
        );
        pass = false;
    }
    let zero = round.force_kill.iter().any(|k| k.ns == Some(0));
    if zero {
        ctx.fail(
            FailureKind::InvalidSample,
            "force_kill",
            r,
            None,
            "a zero duration",
        );
        pass = false;
    }
    // A lost sample is not dropped: it counts as "never", i.e. the largest value.
    let values: Vec<u64> = round
        .force_kill
        .iter()
        .map(|k| k.ns.unwrap_or(u64::MAX))
        .collect();
    let p99 = nearest_rank(&values, 990);
    if let Some(p) = p99
        && p >= spec.p99_limit_ns
    {
        ctx.fail(
            FailureKind::ForceKillP99,
            "force_kill",
            r,
            None,
            format!("p99 {p} ns"),
        );
        pass = false;
    }
    out.force_kill.push(KillVerdict {
        round: r,
        samples: n,
        p99_ns: p99,
        pass,
    });
}

fn pss(ctx: &mut Ctx<'_>, conditions: &str, round: &PssRound, out: &mut Summary) {
    let r = round.round;
    let spec = &ctx.spec.pss;
    let ok = ctx.envelope(
        "pss",
        r,
        (
            round.schema,
            round.evidence,
            &round.spec_sha256,
            &round.conditions_sha256,
            &round.status,
            &round.scope,
        ),
        conditions,
    );
    if !ok {
        return;
    }
    let mut pass = true;
    if round.hold_secs < spec.hold_secs {
        ctx.fail(
            FailureKind::PssHoldTooShort,
            "pss",
            r,
            None,
            format!("held {} s, required {} s", round.hold_secs, spec.hold_secs),
        );
        pass = false;
    }
    let guards = round.guards.len() as u32;
    if guards != spec.guards {
        ctx.fail(
            FailureKind::PssGuardCount,
            "pss",
            r,
            None,
            format!("{guards} guards, expected {}", spec.guards),
        );
        pass = false;
    }
    let values: Vec<u64> = round.guards.iter().filter_map(|g| g.pss_kib).collect();
    if values.len() != round.guards.len() {
        ctx.fail(
            FailureKind::PssMissingReading,
            "pss",
            r,
            None,
            format!(
                "{} of {} guards unreadable",
                round.guards.len() - values.len(),
                round.guards.len()
            ),
        );
        pass = false;
    }
    let (median, p95) = (nearest_rank(&values, 500), nearest_rank(&values, 950));
    if median.is_some_and(|m| m > spec.median_limit_kib) {
        ctx.fail(
            FailureKind::PssMedian,
            "pss",
            r,
            None,
            format!("median {} KiB", median.unwrap_or(0)),
        );
        pass = false;
    }
    if p95.is_some_and(|m| m > spec.p95_limit_kib) {
        ctx.fail(
            FailureKind::PssP95,
            "pss",
            r,
            None,
            format!("p95 {} KiB", p95.unwrap_or(0)),
        );
        pass = false;
    }
    out.pss.push(PssVerdict {
        round: r,
        guards,
        median_kib: median,
        p95_kib: p95,
        pass,
    });
}

/// Judge the evidence of a whole gate run. Passes only when no failure was
/// recorded: every expected round of both cells present, valid and inside the
/// limits on its own.
pub fn evaluate(spec: &Spec, evidence: &Evidence) -> Summary {
    let mut ctx = Ctx {
        spec,
        failures: Vec::new(),
    };
    let mut out = Summary {
        schema: SCHEMA,
        spec_sha256: Spec::sha256(),
        conditions_sha256: evidence.conditions_sha256.clone(),
        pass: false,
        latency: Vec::new(),
        force_kill: Vec::new(),
        pss: Vec::new(),
        failures: Vec::new(),
    };
    for problem in &evidence.conditions_problems {
        ctx.failures.push(Failure {
            kind: FailureKind::ConditionsMismatch,
            cell: "conditions".into(),
            round: None,
            workload: None,
            detail: problem.clone(),
        });
    }
    rounds_present(&mut ctx, "latency", &evidence.latency, |r| r.round);
    rounds_present(&mut ctx, "pss", &evidence.pss, |r| r.round);
    for round in &evidence.latency {
        latency(&mut ctx, &evidence.conditions_sha256, round, &mut out);
    }
    for round in &evidence.pss {
        pss(&mut ctx, &evidence.conditions_sha256, round, &mut out);
    }
    out.pass = ctx.failures.is_empty();
    out.failures = ctx.failures;
    out
}
