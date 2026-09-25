//! Internal lesson extraction/review submissions and immutable, scoped source snapshots.
//! Tool calls only stage data. The learning pipeline commits it after successful runs.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{AgentRunPhase, AgentRuntime, RunConfig, RunId, RunPurpose};

pub const MAX_LESSON_CANDIDATES: usize = 8;
const MAX_CONTENT_BYTES: usize = 2_000;
const MAX_RATIONALE_BYTES: usize = 1_000;
const MAX_EVIDENCE_REFS: usize = 8;
const SOURCE_CHUNK_BYTES: usize = 1_024;
const MAX_SOURCE_PAGE: usize = 4;
const MAX_TOOL_OUTPUT_BYTES: usize = 32_768;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LessonCandidate {
    pub id: String,
    pub content: String,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonVerdict {
    Approve,
    Reject,
    RequestUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LessonReview {
    pub candidate_id: String,
    pub verdict: LessonVerdict,
    pub rationale: String,
    pub evidence_refs: Vec<String>,
}

#[derive(Default)]
pub(crate) struct LearningStaging {
    extractions: HashMap<RunId, Extraction>,
    reviews: HashMap<RunId, ReviewStaging>,
}

struct Extraction {
    source_run_id: RunId,
    snapshot: Arc<SourceSnapshot>,
    inspected: BTreeSet<String>,
    candidates: Vec<LessonCandidate>,
}

#[derive(Default)]
struct ReviewStaging {
    inspected: BTreeSet<String>,
    listed: BTreeSet<String>,
    submissions: BTreeMap<String, LessonReview>,
}

struct SourceSnapshot {
    runs: BTreeMap<String, SourceRun>,
}

struct SourceRun {
    parent_run_id: Option<String>,
    phase: String,
    records: Vec<SourceRecord>,
}

struct SourceRecord {
    reference: String,
    role: &'static str,
    kind: &'static str,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InspectSourceArgs {
    pub run_id: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_source_limit")]
    pub limit: usize,
    #[serde(default)]
    pub content_offset: usize,
}
fn default_source_limit() -> usize {
    MAX_SOURCE_PAGE
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StackCandidateArgs {
    pub content: String,
    pub evidence_refs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListCandidatesArgs {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_candidate_limit")]
    pub limit: usize,
}
fn default_candidate_limit() -> usize {
    2
}

impl AgentRuntime {
    /// Successful extraction submissions; final assistant text is never parsed.
    pub fn learning_candidates(&self, extraction: RunId) -> Result<Vec<LessonCandidate>, String> {
        self.require_learning_done(extraction)?;
        let staging = self
            .shared
            .lesson_staging
            .lock()
            .map_err(|error| error.to_string())?;
        Ok(staging
            .extractions
            .get(&extraction)
            .ok_or("extraction did not inspect a persisted learning source")?
            .candidates
            .clone())
    }

    /// Successful review submissions. Missing candidates have no implicit approval.
    pub fn learning_reviews(&self, reviewer: RunId) -> Result<Vec<LessonReview>, String> {
        self.require_learning_done(reviewer)?;
        let staging = self
            .shared
            .lesson_staging
            .lock()
            .map_err(|error| error.to_string())?;
        Ok(staging
            .reviews
            .get(&reviewer)
            .map(|entry| entry.submissions.values().cloned().collect())
            .unwrap_or_default())
    }

    /// Release temporary evidence snapshots and submissions after pipeline completion/failure.
    pub fn clear_learning_run(&self, run: RunId) {
        let mut staging = self
            .shared
            .lesson_staging
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        staging.extractions.remove(&run);
        staging.reviews.remove(&run);
    }

    fn require_learning_done(&self, run: RunId) -> Result<(), String> {
        if self
            .inspect_agent(run)
            .map_err(|error| error.to_string())?
            .phase
            != AgentRunPhase::Done
        {
            return Err(
                "learning submissions are unavailable until the run completes successfully".into(),
            );
        }
        Ok(())
    }

    fn learning_snapshot(
        &self,
        caller: RunId,
        config: &RunConfig,
    ) -> Result<Arc<SourceSnapshot>, String> {
        let (source, extraction, review) = purpose(caller, config)?;
        if review {
            self.require_learning_done(extraction)?;
        }
        {
            let staging = self
                .shared
                .lesson_staging
                .lock()
                .map_err(|error| error.to_string())?;
            if let Some(entry) = staging.extractions.get(&extraction) {
                if entry.source_run_id != source {
                    return Err("learning source authority mismatch".into());
                }
                return Ok(Arc::clone(&entry.snapshot));
            }
        }
        if review {
            return Err("extraction source snapshot is unavailable".into());
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or("persisted run context is required for learning")?;
        let records = store
            .learning_source_contexts(source)
            .map_err(|error| error.to_string())?;
        let snapshot = Arc::new(SourceSnapshot::from_records(source, records)?);
        let mut staging = self
            .shared
            .lesson_staging
            .lock()
            .map_err(|error| error.to_string())?;
        let entry = staging
            .extractions
            .entry(extraction)
            .or_insert_with(|| Extraction {
                source_run_id: source,
                snapshot,
                inspected: BTreeSet::new(),
                candidates: Vec::new(),
            });
        Ok(Arc::clone(&entry.snapshot))
    }

    pub(crate) fn inspect_learning_source(
        &self,
        caller: RunId,
        config: &RunConfig,
        args: InspectSourceArgs,
    ) -> Result<Value, String> {
        let (_, extraction, review) = purpose(caller, config)?;
        let snapshot = self.learning_snapshot(caller, config)?;
        let (output, references) = snapshot.page(args)?;
        let mut staging = self
            .shared
            .lesson_staging
            .lock()
            .map_err(|error| error.to_string())?;
        let inspected = if review {
            &mut staging.reviews.entry(caller).or_default().inspected
        } else {
            &mut staging
                .extractions
                .get_mut(&extraction)
                .ok_or("extraction stage is unavailable")?
                .inspected
        };
        inspected.extend(references);
        Ok(output)
    }

    pub(crate) fn stack_lesson_candidate(
        &self,
        caller: RunId,
        config: &RunConfig,
        args: StackCandidateArgs,
    ) -> Result<Value, String> {
        let (_, extraction, review) = purpose(caller, config)?;
        if review {
            return Err("only lesson extraction can stack candidates".into());
        }
        bounded_text(&args.content, MAX_CONTENT_BYTES, "content")?;
        validate_refs(&args.evidence_refs, false)?;
        let mut staging = self
            .shared
            .lesson_staging
            .lock()
            .map_err(|error| error.to_string())?;
        let entry = staging
            .extractions
            .get_mut(&extraction)
            .ok_or("inspect source evidence before stacking a candidate")?;
        if args
            .evidence_refs
            .iter()
            .any(|reference| !entry.inspected.contains(reference))
        {
            return Err(
                "candidate evidence must reference source records inspected by this extraction run"
                    .into(),
            );
        }
        if let Some(existing) = entry.candidates.iter().find(|candidate| {
            candidate.content == args.content && candidate.evidence_refs == args.evidence_refs
        }) {
            return Ok(json!({"candidate_id":existing.id,"status":"staged"}));
        }
        if entry.candidates.len() >= MAX_LESSON_CANDIDATES {
            return Err("at most 8 lesson candidates may be staged".into());
        }
        let id = format!("lesson-{}-{}", extraction.get(), entry.candidates.len() + 1);
        entry.candidates.push(LessonCandidate {
            id: id.clone(),
            content: args.content,
            evidence_refs: args.evidence_refs,
        });
        Ok(json!({"candidate_id":id,"status":"staged"}))
    }

    pub(crate) fn list_lesson_candidates(
        &self,
        caller: RunId,
        config: &RunConfig,
        args: ListCandidatesArgs,
    ) -> Result<Value, String> {
        let (_, extraction, review) = purpose(caller, config)?;
        if !review {
            return Err("only lesson review can list staged candidates".into());
        }
        self.learning_snapshot(caller, config)?;
        if !(1..=2).contains(&args.limit) {
            return Err("limit must be between 1 and 2".into());
        }
        let mut staging = self
            .shared
            .lesson_staging
            .lock()
            .map_err(|error| error.to_string())?;
        let candidates = &staging
            .extractions
            .get(&extraction)
            .ok_or("extraction stage is unavailable")?
            .candidates;
        if args.offset > candidates.len() {
            return Err("offset exceeds candidate count".into());
        }
        let selected: Vec<_> = candidates
            .iter()
            .skip(args.offset)
            .take(args.limit)
            .cloned()
            .collect();
        let next = args.offset + selected.len();
        let output = json!({"candidates":selected,"next_offset":(next < candidates.len()).then_some(next),"total":candidates.len()});
        bound_output(&output)?;
        staging
            .reviews
            .entry(caller)
            .or_default()
            .listed
            .extend(selected.into_iter().map(|candidate| candidate.id));
        Ok(output)
    }

    pub(crate) fn submit_lesson_review(
        &self,
        caller: RunId,
        config: &RunConfig,
        review: LessonReview,
    ) -> Result<Value, String> {
        let (_, extraction, is_review) = purpose(caller, config)?;
        if !is_review {
            return Err("only lesson review can submit a verdict".into());
        }
        self.require_learning_done(extraction)?;
        bounded_text(&review.rationale, MAX_RATIONALE_BYTES, "rationale")?;
        bounded_text(&review.candidate_id, 128, "candidate_id")?;
        validate_refs(
            &review.evidence_refs,
            review.verdict != LessonVerdict::Approve,
        )?;
        let mut staging = self
            .shared
            .lesson_staging
            .lock()
            .map_err(|error| error.to_string())?;
        let candidate = staging
            .extractions
            .get(&extraction)
            .and_then(|entry| {
                entry
                    .candidates
                    .iter()
                    .find(|candidate| candidate.id == review.candidate_id)
            })
            .ok_or("candidate does not belong to this extraction")?
            .clone();
        let entry = staging.reviews.entry(caller).or_default();
        if !entry.listed.contains(&candidate.id) {
            return Err("list this candidate before reviewing it".into());
        }
        if review
            .evidence_refs
            .iter()
            .any(|reference| !entry.inspected.contains(reference))
        {
            return Err("review evidence must be inspected independently by the review run".into());
        }
        if review.verdict == LessonVerdict::Approve
            && candidate
                .evidence_refs
                .iter()
                .any(|reference| !review.evidence_refs.contains(reference))
        {
            return Err("approval must verify every candidate evidence reference".into());
        }
        if let Some(existing) = entry.submissions.get(&candidate.id) {
            if existing != &review {
                return Err("a verdict is already staged for this candidate".into());
            }
        } else {
            entry.submissions.insert(candidate.id.clone(), review);
        }
        Ok(json!({"candidate_id":candidate.id,"status":"staged"}))
    }
}

fn purpose(caller: RunId, config: &RunConfig) -> Result<(RunId, RunId, bool), String> {
    if !config.learning_internal {
        return Err("internal learning authority is required".into());
    }
    match config.purpose {
        RunPurpose::LessonExtract { source_run_id } => Ok((source_run_id, caller, false)),
        RunPurpose::LessonReview {
            source_run_id,
            extraction_run_id,
        } => Ok((source_run_id, extraction_run_id, true)),
        RunPurpose::General => Err("this run has no learning authority".into()),
    }
}

impl SourceSnapshot {
    fn from_records(
        source: RunId,
        records: Vec<storage::RunContextRecord>,
    ) -> Result<Self, String> {
        let source_id = source.to_string();
        let root = records
            .iter()
            .find(|record| record.run_id == source_id)
            .ok_or("source run has no persisted context")?;
        if root.parent_run_id.is_some() || root.terminal_phase != "Done" {
            return Err("learning source must be a successfully completed root run".into());
        }
        let mut runs = BTreeMap::new();
        for record in records {
            // Learning stages cannot become the evidence of another learning stage.
            let config: Value =
                serde_json::from_str(&record.config_json).map_err(|error| error.to_string())?;
            if config
                .get("non_restorable_reason")
                .and_then(Value::as_str)
                .is_some_and(|reason| reason.contains("learning_purpose"))
            {
                continue;
            }
            let messages: Vec<providers::Message> =
                serde_json::from_str(&record.messages_json).map_err(|error| error.to_string())?;
            let mut visible = Vec::new();
            for (message_index, message) in messages.into_iter().enumerate() {
                let role = match message.role {
                    providers::Role::System => continue,
                    providers::Role::User => "user",
                    providers::Role::Assistant => "assistant",
                };
                for (block_index, block) in message.content.into_iter().enumerate() {
                    let (kind, content) = match block {
                        providers::ContentBlock::Text { text } => ("text", text),
                        providers::ContentBlock::ToolUse { id, name, input } => ("tool_use", json!({"id":id,"name":name,"input":input}).to_string()),
                        providers::ContentBlock::ToolResult { tool_call_id, content, is_error } => ("tool_result", json!({"tool_call_id":tool_call_id,"content":content,"is_error":is_error}).to_string()),
                        providers::ContentBlock::Reasoning { .. } | providers::ContentBlock::Image { .. } => continue,
                    };
                    visible.push(SourceRecord {
                        reference: format!(
                            "{}@{}:m{message_index}:b{block_index}",
                            record.run_id, record.updated_at_ns
                        ),
                        role,
                        kind,
                        content,
                    });
                }
            }
            runs.insert(
                record.run_id,
                SourceRun {
                    parent_run_id: record.parent_run_id,
                    phase: record.terminal_phase,
                    records: visible,
                },
            );
        }
        Ok(Self { runs })
    }

    fn page(&self, args: InspectSourceArgs) -> Result<(Value, Vec<String>), String> {
        if !(1..=MAX_SOURCE_PAGE).contains(&args.limit) {
            return Err("limit must be between 1 and 4".into());
        }
        let Some(run_id) = args.run_id else {
            if args.content_offset != 0 {
                return Err("content_offset requires run_id".into());
            }
            if args.offset > self.runs.len() {
                return Err("offset exceeds run count".into());
            }
            let runs: Vec<_> = self.runs.iter().skip(args.offset).take(args.limit).map(|(id, run)| json!({
                "run_id":id,"parent_run_id":run.parent_run_id,"phase":run.phase,"record_count":run.records.len()
            })).collect();
            let next = args.offset + runs.len();
            return Ok((
                json!({"runs":runs,"next_offset":(next < self.runs.len()).then_some(next),"total":self.runs.len()}),
                Vec::new(),
            ));
        };
        let run = self
            .runs
            .get(&run_id)
            .ok_or("run is outside this learning source snapshot")?;
        if args.offset > run.records.len() {
            return Err("offset exceeds record count".into());
        }
        let mut selected = Vec::new();
        let mut references = Vec::new();
        let limit = if args.content_offset > 0 {
            1
        } else {
            args.limit
        };
        for (index, record) in run.records.iter().enumerate().skip(args.offset).take(limit) {
            let (content, next_content_offset) =
                byte_slice(&record.content, args.content_offset, SOURCE_CHUNK_BYTES)?;
            selected.push(json!({"offset":index,"reference":record.reference,"role":record.role,"kind":record.kind,
                "content":content,"content_offset":args.content_offset,"next_content_offset":next_content_offset}));
            references.push(record.reference.clone());
        }
        let next = args.offset + selected.len();
        let output = json!({"run_id":run_id,"records":selected,"next_offset":(next < run.records.len()).then_some(next),"total":run.records.len()});
        bound_output(&output)?;
        Ok((output, references))
    }
}

fn byte_slice(text: &str, start: usize, maximum: usize) -> Result<(&str, Option<usize>), String> {
    if start > text.len() || !text.is_char_boundary(start) {
        return Err("content_offset must be a valid UTF-8 byte boundary".into());
    }
    let mut end = start.saturating_add(maximum).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok((&text[start..end], (end < text.len()).then_some(end)))
}
fn bounded_text(text: &str, maximum: usize, field: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > maximum {
        return Err(format!("{field} must contain 1..={maximum} UTF-8 bytes"));
    }
    Ok(())
}
fn validate_refs(references: &[String], allow_empty: bool) -> Result<(), String> {
    if (!allow_empty && references.is_empty()) || references.len() > MAX_EVIDENCE_REFS {
        return Err(
            "evidence_refs must contain 1..=8 references (empty allowed for non-approval)".into(),
        );
    }
    let mut seen = BTreeSet::new();
    for reference in references {
        bounded_text(reference, 128, "evidence reference")?;
        if !seen.insert(reference) {
            return Err("duplicate evidence reference".into());
        }
    }
    Ok(())
}
fn bound_output(value: &Value) -> Result<(), String> {
    if value.to_string().len() > MAX_TOOL_OUTPUT_BYTES {
        return Err("learning output exceeds page limit; request a smaller limit".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
