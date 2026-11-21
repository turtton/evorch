//! The measured intervals of `measurements/f4-product/conditions.md`. Each
//! function runs one operation once and returns what happened: a duration and
//! an outcome. Nothing is retried, no limit is raised and no latency is asserted.
use super::{
    artifact::{ProductArtifact, hash_pass},
    env::IsolatedEnv,
    launch::ProductLaunch,
    measure::{Outcome, Sample, ms},
    old_core,
};
use core_launcher::handoff::{
    CurrentElf, ProbeLimits,
    targets::{RestartTarget, TargetRequirements, TargetSelection},
};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

pub struct Recorder {
    pub profile: String,
    pub condition: &'static str,
    pub samples: Vec<Sample>,
    pub seq: usize,
}
impl Recorder {
    pub fn new(profile: &str, condition: &'static str) -> Self {
        Self {
            profile: profile.into(),
            condition,
            samples: Vec::new(),
            seq: 0,
        }
    }
    fn push(
        &mut self,
        n: usize,
        interval: &'static str,
        d: Duration,
        outcome: Outcome,
        note: String,
    ) {
        self.seq += 1;
        self.samples.push(Sample {
            condition: self.condition,
            interval,
            profile: self.profile.clone(),
            n,
            millis: ms(d),
            outcome,
            note: format!("seq={} {note}", self.seq).trim().to_owned(),
        });
    }
}

fn outcome<T, E: std::fmt::Display>(r: &Result<T, E>) -> Outcome {
    match r {
        Ok(_) => Outcome::Ok,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// `preflight`, `reprobe_before_capture`, `before_exec_check` for a target that
/// is not the current binary, in the order the coordinator runs them. The
/// `current` argument is only passed through: with a non-`current` target the
/// launcher never reads it.
pub async fn launcher_chain(
    rec: &mut Recorder,
    n: usize,
    artifact: &ProductArtifact,
    current: &CurrentElf,
) {
    let description = artifact.describe().expect("describe for requirements");
    let schema = *storage::supported_schema_versions().unwrap().end();
    let limits = ProbeLimits::default();
    let start = Instant::now();
    let preflight = RestartTarget::preflight(
        TargetSelection {
            id: "target",
            configured_path: Some(&artifact.path),
            timeout: Duration::from_secs(300),
        },
        current,
        TargetRequirements {
            handoff: 1,
            snapshot: 1,
            storage_schema: schema,
            protocol: description.protocol,
            platform: &description.platform,
        },
        limits,
        std::future::pending(),
    )
    .await;
    rec.push(
        n,
        "preflight",
        start.elapsed(),
        outcome(&preflight),
        String::new(),
    );
    let Ok(target) = preflight else { return };
    let start = Instant::now();
    let checked = target
        .before_capture(current, limits, std::future::pending())
        .await;
    rec.push(
        n,
        "reprobe_before_capture",
        start.elapsed(),
        outcome(&checked),
        String::new(),
    );
    let Ok(checked) = checked else { return };
    let start = Instant::now();
    let before_exec = checked.before_exec(current).map(|_| ());
    rec.push(
        n,
        "before_exec_check",
        start.elapsed(),
        outcome(&before_exec),
        String::new(),
    );
}

pub fn describe_probe_run(rec: &mut Recorder, n: usize, artifact: &ProductArtifact) {
    let start = Instant::now();
    let result = artifact.describe();
    rec.push(
        n,
        "describe_probe_run",
        start.elapsed(),
        outcome(&result),
        String::new(),
    );
}

pub fn hash_unit(rec: &mut Recorder, n: usize, artifact: &ProductArtifact) {
    let result = hash_pass(&artifact.path);
    let (d, o) = match &result {
        Ok(d) => (*d, Outcome::Ok),
        Err(e) => (Duration::ZERO, Outcome::Failed(e.to_string())),
    };
    rec.push(n, "hash_pass", d, o, String::new());
}

/// Product spawned through its ordinary entry, until its readiness line.
pub fn cold_spawn_to_ready(rec: &mut Recorder, n: usize, artifact: &ProductArtifact) {
    let env = IsolatedEnv::new();
    let provider = super::wire::MockProvider::scripted(vec![]);
    env.write_product_config(&provider.base_url(), super::launch::KEY_ENV);
    let launched = ProductLaunch::new(artifact, &env).start();
    let mut product = match launched {
        Ok(p) => p,
        Err(e) => {
            rec.push(
                n,
                "cold_spawn_to_ready",
                Duration::ZERO,
                Outcome::Failed(e.to_string()),
                String::new(),
            );
            return;
        }
    };
    let started = product.started;
    let ready = product.wait_ready();
    let (d, o) = match &ready {
        Ok(r) => (r.at.saturating_duration_since(started), Outcome::Ok),
        Err(e) => (started.elapsed(), Outcome::Failed(format!("{e}"))),
    };
    rec.push(n, "cold_spawn_to_ready", d, o, String::new());
    let (report, footprint) = product.stop();
    assert!(report.orphans.is_empty(), "{:?}", report.orphans);
    drop(env);
    assert!(footprint.remains().is_empty());
}

/// One harness old-core -> exec -> product cycle: `exec_to_ready` and
/// `exec_to_claim`.
pub fn exec_cycle(rec: &mut Recorder, n: usize, artifact: &ProductArtifact, harness: &Path) {
    match old_core::exec_cycle(artifact, harness) {
        Ok(c) => {
            let note = format!(
                "claim_stable={} lease_ms={} grace_ms={} exe_replaced={}",
                c.claim_stable, c.lease_ms, c.grace_ms, c.exe_replaced
            );
            rec.push(
                n,
                "exec_to_ready",
                c.exec_to_ready,
                Outcome::Ok,
                note.clone(),
            );
            rec.push(
                n,
                "exec_to_claim",
                Duration::from_secs_f64(c.exec_to_claim_ms.max(0) as f64 / 1000.0),
                Outcome::Ok,
                format!("{note} raw_ms={}", c.exec_to_claim_ms),
            );
        }
        Err((error, elapsed)) => {
            // A failure keeps its own duration and the interval that was not
            // reached stays visible as a failure, never as a missing row.
            let note = format!("after_ms={:.3}", ms(elapsed));
            let invalid = matches!(error, old_core::CycleError::Invalid(_));
            let why = if invalid {
                format!("invalid: {error}")
            } else {
                error.to_string()
            };
            rec.push(
                n,
                "exec_to_ready",
                elapsed,
                Outcome::Failed(why),
                note.clone(),
            );
            rec.push(
                n,
                "exec_to_claim",
                elapsed,
                Outcome::Failed("not reached: the exec cycle failed".into()),
                note,
            );
        }
    }
}

/// A byte-identical copy of the artifact at a new path (new inode), verified
/// like any artifact. A sealed product is copied as its whole closure (the core
/// is never copied alone). A measurement-only artifact is copied as a core
/// and manifest, then the prepared guard is bound to that exact private ELF.
pub fn fresh_copy(artifact: &ProductArtifact, dir: &Path) -> ProductArtifact {
    if let Some(sealed) = &artifact.sealed {
        let copied = sealed
            .copy_closure_into(dir)
            .unwrap_or_else(|reason| panic!("copy the sealed closure: {reason}"));
        return ProductArtifact::from_sealed(&copied, &|name| std::env::var_os(name))
            .expect("the closure copy verifies");
    }
    let copy = dir.join("evorch-core");
    std::fs::copy(&artifact.path, &copy).expect("copy the artifact");
    test_support::guard_bundle::bind_copy(&copy, "core").expect("bind measurement-only copy");
    let mut manifest = artifact.manifest.clone();
    manifest.artifact = "evorch-core".into();
    let path = dir.join("manifest.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    ProductArtifact::open(&path, &|name| std::env::var_os(name)).expect("the copy verifies")
}

/// One position of the fixed order of conditions.md.
pub async fn one_round(
    rec: &mut Recorder,
    n: usize,
    artifact: &ProductArtifact,
    current: &CurrentElf,
    harness: &Path,
) {
    rec.seq = 0;
    launcher_chain(rec, n, artifact, current).await;
    describe_probe_run(rec, n, artifact);
    hash_unit(rec, n, artifact);
    cold_spawn_to_ready(rec, n, artifact);
    exec_cycle(rec, n, artifact, harness);
}

pub fn current_elf() -> Arc<CurrentElf> {
    Arc::new(CurrentElf::record().expect("record the harness ELF"))
}
