//! The report's comparison, split by what it measures (ADR 0036 §8): the
//! initial system and tools size, the first request's measured usage, the
//! cumulative usage, quality, the effective differences, cost by payer, and
//! whether the comparison is valid or must be held pending.
//!
//! A number that is missing is unknown, never zero: an unreported usage, an
//! unpriced model or a replay without a database leaves the figure `None`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{AspectTally, ConfigurationCost, LoadedRun, Overhead, PairReport, incomplete_reason};
use crate::changes::{Changes, ToolDescriptionChange};
use crate::cost::{Cost, RequestUsage};
use crate::driver::Status;
use crate::output::{BaseContextSummary, Observed, RunEntry};
use crate::roles;
use crate::runner::CANDIDATE;

/// How an initial size was obtained, said wherever the number is shown.
pub const ESTIMATE_NOTE: &str = "estimated by the runtime from byte counts (bytes / 4), \
not a provider's token count";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Breakdown {
    /// The system prompt and tools each replay's own runtime composed.
    pub initial_context: Vec<InitialContext>,
    /// What the provider reported for each replay's first request.
    pub first_request: Vec<FirstRequest>,
    /// Every request of each replay, the initial context and the history alike.
    pub cumulative: Vec<Cumulative>,
    pub quality: QualityBreakdown,
    pub effective_changes: EffectiveChanges,
    pub cost: CostBreakdown,
    pub validity: ComparisonValidity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitialContext {
    pub run: String,
    /// `None`: the replay's runtime did not record it, so it is unknown.
    pub context: Option<BaseContextSummary>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirstUsage {
    /// All input tokens, cached ones included.
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
}

/// The measured usage of a replay's first request. It also counts the task
/// text and whatever else the request carried, so it is not the tools' size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirstRequest {
    pub run: String,
    /// The first attempt, whether or not it succeeded; unknown when it failed
    /// or reported no usage.
    pub usage: Option<FirstUsage>,
    /// Why `usage` is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown: Option<String>,
    /// The first request that succeeded, when the first attempt did not: a
    /// retry, kept apart so that it never stands for the first request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_success: Option<FirstUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cumulative {
    pub run: String,
    pub requests: u32,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    /// `false` when some request reported no usage: the sums are then a lower
    /// bound.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunQuality {
    pub run: String,
    pub status: Status,
    /// The grading of the sample's checks; `None` without checks.
    pub checks: Option<String>,
    pub checks_tally: Option<String>,
    /// Whether the record is a complete account of the replay, apart from any
    /// check; `None` without checks.
    pub replay_valid: Option<bool>,
    pub verification: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QualityBreakdown {
    pub runs: Vec<RunQuality>,
    pub judged_pairs: u32,
    pub unjudged_pairs: u32,
    /// Per aspect: wins, ties both orders gave and order disagreements.
    pub aspects: Vec<AspectTally>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveDescription {
    pub profile: String,
    pub route_model_id: String,
    pub tool: String,
    pub baseline_hash: String,
    pub candidate_hash: String,
    /// Candidate replays in which a run froze this description.
    pub applied_in: Vec<String>,
    pub not_applied_in: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveChanges {
    pub bindings: Vec<String>,
    pub affected_roles: Vec<String>,
    pub tool_descriptions: Vec<EffectiveDescription>,
    /// Requests of an affected role that went to a model the changed
    /// descriptions are not for.
    pub fallbacks: Vec<String>,
}

/// One payer's cost: `usd` is `None` when any part of it is unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostLine {
    pub usd: Option<f64>,
    /// The priced part, a lower bound when `usd` is unknown.
    pub priced_usd: f64,
    pub api_equivalent: bool,
    pub requests: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown: Option<String>,
}

impl CostLine {
    fn of(cost: &Cost, requests: u32) -> Self {
        let unknown = (!cost.is_complete()).then(|| incomplete_reason(cost));
        Self {
            usd: unknown.is_none().then_some(cost.usd),
            priced_usd: cost.usd,
            api_equivalent: cost.is_api_equivalent(),
            requests,
            unknown,
        }
    }

    pub fn text(&self) -> String {
        match (&self.usd, &self.unknown) {
            (Some(usd), _) => format!(
                "${usd:.4}{}",
                if self.api_equivalent {
                    " (API-equivalent)"
                } else {
                    ""
                }
            ),
            (None, reason) => format!(
                "unknown (priced part ${:.4}; {})",
                self.priced_usd,
                reason.as_deref().unwrap_or("unknown")
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabeledCost {
    pub label: String,
    pub cost: CostLine,
}

/// What each payer spent: the replayed agents per configuration, the
/// simulated user, and the judge, apart from one another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub agent: Vec<LabeledCost>,
    pub simulator: CostLine,
    pub judge: CostLine,
    pub total: CostLine,
}

impl Default for CostBreakdown {
    fn default() -> Self {
        let none = || CostLine::of(&Cost::default(), 0);
        Self {
            agent: Vec::new(),
            simulator: none(),
            judge: none(),
            total: none(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingKind {
    /// A changed description was not used by any run of the replay.
    NotApplied,
    /// Requests of an affected role went to another model.
    ModelFallback,
    /// The replay left no readable record of what it used.
    EvidenceMissing,
    ReplayIncomplete,
    /// A replay the evaluation was to make was never started or did not finish.
    ReplayMissing,
    /// The evaluation's preflight refused it: nothing was compared.
    PreflightFailed,
    /// The checks are indeterminate, or passed on a replay whose record is incomplete.
    ChecksNotConclusive,
    JudgeOrderDisagreement,
    JudgingFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub kind: PendingKind,
    pub subject: String,
    pub reason: String,
}

/// Whether the comparison can be read as a result. Anything pending holds the
/// judgment: it is neither a pass nor a regression.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonValidity {
    pub valid: bool,
    pub pending: Vec<Pending>,
}

/// What a replay really used of the described differences, read from its
/// run's own record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunEffect {
    /// Whether the replay recorded what it used at all.
    pub observed: bool,
    /// Per changed description, whether some run froze it.
    pub applied: Vec<bool>,
    /// The runs that froze at least one changed description.
    pub applied_runs: BTreeSet<String>,
    pub fallbacks: Vec<String>,
}

fn base_model(id: &str) -> &str {
    config::types::provider::parse_model_speed(id).0
}

/// What `entry`'s replay used of the description changes in `changes`.
///
/// A description counts as applied only when all four hold:
///
/// 1. the run's frozen snapshot selects the change's (profile, route model ID),
///    taken from the candidate's own configuration, and lists the tool;
/// 2. the tool is among the `tool_names` the run's own record offered, so the
///    run's role and its being a child are accounted for (and its role is one
///    the change reaches);
/// 3. every request of the role was answered by the (profile, base model) the
///    role's own runs were fixed to, as their snapshots record. This reads the
///    usage ledger's actual profile and model, never the fallback events, and is
///    checked for baseline and candidate alike. A role whose runs do not fix
///    exactly one known model cannot be told from a fallback, and is held
///    (`fallbacks`);
/// 4. the description reaches the model's wire: `offered` already holds only
///    the tools that pass `BuiltinCatalog::description_reaches_wire` for the
///    edit interface of the run's profile and model.
///
/// When any of it cannot be told, nothing counts as applied.
pub fn effect_of(changes: &Changes, entry: Option<&RunEntry>) -> RunEffect {
    let observed: Option<&Observed> = entry.and_then(|entry| entry.observed.as_ref());
    let Some(observed) = observed else {
        return RunEffect {
            observed: false,
            applied: vec![false; changes.tool_descriptions.len()],
            ..RunEffect::default()
        };
    };
    let used = |applied: &crate::output::AppliedDescriptions, change: &ToolDescriptionChange| {
        applied.profile.as_deref() == Some(change.profile.as_str())
            && applied.route_model_id.as_deref() == Some(change.route_model_id.as_str())
            && applied.tools.contains(&change.tool)
            && applied
                .offered
                .as_ref()
                .is_some_and(|offered| offered.contains(&change.tool))
            && roles::address(&applied.role)
                .is_some_and(|address| changes.affected_roles.iter().any(|role| role == address))
    };
    let applied = changes
        .tool_descriptions
        .iter()
        .map(|change| {
            observed
                .tool_descriptions
                .iter()
                .any(|applied| used(applied, change))
        })
        .collect();
    let applied_runs = observed
        .tool_descriptions
        .iter()
        .filter(|applied| {
            changes
                .tool_descriptions
                .iter()
                .any(|change| used(applied, change))
        })
        .map(|applied| applied.run_id.clone())
        .collect();
    let mut fallbacks = Vec::new();
    if !changes.tool_descriptions.is_empty() {
        // The model each role was fixed to, read from its own runs' snapshots: one
        // (profile, base model) per role, or else it cannot be told.
        let mut fixed: BTreeMap<&str, BTreeSet<Option<(String, String)>>> = BTreeMap::new();
        for applied in &observed.tool_descriptions {
            if let Some(address) = roles::address(&applied.role) {
                let identity = applied
                    .profile
                    .as_deref()
                    .zip(applied.route_model_id.as_deref())
                    .map(|(profile, model)| (profile.to_owned(), base_model(model).to_owned()));
                fixed.entry(address).or_default().insert(identity);
            }
        }
        for used in &observed.models {
            let name = used.role.as_deref().unwrap_or("a request with no role");
            let Some(address) = used.role.as_deref().and_then(roles::address) else {
                fallbacks.push(format!(
                    "{name}: {} request(s) to {}/{} cannot be attributed to a role, so a fallback cannot be ruled out",
                    used.requests,
                    used.profile.as_deref().unwrap_or(&used.provider),
                    used.model
                ));
                continue;
            };
            if !changes.affected_roles.iter().any(|role| role == address) {
                continue;
            }
            let sent = used
                .profile
                .as_deref()
                .map(|profile| (profile.to_owned(), base_model(&used.model).to_owned()));
            match fixed.get(address).map(|set| set.iter().collect::<Vec<_>>()) {
                Some(identities) if identities.len() == 1 && identities[0].is_some() => {
                    if sent.as_ref() != identities[0].as_ref() {
                        let (profile, model) = identities[0].clone().unwrap_or_default();
                        fallbacks.push(format!(
                            "{name} is fixed to {profile}/{model} by its own run, but sent {} request(s) to {}/{}",
                            used.requests,
                            used.profile.as_deref().unwrap_or(&used.provider),
                            used.model
                        ));
                    }
                }
                _ => fallbacks.push(format!(
                    "{name}: its runs do not fix one model (none recorded, an unknown one or several), so {} request(s) to {}/{} cannot be told from a fallback",
                    used.requests,
                    used.profile.as_deref().unwrap_or(&used.provider),
                    used.model
                )),
            }
        }
    }
    RunEffect {
        observed: true,
        applied,
        applied_runs,
        fallbacks,
    }
}

fn usage_of(request: &RequestUsage) -> FirstUsage {
    FirstUsage {
        input_tokens: request.input_tokens,
        cache_read_tokens: request.cache_read_tokens,
        cache_write_tokens: request.cache_write_tokens,
        output_tokens: request.output_tokens,
    }
}

/// The first attempt is the first request on record, whether or not it
/// succeeded: a later success is not what the replay's first request cost.
fn first_request(run: &LoadedRun) -> FirstRequest {
    let first: Option<&RequestUsage> = run.record.agent_requests().next();
    let unknown = match first {
        None => Some("the replay made no agent request on record".to_owned()),
        Some(request) if request.failed => {
            Some("the first attempt failed and left no usage".to_owned())
        }
        Some(request) if !request.usage_reported => {
            Some("the provider reported no usage for it".to_owned())
        }
        Some(_) => None,
    };
    let first_success = first
        .filter(|request| request.failed)
        .and_then(|_| {
            run.record
                .agent_requests()
                .find(|request| !request.failed && request.usage_reported)
        })
        .map(usage_of);
    FirstRequest {
        run: run.summary.name(),
        usage: first.filter(|_| unknown.is_none()).map(usage_of),
        unknown,
        first_success,
    }
}

fn verification_text(run: &LoadedRun) -> Option<String> {
    run.summary.verification.as_ref().map(|verification| {
        if verification.passed {
            "passed".to_owned()
        } else if verification.timed_out {
            "timed out".to_owned()
        } else {
            "failed".to_owned()
        }
    })
}

/// What [`build`] reads from the evaluation.
pub struct Inputs<'a> {
    pub changes: &'a Changes,
    /// The configurations the evaluation was to replay, `repeat` times each.
    pub configurations: &'a [String],
    pub repeat: u32,
    pub preflight_failure: Option<&'a str>,
    pub runs: &'a [LoadedRun],
    pub entries: &'a [RunEntry],
    pub costs: &'a [ConfigurationCost],
    pub overhead: &'a Overhead,
    pub pairs: &'a [PairReport],
    pub tally: &'a [AspectTally],
}

pub fn build(input: &Inputs<'_>) -> Breakdown {
    let Inputs {
        changes,
        configurations,
        repeat,
        preflight_failure,
        runs,
        entries,
        costs,
        overhead,
        pairs,
        tally,
    } = input;
    let initial_context = runs
        .iter()
        .map(|run| InitialContext {
            run: run.summary.name(),
            context: run.base_context.clone(),
            note: if run.base_context.is_some() {
                ESTIMATE_NOTE.to_owned()
            } else {
                "unknown: the replay's runtime recorded no initial context".to_owned()
            },
        })
        .collect();
    let cumulative = runs
        .iter()
        .map(|run| {
            let tokens = &run.summary.metrics.tokens;
            Cumulative {
                run: run.summary.name(),
                requests: tokens.requests,
                input_tokens: tokens.input(),
                cache_read_tokens: tokens.cache_read,
                cache_write_tokens: tokens.cache_write,
                output_tokens: tokens.output,
                complete: run.summary.cost.unreported.is_empty() && !run.evidence_missing,
            }
        })
        .collect();
    let quality = QualityBreakdown {
        runs: runs
            .iter()
            .map(|run| {
                let grading = run.summary.checks.as_ref();
                RunQuality {
                    run: run.summary.name(),
                    status: run.summary.status,
                    checks: grading.map(|grading| grading.quality().label().to_owned()),
                    checks_tally: grading.map(crate::checks::Grading::tally),
                    replay_valid: grading.map(|grading| grading.validity.valid),
                    verification: verification_text(run),
                }
            })
            .collect(),
        judged_pairs: count(pairs.iter().filter(|pair| !pair.verdicts.is_empty())),
        unjudged_pairs: count(pairs.iter().filter(|pair| pair.verdicts.is_empty())),
        aspects: tally.to_vec(),
    };

    let effects: Vec<(&LoadedRun, RunEffect)> = runs
        .iter()
        .map(|run| {
            let entry = entries
                .iter()
                .find(|entry| entry.label == run.summary.label && entry.index == run.summary.index);
            (run, effect_of(changes, entry))
        })
        .collect();
    let candidates: Vec<&(&LoadedRun, RunEffect)> = effects
        .iter()
        .filter(|(run, _)| run.summary.label == CANDIDATE)
        .collect();
    let effective_changes = EffectiveChanges {
        bindings: changes.bindings.clone(),
        affected_roles: changes.affected_roles.clone(),
        tool_descriptions: changes
            .tool_descriptions
            .iter()
            .enumerate()
            .map(|(at, change)| {
                let named = |applied: bool| -> Vec<String> {
                    candidates
                        .iter()
                        .filter(|(_, effect)| {
                            effect.applied.get(at).copied().unwrap_or(false) == applied
                        })
                        .map(|(run, _)| run.summary.name())
                        .collect()
                };
                EffectiveDescription {
                    profile: change.profile.clone(),
                    route_model_id: change.route_model_id.clone(),
                    tool: change.tool.clone(),
                    baseline_hash: change.baseline_hash.clone(),
                    candidate_hash: change.candidate_hash.clone(),
                    applied_in: named(true),
                    not_applied_in: named(false),
                }
            })
            .collect(),
        fallbacks: effects
            .iter()
            .flat_map(|(run, effect)| {
                effect
                    .fallbacks
                    .iter()
                    .map(|detail| format!("{}: {detail}", run.summary.name()))
            })
            .collect(),
    };

    let requests = |label: &str| -> u32 {
        runs.iter()
            .filter(|run| run.summary.label == label)
            .map(|run| run.summary.metrics.tokens.requests)
            .sum()
    };
    let agent: Vec<LabeledCost> = costs
        .iter()
        .map(|cost| {
            let mut line = CostLine::of(&cost.cost, requests(&cost.label));
            if cost.runs == 0 && line.unknown.is_none() {
                line.usd = None;
                line.unknown = Some("no replay of it finished".to_owned());
            }
            LabeledCost {
                label: cost.label.clone(),
                cost: line,
            }
        })
        .collect();
    let simulator = CostLine::of(
        &overhead.user_simulation.cost,
        overhead.user_simulation.tokens.requests,
    );
    let judge = CostLine::of(&overhead.judging.cost, overhead.judging.tokens.requests);
    let total = {
        let lines: Vec<(&str, &CostLine)> = agent
            .iter()
            .map(|labeled| (labeled.label.as_str(), &labeled.cost))
            .chain([("user simulation", &simulator), ("judge", &judge)])
            .collect();
        let unknown: Vec<&str> = lines
            .iter()
            .filter(|(_, line)| line.usd.is_none())
            .map(|(name, _)| *name)
            .collect();
        let priced: f64 = lines.iter().map(|(_, line)| line.priced_usd).sum();
        let unknown_text =
            (!unknown.is_empty()).then(|| format!("unknown for {}", unknown.join(", ")));
        CostLine {
            usd: unknown_text.is_none().then_some(priced),
            priced_usd: priced,
            api_equivalent: lines.iter().any(|(_, line)| line.api_equivalent),
            requests: lines.iter().map(|(_, line)| line.requests).sum(),
            unknown: unknown_text,
        }
    };
    let cost = CostBreakdown {
        agent,
        simulator,
        judge,
        total,
    };

    let mut pending = Vec::new();
    for run in *runs {
        let name = run.summary.name();
        if run.summary.status != Status::Completed {
            pending.push(Pending {
                kind: PendingKind::ReplayIncomplete,
                subject: name.clone(),
                reason: format!(
                    "the replay ended {:?}: {}",
                    run.summary.status, run.summary.reason
                ),
            });
        }
        if run.evidence_missing {
            pending.push(Pending {
                kind: PendingKind::EvidenceMissing,
                subject: name.clone(),
                reason: "the replay left no run database to read".into(),
            });
        }
        if let Some(grading) = &run.summary.checks
            && matches!(
                grading.quality(),
                crate::checks::Quality::Indeterminate | crate::checks::Quality::ReplayInvalid
            )
        {
            pending.push(Pending {
                kind: PendingKind::ChecksNotConclusive,
                subject: name.clone(),
                reason: format!(
                    "the checks are {}: {}",
                    grading.quality().label(),
                    grading.validity.issues.join("; ")
                ),
            });
        }
    }
    if !changes.tool_descriptions.is_empty() {
        for (run, effect) in &effects {
            let name = run.summary.name();
            if !effect.observed {
                pending.push(Pending {
                    kind: PendingKind::EvidenceMissing,
                    subject: name.clone(),
                    reason: "the replay recorded neither the models nor the descriptions it used, so it cannot be told whether a request went to another model or the changed descriptions applied".into(),
                });
                continue;
            }
            if run.summary.label == CANDIDATE {
                for (change, applied) in changes.tool_descriptions.iter().zip(&effect.applied) {
                    if !applied {
                        pending.push(Pending {
                            kind: PendingKind::NotApplied,
                            subject: name.clone(),
                            reason: format!(
                                "no run that was offered `{}` froze the changed description for {}/{}; the comparison says nothing of it",
                                change.tool, change.profile, change.route_model_id
                            ),
                        });
                    }
                }
            }
            for detail in &effect.fallbacks {
                pending.push(Pending {
                    kind: PendingKind::ModelFallback,
                    subject: name.clone(),
                    reason: detail.clone(),
                });
            }
        }
    }
    if let Some(reason) = preflight_failure {
        pending.push(Pending {
            kind: PendingKind::PreflightFailed,
            subject: "preflight".into(),
            reason: format!(
                "the evaluation was refused before any replay compared anything: {reason}"
            ),
        });
    }
    for label in *configurations {
        let started: Vec<&RunEntry> = entries
            .iter()
            .filter(|entry| &entry.label == label && entry.index >= 1 && entry.index <= *repeat)
            .collect();
        let never = usize::try_from(*repeat)
            .unwrap_or(usize::MAX)
            .saturating_sub(started.len());
        if never > 0 {
            pending.push(Pending {
                kind: PendingKind::ReplayMissing,
                subject: label.clone(),
                reason: format!("{never} of {repeat} replay(s) were never started"),
            });
        }
        for entry in started.iter().filter(|entry| entry.status.is_none()) {
            pending.push(Pending {
                kind: PendingKind::ReplayMissing,
                subject: format!("{label}-{}", entry.index),
                reason:
                    "the replay was started but has no recorded end (still running, or interrupted)"
                        .into(),
            });
        }
    }
    for aspect in *tally {
        if aspect.order_disagreements > 0 {
            pending.push(Pending {
                kind: PendingKind::JudgeOrderDisagreement,
                subject: aspect.aspect.clone(),
                reason: format!(
                    "the two presentation orders named different winners in {} pair(s); the aspect has no result there",
                    aspect.order_disagreements
                ),
            });
        }
    }
    for pair in *pairs {
        for error in pair.judging_error.iter() {
            pending.push(Pending {
                kind: PendingKind::JudgingFailed,
                subject: format!("pair {}", pair.index),
                reason: error.clone(),
            });
        }
    }

    Breakdown {
        initial_context,
        first_request: runs.iter().map(first_request).collect(),
        cumulative,
        quality,
        effective_changes,
        cost,
        validity: ComparisonValidity {
            valid: pending.is_empty(),
            pending,
        },
    }
}

fn count<T>(items: impl Iterator<Item = T>) -> u32 {
    u32::try_from(items.count()).unwrap_or(u32::MAX)
}

fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}

fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// The sections of `report.md` that split the comparison by measure.
pub fn markdown(breakdown: &Breakdown) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "\n## Initial context (estimate)\n\nThe system prompt and tools each replay's root run starts with, composed by that replay's own runtime; {ESTIMATE_NOTE}. It is not a measured usage.\n\n| run | model | system | tools | total | tools (count) |\n|---|---|---|---|---|---|\n"
    ));
    for initial in &breakdown.initial_context {
        match &initial.context {
            Some(context) => out.push_str(&format!(
                "| {} | {} | ~{} tokens ({} bytes) | ~{} tokens ({} bytes) | ~{} tokens | {} |\n",
                initial.run,
                context.selected_model,
                context.system_tokens,
                context.system_bytes,
                context.tools_tokens,
                context.tools_bytes,
                context.total_tokens(),
                context.tool_count
            )),
            None => out.push_str(&format!(
                "| {} | unknown | unknown | unknown | unknown | unknown |\n",
                initial.run
            )),
        }
    }

    out.push_str("\n## First request (measured)\n\nThe provider's reported usage for each replay's first request. It includes the task text and whatever else the request carried, so it is not the tools' size alone.\n\n| run | input (all) | cache read | cache write | output |\n|---|---|---|---|---|\n");
    for first in &breakdown.first_request {
        match &first.usage {
            Some(usage) => out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                first.run,
                usage.input_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
                usage.output_tokens
            )),
            None => out.push_str(&format!(
                "| {} | unknown ({}) | - | - | - |\n",
                first.run,
                first.unknown.as_deref().unwrap_or("unknown")
            )),
        }
        if let Some(retry) = &first.first_success {
            out.push_str(&format!(
                "| {} (first request that succeeded, a retry) | {} | {} | {} | {} |\n",
                first.run,
                retry.input_tokens,
                retry.cache_read_tokens,
                retry.cache_write_tokens,
                retry.output_tokens
            ));
        }
    }

    out.push_str("\n## Cumulative usage (measured)\n\nEvery request of the replay's agents: the initial context is sent again with each one, and the history grows. Not comparable with the initial context above.\n\n| run | requests | input (all) | cache read | cache write | output |\n|---|---|---|---|---|---|\n");
    for cumulative in &breakdown.cumulative {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {}{} |\n",
            cumulative.run,
            cumulative.requests,
            cumulative.input_tokens,
            cumulative.cache_read_tokens,
            cumulative.cache_write_tokens,
            cumulative.output_tokens,
            if cumulative.complete {
                ""
            } else {
                " (lower bound: usage missing)"
            }
        ));
    }

    out.push_str("\n## Quality\n\n| run | status | machine checks | replay record | verification |\n|---|---|---|---|---|\n");
    for run in &breakdown.quality.runs {
        out.push_str(&format!(
            "| {} | {:?} | {} | {} | {} |\n",
            run.run,
            run.status,
            match (&run.checks, &run.checks_tally) {
                (Some(label), Some(tally)) => format!("{label} ({tally})"),
                _ => "no machine judgment".to_owned(),
            },
            match run.replay_valid {
                Some(true) => "complete",
                Some(false) => "incomplete",
                None => "-",
            },
            run.verification.as_deref().unwrap_or("-")
        ));
    }
    out.push_str(&format!(
        "\nJudge: {} pair(s) judged, {} without a usable judgment.",
        breakdown.quality.judged_pairs, breakdown.quality.unjudged_pairs
    ));
    for aspect in &breakdown.quality.aspects {
        out.push_str(&format!(
            "\n- {}: candidate {}, baseline {}, tie in both orders {}, orders disagreed {}",
            aspect.aspect,
            aspect.candidate_wins,
            aspect.baseline_wins,
            aspect.ties,
            aspect.order_disagreements
        ));
    }
    out.push('\n');

    let effective = &breakdown.effective_changes;
    out.push_str("\n## Effective changes\n\n");
    if effective.bindings.is_empty() && effective.tool_descriptions.is_empty() {
        out.push_str("None recorded.\n");
    }
    if !effective.bindings.is_empty() {
        out.push_str(&format!(
            "- Role bindings, routes or presets differ for: {}\n",
            effective.bindings.join(", ")
        ));
    }
    for change in &effective.tool_descriptions {
        out.push_str(&format!(
            "- Description of `{}` for {}/{}: {} → {}; used in {}; not used in {}\n",
            change.tool,
            change.profile,
            change.route_model_id,
            short(&change.baseline_hash),
            short(&change.candidate_hash),
            if change.applied_in.is_empty() {
                "no candidate replay".to_owned()
            } else {
                change.applied_in.join(", ")
            },
            if change.not_applied_in.is_empty() {
                "none".to_owned()
            } else {
                change.not_applied_in.join(", ")
            }
        ));
    }
    if !effective.affected_roles.is_empty() {
        out.push_str(&format!(
            "- Roles the differences can reach: {}\n",
            effective.affected_roles.join(", ")
        ));
    }
    for fallback in &effective.fallbacks {
        out.push_str(&format!("- Fallback: {fallback}\n"));
    }

    out.push_str("\n## Cost by payer\n\nAn unknown cost is shown as unknown, never as $0.\n\n| payer | cost | requests |\n|---|---|---|\n");
    for agent in &breakdown.cost.agent {
        out.push_str(&format!(
            "| agent ({}) | {} | {} |\n",
            agent.label,
            cell(&agent.cost.text()),
            agent.cost.requests
        ));
    }
    for (name, line) in [
        ("user simulator", &breakdown.cost.simulator),
        ("judge", &breakdown.cost.judge),
        ("total", &breakdown.cost.total),
    ] {
        out.push_str(&format!(
            "| {name} | {} | {} |\n",
            cell(&line.text()),
            line.requests
        ));
    }

    out.push_str("\n## Validity\n\n");
    if breakdown.validity.valid {
        out.push_str("Nothing holds the comparison: every replay completed, the changes were in force, and the judge's orders agreed or tied.\n");
    } else {
        out.push_str("**Judgment pending.** The following are neither passes nor regressions:\n\n");
        for pending in &breakdown.validity.pending {
            out.push_str(&format!(
                "- {:?} ({}): {}\n",
                pending.kind, pending.subject, pending.reason
            ));
        }
    }
    out
}
