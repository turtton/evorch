//! Measures how long a learning pipeline takes, to choose
//! `core.restart.learning_drain_timeout_secs`.
//!
//! A pipeline runs from the start of its extraction run (`lesson`) to the end of its
//! review run (`learning-evidence-review`), or to the end of the extraction when the
//! extractor staged nothing and no review followed. Both ends come from the wall clock
//! of the persisted events of those runs (`events.run_id`), so mock time never counts.
//!
//! The database is opened read-only and only counts and durations are printed. No
//! prompt, message body, config or run id leaves this program.
//!
//! A review is paired with the extraction that ended last before it started, within
//! `PAIRING_WINDOW`; a review that fits no extraction, and an extraction or review
//! without events (for example purged by retention), are counted as unmeasurable.
//!
//! Run: `cargo run --release -p storage --example learning_pipeline_measure -- <db>`.

use std::error::Error;
use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};

const EXTRACT: &str = "lesson";
const REVIEW: &str = "learning-evidence-review";
const NS: i64 = 1_000_000_000;
/// A review starts right after its extraction ends; a larger gap means another run.
const PAIRING_WINDOW: i64 = 600 * NS;
const MIN_SAMPLES: usize = 30;
const ROUND_SECS: f64 = 30.0;

struct Span {
    start: i64,
    end: i64,
}

fn spans(conn: &Connection, name: &str) -> Result<(Vec<Span>, usize), Box<dyn Error>> {
    let mut statement = conn.prepare(
        "SELECT (SELECT MIN(wall_clock_ns) FROM events WHERE run_id = c.run_id),
                (SELECT MAX(wall_clock_ns) FROM events WHERE run_id = c.run_id)
         FROM run_contexts c WHERE c.name = ?1",
    )?;
    let rows = statement.query_map([name], |row| {
        Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    let (mut found, mut missing) = (Vec::new(), 0);
    for row in rows {
        match row? {
            (Some(start), Some(end)) if end >= start => found.push(Span { start, end }),
            _ => missing += 1,
        }
    }
    found.sort_by_key(|span| span.start);
    Ok((found, missing))
}

/// Nearest-rank percentile over sorted values.
fn percentile(sorted: &[f64], pct: f64) -> f64 {
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

fn report(kind: &str, mut secs: Vec<f64>) -> Option<f64> {
    if secs.is_empty() {
        println!("{kind}: n=0");
        return None;
    }
    secs.sort_by(f64::total_cmp);
    let p95 = percentile(&secs, 95.0);
    println!(
        "{kind}: n={} p50={:.1}s p95={:.1}s max={:.1}s enough_samples={}",
        secs.len(),
        percentile(&secs, 50.0),
        p95,
        secs[secs.len() - 1],
        secs.len() >= MIN_SAMPLES
    );
    Some(p95)
}

fn main() -> Result<(), Box<dyn Error>> {
    let path: PathBuf = std::env::args_os()
        .nth(1)
        .ok_or("usage: learning_pipeline_measure <db-path>")?
        .into();
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (extracts, extract_missing) = spans(&conn, EXTRACT)?;
    let (reviews, review_missing) = spans(&conn, REVIEW)?;

    let mut claimed = vec![false; reviews.len()];
    let mut with_review = Vec::new();
    let mut extract_only = Vec::new();
    for extract in &extracts {
        let paired = reviews
            .iter()
            .enumerate()
            .filter(|(index, review)| {
                !claimed[*index]
                    && review.start >= extract.end
                    && review.start - extract.end <= PAIRING_WINDOW
            })
            .min_by_key(|(_, review)| review.start - extract.end);
        match paired {
            Some((index, review)) => {
                claimed[index] = true;
                with_review.push((review.end - extract.start) as f64 / NS as f64);
            }
            None => extract_only.push((extract.end - extract.start) as f64 / NS as f64),
        }
    }
    let unpaired_reviews = claimed.iter().filter(|claimed| !**claimed).count();

    let p95s: Vec<f64> = [
        report("extract+review", with_review),
        report("extract-only", extract_only),
    ]
    .into_iter()
    .flatten()
    .collect();
    println!(
        "unmeasurable: extractions_without_events={extract_missing} \
         reviews_without_events={review_missing} unpaired_reviews={unpaired_reviews}"
    );
    match p95s.into_iter().reduce(f64::max) {
        Some(p95) => println!(
            "recommended_default_secs={}",
            ((p95 * 1.25 / ROUND_SECS).ceil() * ROUND_SECS) as u64
        ),
        None => println!("recommended_default_secs=none"),
    }
    Ok(())
}
