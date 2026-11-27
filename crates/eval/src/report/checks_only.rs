//! `evorch eval report --checks-only`: grade the runs of finished evaluation
//! directories against a sample's checks, from their recorded evidence alone.
//!
//! Nothing here asks a provider: no judge, no user simulator, no verification
//! command, no price table. The result goes to its own file named after the
//! sample's hash, so an earlier result, `report.json` and the manifest are
//! never rewritten.

use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use storage::eval_sample::{EvalSample, SampleKind};

use super::{grade_run, load_database, read_json_in};
use crate::checks::{self, Grading};
use crate::driver::Status;
use crate::output::{self, Manifest, Outcome};
use crate::runner::EvalError;

/// Where the checks of a [`ChecksReport`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// The sample the evaluation pinned in its manifest when it started.
    Pinned,
    /// The manifest pinned none (an older evaluation, or one started without a
    /// sample): the sample as it is now. A post-hoc analysis, not what the
    /// evaluation was started with.
    CurrentSample,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunChecks {
    pub run: String,
    pub status: Status,
    pub grading: Grading,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChecksReport {
    pub thread_id: String,
    pub basis: Basis,
    pub sample_hash: String,
    pub kind: SampleKind,
    pub check_version: u32,
    pub checks: Vec<String>,
    pub runs: Vec<RunChecks>,
    pub note: String,
    pub generated_at: String,
}

impl ChecksReport {
    /// The file name of this result, below the evaluation directory.
    #[must_use]
    pub fn file_name(&self, extension: &str) -> String {
        format!(
            "checks-{}.{extension}",
            self.sample_hash.chars().take(12).collect::<String>()
        )
    }

    /// One line per run, for the command line.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "{} ({}): {}",
            self.thread_id,
            match self.basis {
                Basis::Pinned => "checks pinned at the start",
                Basis::CurrentSample => "post-hoc: the current sample",
            },
            self.file_name("md")
        )];
        lines.extend(self.runs.iter().map(|run| {
            format!(
                "  {}: {} ({})",
                run.run,
                run.grading.quality().label(),
                run.grading.tally()
            )
        }));
        lines.join("\n")
    }
}

/// Grade every directory and write `checks-<hash>.json` and `.md` in it.
/// `sample_database` is read, read-only, only for a directory whose manifest
/// pinned no sample.
///
/// # Errors
/// A directory is not an evaluation directory, has no sample with checks to
/// grade against, or its result cannot be written.
pub async fn checks_only_pinned(
    dirs: Vec<output::PinnedDir>,
    sample_database: Option<&Path>,
    mut on_written: impl FnMut(usize),
) -> Result<Vec<ChecksReport>, EvalError> {
    if dirs.is_empty() {
        return Err(EvalError::Usage("at least one --output is required".into()));
    }
    let mut reports = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        let report = checks_dir(dir, sample_database)?;
        dir.write_json(&report.file_name("json"), &report)?;
        dir.write(&report.file_name("md"), markdown(&report).as_bytes())?;
        on_written(index);
        reports.push(report);
    }
    Ok(reports)
}

fn current_sample(
    database: Option<&Path>,
    thread_id: &str,
) -> Result<Option<EvalSample>, EvalError> {
    let Some(path) = database else {
        return Ok(None);
    };
    let database =
        storage::Database::open_read_only(path).map_err(|source| EvalError::Database {
            path: path.to_path_buf(),
            source,
        })?;
    database
        .eval_sample(thread_id)
        .map_err(|source| EvalError::Database {
            path: path.to_path_buf(),
            source,
        })
}

fn checks_dir(
    dir: &output::PinnedDir,
    sample_database: Option<&Path>,
) -> Result<ChecksReport, EvalError> {
    let manifest: Manifest = read_json_in(dir, output::MANIFEST_FILE)
        .map_err(|error| EvalError::Usage(format!("not an evaluation directory: {error}")))?;
    let (basis, sample, hash) = match &manifest.sample {
        Some(pinned) => (Basis::Pinned, pinned.sample.clone(), pinned.hash.clone()),
        None => {
            let sample = current_sample(sample_database, &manifest.thread_id)?.ok_or_else(|| {
                EvalError::Usage(format!(
                    "evaluation of thread {} pinned no sample and the thread has none now: there are no checks to grade",
                    manifest.thread_id
                ))
            })?;
            let hash = sample.content_hash();
            (Basis::CurrentSample, sample, hash)
        }
    };
    if sample.checks.is_empty() {
        return Err(EvalError::Usage(format!(
            "the sample of thread {} declares no checks: no machine judgment",
            manifest.thread_id
        )));
    }
    let mut runs = Vec::new();
    let mut unfinished = 0;
    for entry in &manifest.runs {
        if entry.status.is_none() {
            unfinished += 1;
            continue;
        }
        let run_dir = dir.open_below(&entry.dir)?;
        let outcome: Outcome =
            read_json_in(&run_dir, output::OUTCOME_FILE).map_err(EvalError::Usage)?;
        let record = load_database(&run_dir)?;
        runs.push(RunChecks {
            run: format!("{}-{}", entry.label, entry.index),
            status: outcome.status,
            grading: grade_run(&sample, &record, outcome.status),
        });
    }
    let mut note = match basis {
        Basis::Pinned => {
            "Graded against the sample this evaluation pinned when it started.".to_owned()
        }
        Basis::CurrentSample => format!(
            "Post-hoc analysis: this evaluation pinned no sample, so the checks are the sample as it is now (hash {hash}). The manifest, report.json and earlier results are unchanged."
        ),
    };
    if unfinished > 0 {
        note.push_str(&format!(
            " {unfinished} run(s) had not ended and were not graded."
        ));
    }
    Ok(ChecksReport {
        thread_id: manifest.thread_id,
        basis,
        sample_hash: hash,
        kind: sample.kind,
        check_version: sample.check_version,
        checks: sample.checks.iter().map(checks::describe).collect(),
        runs,
        note,
        generated_at: output::timestamp(SystemTime::now()),
    })
}

fn markdown(report: &ChecksReport) -> String {
    let mut out = format!(
        "# Machine checks of thread {}\n\n{}\n\nSample {} ({}, grading version {}). A check result and the validity of the replay are separate: checks that pass on an unfinished replay are not a quality pass.\n\n",
        report.thread_id,
        report.note,
        report.sample_hash,
        report.kind.as_str(),
        report.check_version
    );
    for (index, check) in report.checks.iter().enumerate() {
        out.push_str(&format!("- check {index}: {check}\n"));
    }
    out.push_str("\n| run | status | quality | checks | replay record |\n|---|---|---|---|---|\n");
    for run in &report.runs {
        let issues = if run.grading.validity.valid {
            "complete".to_owned()
        } else {
            run.grading.validity.issues.join("; ").replace('|', "\\|")
        };
        out.push_str(&format!(
            "| {} | {:?} | {} | {} | {issues} |\n",
            run.run,
            run.status,
            run.grading.quality().label(),
            run.grading.tally()
        ));
    }
    for run in &report.runs {
        for result in run
            .grading
            .results
            .iter()
            .filter(|result| result.verdict != checks::Verdict::Pass)
        {
            out.push_str(&format!(
                "\n- {} check {} {:?}: {}",
                run.run, result.index, result.verdict, result.reason
            ));
        }
    }
    out.push('\n');
    out
}
