//! Words and numbers for what an inspection found. An unknown figure reads
//! "—" and is never shown as zero; a partial total says which part it covers.

use eval::cost::Cost;
use eval::inspection::{
    Artifact, CostSource, CostUnavailable, DatabaseBudget, EvaluationMemberSnapshot, GroupPhase,
    MemberCost, MemberPhase, ProcessStatus, TrialCost, TrialOutcome, TrialState,
};

/// Shown where a value is not known.
pub const UNKNOWN: &str = "—";

pub fn usd(value: f64) -> String {
    if value >= 1.0 {
        format!("${value:.2}")
    } else {
        format!("${value:.4}")
    }
}

/// A cost, with what makes it less than the whole truth.
pub fn cost_text(cost: &Cost) -> String {
    let mut text = usd(cost.usd);
    if cost.is_api_equivalent() {
        text.push_str(" (includes API-equivalent subscription use)");
    }
    let missing = cost.unpriced.len() + cost.unreported.len();
    if missing > 0 {
        text.push_str(&format!(
            ", incomplete: {missing} model(s) unpriced or without usage"
        ));
    }
    text
}

pub fn trial_cost_text(cost: &TrialCost) -> String {
    match cost {
        TrialCost::Reported(cost) => cost_text(cost),
        TrialCost::Estimated(cost) => format!("≈ {} (estimated)", cost_text(cost)),
        TrialCost::Unavailable(_) => UNKNOWN.to_owned(),
    }
}

/// Why a trial has no cost, for a tooltip.
pub fn trial_cost_reason(cost: &TrialCost) -> Option<String> {
    let TrialCost::Unavailable(reason) = cost else {
        return None;
    };
    Some(match reason {
        CostUnavailable::NotFinished => "The trial has not finished.".to_owned(),
        CostUnavailable::NotPriced => "Priced when the report is made.".to_owned(),
        CostUnavailable::NotInReport => "The report has no entry for this trial.".to_owned(),
        CostUnavailable::NoDatabase => "The trial database is not there.".to_owned(),
        CostUnavailable::InUse => {
            "The trial database is being written (a -wal or -journal file sits next to it), so its cost is not read."
                .to_owned()
        }
        CostUnavailable::OverBudget(budget) => match budget {
            DatabaseBudget::Bytes { limit, actual } => {
                format!("The trial database ({actual} bytes) is bigger than the {limit} bytes read for a cost.")
            }
            DatabaseBudget::Rows { limit } => {
                format!("The trial database has more than {limit} usage rows, more than are read for a cost.")
            }
        },
        CostUnavailable::Unreadable(problem) => format!("Unreadable: {problem}"),
    })
}

/// A member's cost: where it comes from, then one line per configuration.
pub fn member_cost_lines(cost: &MemberCost) -> (String, Vec<String>) {
    let source = match cost.source {
        CostSource::Report => "Cost from the report",
        CostSource::Estimate => "Cost estimated from finished trials",
        CostSource::None => "Cost",
    };
    if cost.configurations.is_empty() {
        return (source.to_owned(), vec![UNKNOWN.to_owned()]);
    }
    let lines = cost
        .configurations
        .iter()
        .map(|configuration| {
            let mut line = format!(
                "{}: {}",
                configuration.label,
                cost_text(&configuration.cost)
            );
            if configuration.is_partial() {
                line.push_str(&format!(
                    " — counted trials only ({} of {})",
                    configuration.counted, configuration.planned
                ));
            }
            line
        })
        .collect();
    (source.to_owned(), lines)
}

pub fn group_phase_label(phase: GroupPhase) -> &'static str {
    match phase {
        GroupPhase::Running => "Running",
        GroupPhase::Finished => "Finished",
        GroupPhase::Incomplete => "Incomplete",
        GroupPhase::Unverified => "Unverified",
        GroupPhase::Partial => "Partial",
    }
}

pub fn member_phase_label(phase: MemberPhase) -> &'static str {
    match phase {
        MemberPhase::Starting => "Starting",
        MemberPhase::RunningTrials => "Running trials",
        MemberPhase::Reporting => "Reporting",
        MemberPhase::Finished => "Finished",
        MemberPhase::FinishedWithoutReport => "Finished without a report",
        MemberPhase::Failed => "Failed",
        MemberPhase::Unverified => "Unverified",
    }
}

pub fn trial_state_label(state: TrialState) -> &'static str {
    match state {
        TrialState::Queued => "Queued",
        TrialState::Running => "Running",
        TrialState::Finished => "Finished",
        TrialState::Failed => "Failed",
        TrialState::Interrupted => "Interrupted",
        TrialState::Unavailable => "Not run",
        TrialState::Unverified => "Unverified",
    }
}

/// What "unverified" means, wherever it is shown.
pub const UNVERIFIED_HINT: &str = "Nothing shows that the evaluation process still runs or how it ended: it was started by another session. Its files are shown as they are.";

pub fn process_text(process: &ProcessStatus) -> String {
    match process {
        ProcessStatus::Running => "Running (started by this session)".to_owned(),
        ProcessStatus::Exited(exit) => format!("Ended: {}", exit.describe()),
        ProcessStatus::Unknown => "Unknown".to_owned(),
    }
}

/// Finished over planned trials, from the manifest. Never a percentage.
pub fn progress_text(member: &EvaluationMemberSnapshot) -> String {
    match member.progress {
        Some(progress) => {
            let mut text = format!(
                "{} of {} trials finished",
                progress.finished, progress.planned
            );
            if progress.started > progress.finished {
                text.push_str(&format!(", {} started", progress.started));
            }
            text
        }
        None => format!(
            "{} of {} trials finished (no manifest yet)",
            member.finished_trials(),
            member.planned_trials()
        ),
    }
}

pub fn elapsed_text(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}.{} s", seconds, (milliseconds % 1000) / 100)
    }
}

pub fn outcome_text(outcome: &Artifact<TrialOutcome>) -> String {
    match outcome {
        Artifact::Present(outcome) => {
            let mut parts = vec![
                format!("{:?}", outcome.status),
                format!("{} tool calls", outcome.tool_calls),
                format!("{} user turns", outcome.user_turns),
                format!("{} tokens", outcome.tokens),
                elapsed_text(outcome.elapsed_ms),
            ];
            match outcome.verification_passed {
                Some(true) => parts.push("verification passed".to_owned()),
                Some(false) => parts.push("verification failed".to_owned()),
                None => {}
            }
            if outcome.denied_requests > 0 {
                parts.push(format!("{} requests denied", outcome.denied_requests));
            }
            parts.join(" · ")
        }
        Artifact::Missing => UNKNOWN.to_owned(),
        Artifact::Problem(problem) => format!("outcome unreadable: {problem}"),
    }
}

#[cfg(test)]
mod tests {
    use eval::cost::Unpriced;
    use eval::inspection::ConfigurationCostSnapshot;

    use super::*;

    fn priced(usd: f64) -> Cost {
        Cost {
            usd,
            api_equivalent_usd: 0.0,
            unpriced: Vec::new(),
            unreported: Vec::new(),
        }
    }

    #[test]
    fn an_unknown_cost_is_a_dash_not_zero() {
        let none = MemberCost {
            source: CostSource::None,
            configurations: Vec::new(),
        };
        assert_eq!(member_cost_lines(&none).1, vec![UNKNOWN.to_owned()]);
        let unavailable = TrialCost::Unavailable(CostUnavailable::NotFinished);
        assert_eq!(trial_cost_text(&unavailable), UNKNOWN);
        assert!(trial_cost_reason(&unavailable).is_some());
    }

    #[test]
    fn a_partial_total_says_it_counts_only_some_trials() {
        let cost = MemberCost {
            source: CostSource::Estimate,
            configurations: vec![ConfigurationCostSnapshot {
                label: "baseline".into(),
                counted: 1,
                planned: 3,
                cost: priced(0.5),
            }],
        };
        let (source, lines) = member_cost_lines(&cost);
        assert_eq!(source, "Cost estimated from finished trials");
        assert_eq!(
            lines,
            vec!["baseline: $0.5000 — counted trials only (1 of 3)".to_owned()]
        );
    }

    #[test]
    fn an_incomplete_cost_names_what_is_missing() {
        let mut cost = priced(2.0);
        cost.unpriced.push(Unpriced {
            provider: "p".into(),
            profile: None,
            model: "m".into(),
            requests: 4,
        });
        let text = cost_text(&cost);
        assert!(text.starts_with("$2.00"), "{text}");
        assert!(text.contains("incomplete"), "{text}");
    }

    #[test]
    fn elapsed_time_is_compact() {
        assert_eq!(elapsed_text(12_300), "12.3 s");
        assert_eq!(elapsed_text(125_000), "2m 05s");
    }
}
