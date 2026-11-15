//! Measures how long a learning pipeline takes, to choose
//! `core.restart.learning_drain_timeout_secs`.
//!
//! A sample runs from the first event of an extraction run (`lesson`) to the last
//! event of its review run (`learning-evidence-review`), both from the wall clock of
//! the persisted events (`events.run_id`), so mock time never counts.
//!
//! A sample is kept only when it can be shown to be a whole, normally ended pipeline:
//! both runs ended `Done` and have events, and the pairing is unambiguous. A review
//! belongs to an extraction when it starts within `PAIRING_WINDOW` after the
//! extraction ends, and the pairing must hold both ways: exactly one review follows
//! that extraction and exactly one extraction precedes that review. Anything else is
//! counted as unmeasurable and never sampled. An extraction without a review is not
//! a sample either: nothing shows that no review was due.
//!
//! No recommendation is printed until there are `MIN_SAMPLES` samples, and none that
//! exceeds the configurable cap. The database is opened read-only and only counts
//! and durations are printed; no prompt, message body, config or run id leaves this
//! program.
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
const MARGIN: f64 = 1.25;
/// The upper end of `core.restart.learning_drain_timeout_secs`.
const CAP_SECS: u64 = 3600;

struct Span {
    start: i64,
    end: i64,
}

#[derive(Default)]
struct Unmeasurable {
    not_done: usize,
    without_events: usize,
}

/// The spans of every `name` run that ended `Done` and has events.
fn spans(
    conn: &Connection,
    name: &str,
    skipped: &mut Unmeasurable,
) -> Result<Vec<Span>, Box<dyn Error>> {
    let mut statement = conn.prepare(
        "SELECT c.terminal_phase,
                (SELECT MIN(wall_clock_ns) FROM events WHERE run_id = c.run_id),
                (SELECT MAX(wall_clock_ns) FROM events WHERE run_id = c.run_id)
         FROM run_contexts c WHERE c.name = ?1",
    )?;
    let rows = statement.query_map([name], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    })?;
    let mut found = Vec::new();
    for row in rows {
        match row? {
            (phase, _, _) if phase != "Done" => skipped.not_done += 1,
            (_, Some(start), Some(end)) if end >= start => found.push(Span { start, end }),
            _ => skipped.without_events += 1,
        }
    }
    found.sort_by_key(|span| span.start);
    Ok(found)
}

/// Nearest-rank percentile over sorted values.
fn percentile(sorted: &[f64], pct: f64) -> f64 {
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

fn main() -> Result<(), Box<dyn Error>> {
    let path: PathBuf = std::env::args_os()
        .nth(1)
        .ok_or("usage: learning_pipeline_measure <db-path>")?
        .into();
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (mut extract_skipped, mut review_skipped) =
        (Unmeasurable::default(), Unmeasurable::default());
    let extracts = spans(&conn, EXTRACT, &mut extract_skipped)?;
    let reviews = spans(&conn, REVIEW, &mut review_skipped)?;

    let following = |extract: &Span| -> Vec<usize> {
        (0..reviews.len())
            .filter(|&index| {
                let gap = reviews[index].start - extract.end;
                (0..=PAIRING_WINDOW).contains(&gap)
            })
            .collect()
    };
    let preceding = |review: &Span| -> usize {
        extracts
            .iter()
            .filter(|extract| (0..=PAIRING_WINDOW).contains(&(review.start - extract.end)))
            .count()
    };
    let mut samples = Vec::new();
    let (mut ambiguous, mut no_review) = (0_usize, 0_usize);
    for extract in &extracts {
        match following(extract).as_slice() {
            [] => no_review += 1,
            [only] if preceding(&reviews[*only]) == 1 => {
                samples.push((reviews[*only].end - extract.start) as f64 / NS as f64);
            }
            _ => ambiguous += 1,
        }
    }
    println!(
        "unmeasurable: extractions_not_done={} extractions_without_events={} \
         reviews_not_done={} reviews_without_events={} ambiguous_pairings={ambiguous} \
         extractions_without_review={no_review}",
        extract_skipped.not_done,
        extract_skipped.without_events,
        review_skipped.not_done,
        review_skipped.without_events
    );

    samples.sort_by(f64::total_cmp);
    if samples.is_empty() {
        println!("extract+review: n=0");
        println!("recommended_default_secs=insufficient_samples");
        return Ok(());
    }
    let p95 = percentile(&samples, 95.0);
    println!(
        "extract+review: n={} p50={:.1}s p95={p95:.1}s max={:.1}s",
        samples.len(),
        percentile(&samples, 50.0),
        samples[samples.len() - 1]
    );
    if samples.len() < MIN_SAMPLES {
        println!("recommended_default_secs=insufficient_samples (need {MIN_SAMPLES})");
        return Ok(());
    }
    let secs = ((p95 * MARGIN / ROUND_SECS).ceil() * ROUND_SECS) as u64;
    if secs > CAP_SECS {
        println!("recommended_default_secs=exceeds_cap ({secs} > {CAP_SECS})");
    } else {
        println!("recommended_default_secs={secs}");
    }
    Ok(())
}
