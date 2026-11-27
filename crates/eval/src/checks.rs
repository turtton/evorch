//! Deterministic grading of a sample's checks on recorded events (ADR 0036).
//!
//! The same function grades the thread as it was recorded (when a sample is
//! saved and inspected) and the runs of an evaluation (when it is reported).
//! It reads events only: no provider, judge, simulator or verification command
//! is involved. A check that cannot be decided from what was recorded is
//! `indeterminate`; an unobserved condition never counts as a pass.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::SystemTime;

use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent, ToolEvent};
use serde::{Deserialize, Serialize};
use storage::eval_sample::SourceRef;
use storage::eval_sample::{CHECK_VERSION, CallRange, Check, CheckScope, EvalSample, SampleKind};

use crate::driver::Status;
use crate::roles;

/// The most evidence references kept for one check.
const EVIDENCE_MAX: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Indeterminate,
}

/// Where in the record a result comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    /// The position of the check in the sample.
    pub index: usize,
    pub description: String,
    pub verdict: Verdict,
    /// The matching occurrences that were observed (successes, errors or diagnostics).
    pub observed: u32,
    pub reason: String,
    pub evidence: Vec<EvidenceRef>,
    /// Evidence references left out to keep the result bounded.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub evidence_omitted: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Whether the record is a complete account of the replay, apart from any
/// single check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validity {
    pub valid: bool,
    pub issues: Vec<String>,
}

/// What the checks of a sample concluded, and whether the record they were
/// read from is a complete account of the replay. The two are separate: all
/// checks passing on an unfinished replay is not a quality pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grading {
    pub check_version: u32,
    pub results: Vec<CheckResult>,
    pub validity: Validity,
}

/// The one-word outcome of a grading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// The sample declares no checks.
    NoMachineJudgment,
    Pass,
    Fail,
    Indeterminate,
    /// Every check passed, but the record is not a complete account of the replay.
    ReplayInvalid,
}

impl Quality {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoMachineJudgment => "no machine judgment",
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Indeterminate => "indeterminate",
            Self::ReplayInvalid => "replay invalid",
        }
    }
}

impl Grading {
    /// The worst verdict of the checks, or `None` when there are none.
    #[must_use]
    pub fn verdict(&self) -> Option<Verdict> {
        let has = |wanted| self.results.iter().any(|result| result.verdict == wanted);
        if self.results.is_empty() {
            None
        } else if has(Verdict::Fail) {
            Some(Verdict::Fail)
        } else if has(Verdict::Indeterminate) {
            Some(Verdict::Indeterminate)
        } else {
            Some(Verdict::Pass)
        }
    }

    #[must_use]
    pub fn quality(&self) -> Quality {
        match self.verdict() {
            None => Quality::NoMachineJudgment,
            Some(Verdict::Fail) => Quality::Fail,
            Some(Verdict::Indeterminate) => Quality::Indeterminate,
            Some(Verdict::Pass) if self.validity.valid => Quality::Pass,
            Some(Verdict::Pass) => Quality::ReplayInvalid,
        }
    }

    fn count(&self, wanted: Verdict) -> usize {
        self.results
            .iter()
            .filter(|result| result.verdict == wanted)
            .count()
    }

    /// `3 checks: 2 pass, 1 fail, 0 indeterminate`.
    #[must_use]
    pub fn tally(&self) -> String {
        format!(
            "{} checks: {} pass, {} fail, {} indeterminate",
            self.results.len(),
            self.count(Verdict::Pass),
            self.count(Verdict::Fail),
            self.count(Verdict::Indeterminate)
        )
    }
}

/// What the checks are graded on.
#[derive(Debug, Clone, Copy)]
pub struct Observation<'a> {
    pub events: &'a [Event],
    /// Events were left out because the read limit was reached.
    pub truncated: bool,
    /// How a replay ended; `None` for a thread read as it was recorded.
    pub status: Option<Status>,
    /// The file or database the events come from is not there, so `events`
    /// is empty because nothing could be read, not because nothing happened.
    pub evidence_missing: bool,
}

/// One tool call, assembled from its start and terminal events.
struct Call {
    run_id: Option<String>,
    call_id: String,
    tool: String,
    parent_call_id: Option<String>,
    /// `Some(is_error)` once the call's terminal event was seen.
    outcome: Option<bool>,
}

struct Diagnostic {
    run_id: Option<String>,
    call_id: Option<String>,
    code: String,
}

struct Record {
    /// Role address of every run that started.
    run_roles: HashMap<String, String>,
    calls: Vec<Call>,
    diagnostics: Vec<Diagnostic>,
    /// Runs that started and whose recorded phase is not an end: still `Pending`
    /// or `Running`, or with no phase recorded at all.
    open_runs: BTreeSet<String>,
}

fn role_key(role: &str) -> String {
    roles::address(role).map_or_else(|| role.to_ascii_lowercase(), str::to_owned)
}

/// What identifies a diagnostic: run, call, code, detail and time.
type DiagnosticKey = (Option<String>, Option<String>, String, String, SystemTime);

fn read_record(events: &[Event]) -> Record {
    let mut run_roles = HashMap::new();
    let mut calls: Vec<Call> = Vec::new();
    let mut by_key: HashMap<(Option<String>, String), usize> = HashMap::new();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut seen_diagnostics: HashSet<DiagnosticKey> = HashSet::new();
    let mut phases: HashMap<&str, AgentRunPhase> = HashMap::new();
    for event in events {
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted { run_id, role, .. }) => {
                run_roles.insert(run_id.clone(), role_key(role));
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) => {
                phases.insert(run_id, *to);
            }
            EventKind::Tool(ToolEvent::ToolStarted {
                tool_name,
                call_id,
                run_id,
                parent_call_id,
                ..
            }) => {
                let key = (run_id.clone(), call_id.clone());
                if let std::collections::hash_map::Entry::Vacant(slot) = by_key.entry(key) {
                    slot.insert(calls.len());
                    calls.push(Call {
                        run_id: run_id.clone(),
                        call_id: call_id.clone(),
                        tool: tool_name.clone(),
                        parent_call_id: parent_call_id.clone(),
                        outcome: None,
                    });
                }
            }
            EventKind::Tool(ToolEvent::ToolCompleted {
                tool_name,
                call_id,
                is_error,
                run_id,
                parent_call_id,
                ..
            }) => {
                let key = (run_id.clone(), call_id.clone());
                let index = *by_key.entry(key).or_insert_with(|| {
                    calls.push(Call {
                        run_id: run_id.clone(),
                        call_id: call_id.clone(),
                        tool: tool_name.clone(),
                        parent_call_id: parent_call_id.clone(),
                        outcome: None,
                    });
                    calls.len() - 1
                });
                // A second terminal event for the same call is a duplicate.
                calls[index].outcome.get_or_insert(*is_error);
            }
            EventKind::Diagnostic(diagnostic) => {
                let fingerprint = (
                    diagnostic.run_id.clone(),
                    diagnostic.call_id.clone(),
                    diagnostic.code.clone(),
                    diagnostic.detail.clone(),
                    event.meta.wall_clock,
                );
                if seen_diagnostics.insert(fingerprint) {
                    diagnostics.push(Diagnostic {
                        run_id: diagnostic.run_id.clone(),
                        call_id: diagnostic.call_id.clone(),
                        code: diagnostic.code.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    let open_runs = run_roles
        .keys()
        .map(String::as_str)
        .chain(phases.keys().copied())
        .filter(|run| {
            phases.get(run).is_none_or(|phase| {
                matches!(phase, AgentRunPhase::Pending | AgentRunPhase::Running)
            })
        })
        .map(str::to_owned)
        .collect();
    Record {
        run_roles,
        calls,
        diagnostics,
        open_runs,
    }
}

/// Why nothing can be concluded from the absence of an occurrence in a
/// record that is missing or holds no run at all.
fn record_gaps(observation: &Observation<'_>, record: &Record) -> Option<&'static str> {
    if observation.evidence_missing {
        Some("the recorded evidence is missing")
    } else if record.run_roles.is_empty() {
        Some("the record holds no run start, so it is empty or incomplete")
    } else {
        None
    }
}

fn validity(observation: &Observation<'_>, record: &Record) -> Validity {
    let mut issues = Vec::new();
    issues.extend(record_gaps(observation, record).map(str::to_owned));
    if observation.truncated {
        issues.push("events were left out because the read limit was reached".to_owned());
    }
    if let Some(status) = observation.status
        && status != Status::Completed
    {
        issues.push(format!("the replay did not complete ({status:?})"));
    }
    if !record.open_runs.is_empty() {
        issues.push(format!(
            "{} run(s) have no recorded end when the record ends",
            record.open_runs.len()
        ));
    }
    let unfinished = record
        .calls
        .iter()
        .filter(|call| call.outcome.is_none())
        .count();
    if unfinished > 0 {
        issues.push(format!("{unfinished} tool call(s) have no terminal event"));
    }
    Validity {
        valid: issues.is_empty(),
        issues,
    }
}

/// Whether a run is in a check's scope.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Membership {
    In,
    Out,
    Unknown,
}

fn membership(scope: &CheckScope, run_id: Option<&str>, record: &Record) -> Membership {
    match scope {
        CheckScope::Tree => Membership::In,
        CheckScope::Roles(wanted) => {
            let role = run_id.and_then(|run| record.run_roles.get(run));
            match role {
                None => Membership::Unknown,
                Some(role) if wanted.iter().any(|name| &role_key(name) == role) => Membership::In,
                Some(_) => Membership::Out,
            }
        }
    }
}

fn role_absent(scope: &CheckScope, record: &Record) -> bool {
    match scope {
        CheckScope::Tree => false,
        CheckScope::Roles(wanted) => !wanted.iter().any(|name| {
            let wanted = role_key(name);
            record.run_roles.values().any(|role| *role == wanted)
        }),
    }
}

fn describe_scope(scope: &CheckScope) -> String {
    match scope {
        CheckScope::Tree => "whole delegation tree".to_owned(),
        CheckScope::Roles(roles) => format!("roles {}", roles.join(", ")),
    }
}

fn describe_calls(calls: CallRange) -> &'static str {
    match calls {
        CallRange::Direct => "direct calls",
        CallRange::IncludingChildren => "including calls made inside other tools",
    }
}

#[must_use]
pub fn describe(check: &Check) -> String {
    match check {
        Check::SuccessfulCalls {
            tool,
            min,
            max,
            scope,
            calls,
        } => {
            let bounds = match (min, max) {
                (Some(min), Some(max)) => format!("between {min} and {max}"),
                (Some(min), None) => format!("at least {min}"),
                (None, Some(max)) => format!("at most {max}"),
                (None, None) => "any number of".to_owned(),
            };
            format!(
                "{bounds} successful `{tool}` calls ({}; {})",
                describe_scope(scope),
                describe_calls(*calls)
            )
        }
        Check::ToolErrors {
            tool,
            max,
            scope,
            calls,
        } => format!(
            "at most {max} `{tool}` errors ({}; {})",
            describe_scope(scope),
            describe_calls(*calls)
        ),
        Check::Diagnostics { code, max, scope } => format!(
            "at most {max} `{code}` diagnostics ({})",
            describe_scope(scope)
        ),
    }
}

/// What was counted for one check.
#[derive(Default)]
struct Tally {
    observed: u32,
    evidence: Vec<EvidenceRef>,
    omitted: u32,
    /// Matching calls with no terminal event, in scope.
    unfinished: u32,
    /// Matches whose run could not be attributed to a role.
    unattributed: u32,
}

impl Tally {
    fn add(&mut self, run_id: Option<&String>, call_id: Option<&String>, detail: &str) {
        self.observed += 1;
        if self.evidence.len() < EVIDENCE_MAX {
            self.evidence.push(EvidenceRef {
                run_id: run_id.cloned(),
                call_id: call_id.cloned(),
                detail: detail.to_owned(),
            });
        } else {
            self.omitted += 1;
        }
    }
}

fn count_calls(
    record: &Record,
    tool: &str,
    scope: &CheckScope,
    range: CallRange,
    wanted_error: bool,
) -> Tally {
    let mut tally = Tally::default();
    for call in &record.calls {
        if call.tool != tool || (range == CallRange::Direct && call.parent_call_id.is_some()) {
            continue;
        }
        match membership(scope, call.run_id.as_deref(), record) {
            Membership::Out => {}
            Membership::Unknown => tally.unattributed += 1,
            Membership::In => match call.outcome {
                None => tally.unfinished += 1,
                Some(is_error) if is_error == wanted_error => tally.add(
                    call.run_id.as_ref(),
                    Some(&call.call_id),
                    if is_error {
                        "call ended in an error"
                    } else {
                        "call succeeded"
                    },
                ),
                Some(_) => {}
            },
        }
    }
    tally
}

/// Reasons the absence of an occurrence cannot be concluded.
fn doubts(
    tally: &Tally,
    scope: &CheckScope,
    record: &Record,
    observation: &Observation<'_>,
    check_open_calls: bool,
) -> Vec<String> {
    let mut doubts = Vec::new();
    doubts.extend(record_gaps(observation, record).map(str::to_owned));
    if observation.truncated {
        doubts.push("events were left out because the read limit was reached".to_owned());
    }
    if let Some(status) = observation.status
        && status != Status::Completed
    {
        doubts.push(format!("the replay did not complete ({status:?})"));
    }
    if !record.open_runs.is_empty() {
        doubts.push("a run has no recorded end when the record ends".to_owned());
    }
    if check_open_calls && tally.unfinished > 0 {
        doubts.push(format!(
            "{} matching call(s) have no terminal event",
            tally.unfinished
        ));
    }
    if tally.unattributed > 0 {
        doubts.push(format!(
            "{} match(es) belong to a run whose role is unknown",
            tally.unattributed
        ));
    }
    if role_absent(scope, record) {
        doubts.push("none of the roles of the check ever ran".to_owned());
    }
    doubts
}

fn result(
    index: usize,
    check: &Check,
    verdict: Verdict,
    tally: Tally,
    reason: String,
) -> CheckResult {
    CheckResult {
        index,
        description: describe(check),
        verdict,
        observed: tally.observed,
        reason,
        evidence: tally.evidence,
        evidence_omitted: tally.omitted,
    }
}

fn grade_check(
    index: usize,
    check: &Check,
    record: &Record,
    observation: &Observation<'_>,
) -> CheckResult {
    match check {
        Check::SuccessfulCalls {
            tool,
            min,
            max,
            scope,
            calls,
        } => {
            let tally = count_calls(record, tool, scope, *calls, false);
            let observed = tally.observed;
            if let Some(max) = max
                && observed > *max
            {
                let reason =
                    format!("{observed} successful calls were observed, over the maximum of {max}");
                return result(index, check, Verdict::Fail, tally, reason);
            }
            let doubts = doubts(&tally, scope, record, observation, true);
            if let Some(min) = min
                && observed < *min
            {
                return if doubts.is_empty() {
                    let reason =
                        format!("only {observed} successful calls, under the minimum of {min}");
                    result(index, check, Verdict::Fail, tally, reason)
                } else {
                    let reason = format!(
                        "{observed} successful calls so far, under the minimum of {min}, but {}",
                        doubts.join("; ")
                    );
                    result(index, check, Verdict::Indeterminate, tally, reason)
                };
            }
            if max.is_some() && !doubts.is_empty() {
                let reason = format!(
                    "{observed} successful calls so far, within the bounds, but {}",
                    doubts.join("; ")
                );
                return result(index, check, Verdict::Indeterminate, tally, reason);
            }
            let reason = format!("{observed} successful calls, within the bounds");
            result(index, check, Verdict::Pass, tally, reason)
        }
        Check::ToolErrors {
            tool,
            max,
            scope,
            calls,
        } => {
            let tally = count_calls(record, tool, scope, *calls, true);
            let observed = tally.observed;
            if observed > *max {
                let reason = format!("{observed} errors were observed, over the maximum of {max}");
                return result(index, check, Verdict::Fail, tally, reason);
            }
            let doubts = doubts(&tally, scope, record, observation, true);
            if doubts.is_empty() {
                let reason = format!("{observed} errors, within the maximum of {max}");
                result(index, check, Verdict::Pass, tally, reason)
            } else {
                let reason = format!(
                    "{observed} errors so far, within the maximum of {max}, but {}",
                    doubts.join("; ")
                );
                result(index, check, Verdict::Indeterminate, tally, reason)
            }
        }
        Check::Diagnostics { code, max, scope } => {
            let mut tally = Tally::default();
            for diagnostic in record
                .diagnostics
                .iter()
                .filter(|found| &found.code == code)
            {
                match membership(scope, diagnostic.run_id.as_deref(), record) {
                    Membership::Out => {}
                    Membership::Unknown => tally.unattributed += 1,
                    Membership::In => tally.add(
                        diagnostic.run_id.as_ref(),
                        diagnostic.call_id.as_ref(),
                        "diagnostic raised",
                    ),
                }
            }
            let observed = tally.observed;
            if observed > *max {
                let reason =
                    format!("{observed} diagnostics were observed, over the maximum of {max}");
                return result(index, check, Verdict::Fail, tally, reason);
            }
            let doubts = doubts(&tally, scope, record, observation, false);
            if doubts.is_empty() {
                let reason = format!("{observed} diagnostics, within the maximum of {max}");
                result(index, check, Verdict::Pass, tally, reason)
            } else {
                let reason = format!(
                    "{observed} diagnostics so far, within the maximum of {max}, but {}",
                    doubts.join("; ")
                );
                result(index, check, Verdict::Indeterminate, tally, reason)
            }
        }
    }
}

/// Grade every check of `sample` on `observation`. A sample whose checks were
/// written for another grading version is not graded: each check comes back
/// `indeterminate`.
#[must_use]
pub fn grade(sample: &EvalSample, observation: &Observation<'_>) -> Grading {
    let record = read_record(observation.events);
    let validity = validity(observation, &record);
    let results = if sample.check_version == CHECK_VERSION {
        sample
            .checks
            .iter()
            .enumerate()
            .map(|(index, check)| grade_check(index, check, &record, observation))
            .collect()
    } else {
        sample
            .checks
            .iter()
            .enumerate()
            .map(|(index, check)| CheckResult {
                index,
                description: describe(check),
                verdict: Verdict::Indeterminate,
                observed: 0,
                reason: format!(
                    "the checks are for grading version {}, this build grades {CHECK_VERSION}",
                    sample.check_version
                ),
                evidence: Vec::new(),
                evidence_omitted: 0,
            })
            .collect()
    };
    Grading {
        check_version: sample.check_version,
        results,
        validity,
    }
}

/// Whether `grading` of the recorded thread shows that `sample` is fit to be
/// kept: a failure sample's checks must catch the failure, and a regression
/// sample's checks must pass on a complete record. A sample without checks is
/// always accepted; it just gets no machine judgment.
///
/// # Errors
/// Why the sample is refused, naming the results behind it.
pub fn accept(sample: &EvalSample, grading: &Grading) -> Result<(), String> {
    if sample.checks.is_empty() {
        return Ok(());
    }
    let lines = |wanted: &dyn Fn(&CheckResult) -> bool| {
        grading
            .results
            .iter()
            .filter(|result| wanted(result))
            .map(|result| {
                format!(
                    "check {} ({:?}): {}",
                    result.index, result.verdict, result.reason
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    match sample.kind {
        SampleKind::Failure => {
            if grading
                .results
                .iter()
                .any(|result| result.verdict == Verdict::Fail)
            {
                Ok(())
            } else {
                Err(format!(
                    "no check fails on the recorded thread, so the checks cannot detect the failure the sample is for: {}",
                    lines(&|_| true)
                ))
            }
        }
        SampleKind::Regression => {
            let not_passing = lines(&|result| result.verdict != Verdict::Pass);
            if !not_passing.is_empty() {
                Err(format!(
                    "a regression sample's checks must all pass on the recorded thread: {not_passing}"
                ))
            } else if !grading.validity.valid {
                Err(format!(
                    "the recorded thread is not a complete account, so passing proves nothing: {}",
                    grading.validity.issues.join("; ")
                ))
            } else {
                Ok(())
            }
        }
    }
}

/// Check that every reference names something of the thread whose runs are `runs`.
///
/// # Errors
/// The first reference that does not exist or belongs to another thread, or a
/// database read error.
pub fn verify_sources(
    database: &storage::Database,
    runs: &[String],
    refs: &[SourceRef],
) -> Result<(), String> {
    for reference in refs {
        match reference {
            SourceRef::Run { run_id } => {
                if !runs.contains(run_id) {
                    return Err(format!("run {run_id} is not a run of this thread"));
                }
            }
            SourceRef::Event { event_id } => {
                let (events, _) = database
                    .events_for_runs_between(runs, event_id - 1, *event_id, 1)
                    .map_err(|error| error.to_string())?;
                if !events.iter().any(|event| event.id == *event_id) {
                    return Err(format!(
                        "event {event_id} does not exist or is not an event of a run of this thread"
                    ));
                }
            }
            SourceRef::Improvement { candidate_id } => {
                let candidate = database
                    .improvement_candidate(candidate_id)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| {
                        format!("improvement candidate {candidate_id} does not exist")
                    })?;
                let names_a_run = candidate
                    .run_id
                    .iter()
                    .chain(&candidate.recent_run_ids)
                    .any(|run| runs.contains(run));
                if !names_a_run {
                    return Err(format!(
                        "improvement candidate {candidate_id} names no run of this thread"
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
