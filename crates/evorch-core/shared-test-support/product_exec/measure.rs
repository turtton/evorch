//! Raw observations and nearest-rank quantiles. Nothing here changes a deadline
//! or asserts on a latency: a sample is a number (or an outcome), the report
//! says what was seen.
use std::{io::Write, path::Path, time::Duration};

#[derive(Debug, Clone)]
pub enum Outcome {
    Ok,
    /// The operation failed; the duration is the time to the failure.
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Sample {
    pub condition: &'static str, // "first" | "warm"
    pub interval: &'static str,
    pub profile: String,
    pub n: usize,
    pub millis: f64,
    pub outcome: Outcome,
    /// Free-form qualifiers (e.g. `claim_stable=true`).
    pub note: String,
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Nearest rank: sorted sample at `ceil(p*n)-1`.
pub fn quantile(sorted: &[f64], p: f64) -> f64 {
    assert!(!sorted.is_empty());
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub n: usize,
    pub failures: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}
pub fn summarize(samples: &[&Sample]) -> Option<Summary> {
    let mut ok: Vec<f64> = samples
        .iter()
        .filter(|s| matches!(s.outcome, Outcome::Ok))
        .map(|s| s.millis)
        .collect();
    if ok.is_empty() {
        return None;
    }
    ok.sort_by(f64::total_cmp);
    Some(Summary {
        n: samples.len(),
        failures: samples.len() - ok.len(),
        min: ok[0],
        p50: quantile(&ok, 0.5),
        p95: quantile(&ok, 0.95),
        max: *ok.last().unwrap(),
    })
}

pub fn write_csv(path: &Path, samples: &[Sample]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    writeln!(
        file,
        "profile,condition,interval,n,milliseconds,outcome,note"
    )?;
    for s in samples {
        let outcome = match &s.outcome {
            Outcome::Ok => "ok".to_owned(),
            Outcome::Failed(why) => format!("failed:{}", why.replace([',', '\n'], " ")),
        };
        writeln!(
            file,
            "{},{},{},{},{:.3},{},{}",
            s.profile,
            s.condition,
            s.interval,
            s.n,
            s.millis,
            outcome,
            s.note.replace([',', '\n'], " ")
        )?;
    }
    file.sync_all()
}
