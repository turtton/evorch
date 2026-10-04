use std::path::Path;

use serde::{Deserialize, Serialize};

use super::BenchmarkResult;
use super::storage::{Baseline, Trial, read_json, write_json};

/// Mechanical outcomes plus baseline downstream evidence for human evaluation.
/// The evidence is not injected into candidate replay or scored by another LLM.
#[derive(Debug, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub baseline: Baseline,
    pub trials: Vec<Trial>,
    pub incomplete_trials: Vec<String>,
    pub scope: String,
}

pub fn read_report(directory: &Path) -> BenchmarkResult<BenchmarkReport> {
    let baseline = read_json(&directory.join("baseline.json"))?;
    let mut trials = Vec::new();
    let mut incomplete = Vec::new();
    if directory.join("trials").exists() {
        let mut entries =
            std::fs::read_dir(directory.join("trials"))?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if entry.path().join("trial.json").exists() {
                trials.push(read_json(&entry.path().join("trial.json"))?);
            } else {
                incomplete.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    Ok(BenchmarkReport {
        baseline, trials, incomplete_trials: incomplete,
        scope: "Frozen local delegation replay. Baseline downstream and final results are evaluation evidence only. Local results do not estimate whole-task improvement or the effect of replacing every occurrence of a category. No LLM contextual score or monetary cost estimate is computed.".into(),
    })
}

pub(crate) fn save_report(directory: &Path) -> BenchmarkResult<()> {
    let report = read_report(directory)?;
    write_json(&directory.join("report.json"), &report)?;
    std::fs::write(directory.join("report.md"), report.markdown())?;
    Ok(())
}

impl BenchmarkReport {
    pub fn markdown(&self) -> String {
        let mut output = format!("# Delegation benchmark\n\n{}\n\n", self.scope);
        if let Some(reason) = &self.baseline.poisoned {
            output.push_str(&format!("Record poisoned: {}. Create a fresh record; this workspace cannot be replayed.\n\n", reason));
        }
        output.push_str("| Trial | Model | Phase | Trusted verifier | Input tokens | Output tokens | Time (ms) |\n| --- | --- | --- | --- | ---: | ---: | ---: |\n");
        output.push_str(&row(&self.baseline.full));
        if let Some(local) = &self.baseline.local {
            output.push_str(&row(local));
        }
        for trial in &self.trials {
            output.push_str(&row(trial));
        }
        output.push_str("\nBaseline full-task time is end-to-end runtime time and includes downstream execution. All local row times are the sum of provider request durations, excluding runner setup and trusted verification. Prices are not recorded.\n");
        if let Some(checkpoint) = &self.baseline.checkpoint {
            output.push_str(&format!("\n## Assigned delegation\n\nRole: `{}`; category: `{}`; baseline model: `{}`.\n\n{}\n", checkpoint.role.name(), checkpoint.category.as_deref().unwrap_or("none"), checkpoint.selected_model, fence(&checkpoint.prompt)));
        } else {
            output.push_str(
                "\nNo matching checkpoint was captured; this baseline cannot be replayed.\n",
            );
        }
        output.push_str(&format!("\n## Baseline final context\n\n{}\n\nVerifier evidence:\n\n{}\n\nThe full downstream trace is in `baseline-events.json`; the final diff and local outputs are included in `report.json`.\n", fence(self.baseline.full.final_text.as_deref().unwrap_or("No final output")), fence(&self.baseline.full.evaluation.output)));
        for trial in self.baseline.local.iter().chain(self.trials.iter()) {
            output.push_str(&format!(
                "\n## {}\n\nOutput:\n\n{}\n\nDiff:\n\n{}\n\nTrusted verifier:\n\n{}\n",
                trial.name,
                fence(trial.final_text.as_deref().unwrap_or("No output")),
                fence(&trial.diff),
                fence(&trial.evaluation.output)
            ));
            if let Some(error) = &trial.error {
                output.push_str(&format!("\nExecution error: {}\n", fence(error)));
            }
            let rejected: Vec<_> = trial
                .evaluation
                .verifier_tampered
                .iter()
                .chain(&trial.evaluation.unexpected_changes)
                .map(|path| path.display().to_string())
                .collect();
            if !rejected.is_empty() {
                output.push_str(&format!("\nRejected files: {}\n", rejected.join(", ")));
            }
        }
        if !self.incomplete_trials.is_empty() {
            output.push_str(&format!(
                "\nIncomplete trials (retained, not omitted): {}\n",
                self.incomplete_trials.join(", ")
            ));
        }
        output
    }
}

fn row(trial: &Trial) -> String {
    let model = trial
        .candidate
        .as_ref()
        .map(|candidate| {
            format!(
                "{}/{}",
                candidate.profile,
                candidate.model.as_deref().unwrap_or("default")
            )
        })
        .unwrap_or_else(|| "baseline".into());
    let outcome = if !trial.evaluation.verifier_tampered.is_empty()
        || !trial.evaluation.unexpected_changes.is_empty()
    {
        "tampered"
    } else if trial.evaluation.passed {
        "pass"
    } else {
        "fail"
    };
    format!(
        "| {} | {} | {:?} | {} | {} | {} | {} |\n",
        trial.name,
        model.replace('|', "\\|"),
        trial.phase,
        outcome,
        trial.metrics.input_tokens,
        trial.metrics.output_tokens,
        trial.metrics.elapsed_ms
    )
}

fn fence(text: &str) -> String {
    // A malicious model output cannot terminate a fixed Markdown code fence.
    let fence = "`".repeat(
        text.split(|c| c != '`')
            .map(str::len)
            .max()
            .unwrap_or(0)
            .max(2)
            + 1,
    );
    format!("{fence}\n{text}\n{fence}")
}
