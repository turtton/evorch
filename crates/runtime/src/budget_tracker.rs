//! Per-run budget accounting; tool counts come from the escalation detector.

use event_bus::{
    DiagnosticEvent, DiagnosticSeverity, Event, EventBus, OrchestratorEvent,
    event::diagnostic_codes,
};
use providers::Usage;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::Instant;

#[cfg(test)]
#[path = "budget_tracker_tests.rs"]
mod tests;

/// Consecutive child spawns, without a newly observed child completion in
/// between, that still count as progress. Bounds a parent that keeps delegating
/// fresh children but never collects their results.
pub(crate) const MAX_UNOBSERVED_SPAWN_CREDITS: u32 = 20;

/// A quantity-based resource cap whose absence is explicit.
///
/// `Unlimited` is a deliberate "no cap" and `Bounded` is a real cap. There is
/// no `Default`: a "not specified, use the runtime default" value is a separate
/// concern (see [`BudgetSettings::default`]) and must never be spelled by a
/// sentinel such as `u32::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResourceLimit<T> {
    Unlimited,
    Bounded(T),
}

impl<T: Copy> ResourceLimit<T> {
    /// The cap, or `None` when unlimited.
    pub const fn bounded(self) -> Option<T> {
        match self {
            Self::Unlimited => None,
            Self::Bounded(limit) => Some(limit),
        }
    }
}

impl<T: Copy + PartialOrd> ResourceLimit<T> {
    /// Whether `used` has reached the cap (`used >= cap`). Never for `Unlimited`.
    pub fn is_reached_by(self, used: T) -> bool {
        self.bounded().is_some_and(|limit| used >= limit)
    }

    /// Whether `used` is over the cap (`used > cap`). Never for `Unlimited`.
    pub fn is_exceeded_by(self, used: T) -> bool {
        self.bounded().is_some_and(|limit| used > limit)
    }
}

macro_rules! impl_remaining {
    ($($ty:ty),*) => {$(
        impl ResourceLimit<$ty> {
            /// What is left of the cap after `used`; `None` when unlimited.
            pub const fn remaining(self, used: $ty) -> Option<$ty> {
                match self {
                    Self::Unlimited => None,
                    Self::Bounded(limit) => Some(limit.saturating_sub(used)),
                }
            }
        }
    )*};
}
impl_remaining!(u32, u64);

/// Per-run budget. The quantity caps (`max_tool_calls`, `max_elapsed`,
/// `max_tokens`) can be [`ResourceLimit::Unlimited`]; the runaway diagnostics
/// (`max_file_rereads`, `max_no_progress_rounds`,
/// `max_identical_tool_call_repeats`) are always enforced.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BudgetSettings {
    pub max_tool_calls: ResourceLimit<u32>,
    pub max_elapsed: ResourceLimit<Duration>,
    pub max_tokens: ResourceLimit<u64>,
    /// Repetitions after the first observation of the same path, content and effective range.
    /// Unclassified run_js reads share that allowance conservatively; unrelated success does not reset it.
    pub max_file_rereads: u32,
    pub max_no_progress_rounds: u32,
    pub max_identical_tool_call_repeats: u32,
}

impl Default for BudgetSettings {
    fn default() -> Self {
        Self {
            max_tool_calls: ResourceLimit::Bounded(400),
            max_elapsed: ResourceLimit::Bounded(Duration::from_secs(2 * 60 * 60)),
            max_tokens: ResourceLimit::Unlimited,
            max_file_rereads: 20,
            max_no_progress_rounds: 100,
            max_identical_tool_call_repeats: 10,
        }
    }
}

impl BudgetSettings {
    /// These settings with every quantity cap (tool calls, elapsed time,
    /// tokens) lifted. The runaway diagnostics (no-progress rounds, identical
    /// tool calls, file rereads) keep their values, and usage is still metered.
    ///
    /// For a lane-scoped root, which is not stopped by volume alone.
    #[must_use]
    pub const fn with_unlimited_resources(mut self) -> Self {
        self.max_tool_calls = ResourceLimit::Unlimited;
        self.max_elapsed = ResourceLimit::Unlimited;
        self.max_tokens = ResourceLimit::Unlimited;
        self
    }

    /// Settings for a lane-scoped root: the default runaway diagnostics with
    /// every quantity cap lifted. Never equal to [`Self::default`], so it is
    /// not mistaken for "unspecified" when a run is started.
    #[must_use]
    pub fn lane_scope() -> Self {
        Self::default().with_unlimited_resources()
    }
}

impl From<&config::BudgetConfig> for BudgetSettings {
    fn from(config: &config::BudgetConfig) -> Self {
        Self {
            max_tool_calls: ResourceLimit::Bounded(config.max_tool_calls),
            max_tokens: config
                .max_tokens
                .map_or(ResourceLimit::Unlimited, ResourceLimit::Bounded),
            max_elapsed: ResourceLimit::Bounded(Duration::from_secs(config.max_elapsed_secs)),
            max_no_progress_rounds: config.max_no_progress_rounds,
            max_file_rereads: config.max_file_rereads,
            max_identical_tool_call_repeats: config.max_identical_tool_call_repeats,
        }
    }
}

/// Durable identity-aware credit accounting, shared by child and lane spawns.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SpawnCredits {
    spawned: BTreeSet<String>,
    observed: BTreeSet<String>,
    unobserved: u32,
    /// Successful creation intents while the original uncollected cohort remains.
    #[serde(default)]
    uncollected_lane_open_count: u32,
    #[serde(default)]
    uncollected_lane_ids: BTreeSet<String>,
    /// Identity of the current uncollected cohort: a digest of the lane IDs it was
    /// seeded with and of `uncollected_cohort_generation`, so a restored run names the
    /// same cohort while a later cohort of the same lanes is a different one. Absent in
    /// a snapshot that predates it; derived from the remaining IDs when first needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uncollected_cohort_id: Option<String>,
    /// How many cohorts this lead has seeded. A cohort that resolved and later forms
    /// again from the same lane IDs (a closed lane that reports anew) is a new episode.
    #[serde(default, skip_serializing_if = "is_zero")]
    uncollected_cohort_generation: u32,
    /// The suppression of `uncollected_cohort_id` was already reported as a diagnostic.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    uncollected_suppression_reported: bool,
    /// Receipt acceptance is not model observation. Retained until a successful
    /// provider request, including across a crash/restore before presentation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) lane_presentations: Vec<LanePresentation>,
}

/// Not a lane-count limit; only further model-authored creation is suppressed.
pub(crate) const UNCOLLECTED_LANE_OPEN_LIMIT: u32 = 100;

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn cohort_identity(ids: &BTreeSet<String>, generation: u32) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(generation.to_le_bytes());
    for id in ids {
        hash.update(id.as_bytes());
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())[..16].to_owned()
}

/// What one uncollected cohort's first refusal tells a diagnostic consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UncollectedSuppression {
    pub(crate) cohort_id: String,
    pub(crate) uncollected_lane_open_count: u32,
    pub(crate) threshold: u32,
    pub(crate) lane_ids: Vec<String>,
    pub(crate) remaining_lane_ids: usize,
}

impl UncollectedSuppression {
    /// `key=value` lines for the diagnostic. `identity` is what a consumer
    /// deduplicates on together with the lead thread. Lane IDs are bounded at
    /// eight; no brief, report or tool input text is included.
    pub(crate) fn detail(&self, lead_thread_id: &str) -> String {
        format!(
            "action=suppress_lane_open\nidentity={cohort}\ncohort={cohort}\nlead_thread_id={lead_thread_id}\nuncollected_lane_open_count={count}\nthreshold={threshold}\nlane_ids={ids}\nremaining_lane_ids={remaining}",
            cohort = self.cohort_id,
            count = self.uncollected_lane_open_count,
            threshold = self.threshold,
            ids = self.lane_ids.join(","),
            remaining = self.remaining_lane_ids,
        )
    }
}

impl SpawnCredits {
    /// Ledger state is authoritative. Working is not collection: keep a cohort
    /// through temporary nonterminal reports, but never suppress on running alone.
    #[cfg(test)]
    pub(crate) fn reconcile_uncollected_lanes(
        &mut self,
        managed: &BTreeSet<String>,
        terminal: &BTreeSet<String>,
    ) -> bool {
        self.reconcile_uncollected_lanes_at(managed, terminal, UNCOLLECTED_LANE_OPEN_LIMIT)
    }

    /// Ledger state is authoritative. Working is not collection: keep a cohort
    /// through temporary nonterminal reports, but never suppress on running alone.
    /// `limit` is the creation threshold: the default except where a test injects one.
    pub(crate) fn reconcile_uncollected_lanes_at(
        &mut self,
        managed: &BTreeSet<String>,
        terminal: &BTreeSet<String>,
        limit: u32,
    ) -> bool {
        self.uncollected_lane_ids
            .retain(|id| managed.contains(id) && !self.observed.contains(&format!("lane:{id}")));
        if self.uncollected_lane_ids.is_empty() {
            self.uncollected_lane_open_count = 0;
            self.uncollected_lane_ids = terminal
                .iter()
                .filter(|id| !self.observed.contains(&format!("lane:{id}")))
                .cloned()
                .collect();
            if self.uncollected_lane_ids.is_empty() {
                self.uncollected_cohort_id = None;
            } else {
                self.uncollected_cohort_generation =
                    self.uncollected_cohort_generation.saturating_add(1);
                self.uncollected_cohort_id = Some(cohort_identity(
                    &self.uncollected_lane_ids,
                    self.uncollected_cohort_generation,
                ));
            }
            self.uncollected_suppression_reported = false;
        }
        self.uncollected_lane_open_count >= limit
            && self
                .uncollected_lane_ids
                .iter()
                .any(|id| terminal.contains(id))
    }

    /// The durable creation identity, not admission or a new root generation.
    /// Sharing the existing identity set also handles an intent-only partial failure.
    pub(crate) fn record_lane_open(&mut self, lane_id: &str) {
        if self.spawned.insert(format!("lane-open:{lane_id}"))
            && !self.uncollected_lane_ids.is_empty()
        {
            self.uncollected_lane_open_count = self.uncollected_lane_open_count.saturating_add(1);
        }
    }

    /// The diagnostic for a refusal being returned (call only then), once per
    /// cohort. Reporting is recorded in the snapshot, so a restored run does not
    /// repeat it; a snapshot saved before the report can repeat it at most once.
    pub(crate) fn take_uncollected_suppression(
        &mut self,
        limit: u32,
    ) -> Option<UncollectedSuppression> {
        if self.uncollected_suppression_reported || self.uncollected_lane_ids.is_empty() {
            return None;
        }
        let (ids, generation) = (
            &self.uncollected_lane_ids,
            self.uncollected_cohort_generation,
        );
        let cohort_id = self
            .uncollected_cohort_id
            .get_or_insert_with(|| cohort_identity(ids, generation))
            .clone();
        self.uncollected_suppression_reported = true;
        Some(UncollectedSuppression {
            cohort_id,
            uncollected_lane_open_count: self.uncollected_lane_open_count,
            threshold: limit,
            lane_ids: self.uncollected_lane_ids.iter().take(8).cloned().collect(),
            remaining_lane_ids: self.uncollected_lane_ids.len().saturating_sub(8),
        })
    }

    pub(crate) fn uncollected_lane_rejection(&self, limit: u32) -> String {
        // IDs are bounded at ingestion; at most eight are returned to the provider.
        serde_json::json!({
            "code": "lane_creation_suppressed",
            "uncollected_lane_open_count": self.uncollected_lane_open_count,
            "threshold": limit,
            "lane_ids": self.uncollected_lane_ids.iter().take(8).collect::<Vec<_>>(),
            "remaining_lane_ids": self.uncollected_lane_ids.len().saturating_sub(8),
            "message": "New lane_open temporarily suppressed. Collect the listed lanes' current ready/failed reports with lane_list or notification delivery. Existing lanes, collection and answers remain available; this is not a lane-count limit."
        }).to_string()
    }
}

/// The uncollected-lane marker of a stored run snapshot, as a diagnostic reads it
/// (`evorch inspect run`). A value stored in the snapshot, not the ledger's current
/// state: the cohort may have been collected since.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StoredUncollectedLanes {
    /// Identity of the cohort the suppression diagnostic names. When the snapshot
    /// predates the stored identity it is derived from the remaining IDs
    /// (`cohort_id_derived`), which may differ from the one reported earlier.
    pub cohort_id: Option<String>,
    pub cohort_id_derived: bool,
    /// Successful creation intents while the cohort remained uncollected.
    pub uncollected_lane_open_count: u32,
    pub lane_ids: Vec<String>,
    /// The cohort's suppression diagnostic was already emitted.
    pub suppression_reported: bool,
    /// The creation threshold of a normal runtime; a test may inject another, which
    /// the snapshot does not record.
    pub default_threshold: u32,
}

/// Why the uncollected-lane marker of a stored run configuration cannot be shown.
/// A snapshot without the marker is never read as "nothing uncollected".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoredSpawnCreditsError {
    #[error("the stored run configuration has no spawn_credits")]
    Missing,
    #[error("the stored spawn_credits cannot be read: {0}")]
    Malformed(String),
}

impl SpawnCredits {
    /// The uncollected-lane marker, for diagnosis. Read only.
    #[must_use]
    pub fn stored_uncollected_lanes(&self) -> StoredUncollectedLanes {
        let derived = self.uncollected_cohort_id.is_none() && !self.uncollected_lane_ids.is_empty();
        StoredUncollectedLanes {
            cohort_id: self.uncollected_cohort_id.clone().or_else(|| {
                (!self.uncollected_lane_ids.is_empty()).then(|| {
                    cohort_identity(
                        &self.uncollected_lane_ids,
                        self.uncollected_cohort_generation,
                    )
                })
            }),
            cohort_id_derived: derived,
            uncollected_lane_open_count: self.uncollected_lane_open_count,
            lane_ids: self.uncollected_lane_ids.iter().cloned().collect(),
            suppression_reported: self.uncollected_suppression_reported,
            default_threshold: UNCOLLECTED_LANE_OPEN_LIMIT,
        }
    }
}

/// Fields `SpawnCredits` always serializes (it skips only the optional cohort identity,
/// the reported flag and empty presentations), so a stored value without them was not
/// written with the uncollected marker and is not read as an empty one.
const STORED_UNCOLLECTED_FIELDS: [&str; 2] =
    ["uncollected_lane_open_count", "uncollected_lane_ids"];

/// Reads the uncollected-lane marker out of a stored run configuration document
/// (`run_contexts.config_json`, resolved). It decodes `spawn_credits` with the
/// type the runtime serializes it from, so the stored form and this reader cannot
/// drift apart unnoticed.
///
/// # Errors
/// [`StoredSpawnCreditsError`] when the document has no `spawn_credits`, it does
/// not decode, or it lacks a field that is always stored; never a zero value in
/// their place.
pub fn stored_uncollected_lanes(
    run_config: &serde_json::Value,
) -> Result<StoredUncollectedLanes, StoredSpawnCreditsError> {
    let credits = run_config
        .get("spawn_credits")
        .ok_or(StoredSpawnCreditsError::Missing)?;
    let object = credits.as_object().ok_or_else(|| {
        StoredSpawnCreditsError::Malformed("spawn_credits is not an object".into())
    })?;
    if let Some(field) = STORED_UNCOLLECTED_FIELDS
        .iter()
        .find(|field| !object.contains_key(**field))
    {
        return Err(StoredSpawnCreditsError::Malformed(format!(
            "spawn_credits has no {field}"
        )));
    }
    let credits: SpawnCredits = serde_json::from_value(credits.clone())
        .map_err(|error| StoredSpawnCreditsError::Malformed(error.to_string()))?;
    Ok(credits.stored_uncollected_lanes())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LanePresentation {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) nudge_episode_ids: Vec<String>,
    pub(crate) fingerprint: String,
    pub(crate) lanes: Vec<String>,
}

/// Exact provider-visible bodies, never summaries or reference-only substitutes.
pub(crate) fn presentation_texts(messages: &[providers::Message]) -> impl Iterator<Item = &str> {
    messages.iter().flat_map(|message| {
        message.content.iter().flat_map(move |block| {
            let mut texts = Vec::new();
            match block {
                providers::ContentBlock::Text { text } if message.role == providers::Role::User => {
                    texts.push(text.as_str())
                }
                providers::ContentBlock::ToolResult {
                    content,
                    is_error: false,
                    ..
                } => {
                    texts.extend(content.iter().filter_map(|part| match part {
                        providers::ToolResultContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    }));
                }
                _ => {}
            }
            texts
        })
    })
}

pub(crate) struct BudgetCounters {
    started_at: Instant,
    paused_at: Option<Instant>,
    paused_total: Duration,
    cumulative_input_tokens: u64,
    cumulative_output_tokens: u64,
    file_reads: BTreeMap<PathBuf, FileReads>,
    no_progress_rounds: u32,
    last_checkpoint_at: u32,
    warned: bool,
    exhausted: Option<BudgetBreach>,
    round_changed: bool,
    /// Children whose terminal result was already counted as progress.
    pub(crate) spawn_credits: SpawnCredits,
    /// Run-local semantic read identities; restored runs start fresh, like repeat streaks.
    /// SHA-256 entries are fixed-size; no eviction that could mint progress on a reread.
    pub(crate) lane_observations: HashSet<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BudgetBreach {
    pub code: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BudgetDecision {
    Continue,
    Exhausted(BudgetBreach),
}

pub(crate) struct BudgetContext<'a> {
    pub bus: &'a EventBus,
    pub run_id: &'a str,
    pub task_id: &'a str,
    pub settings: &'a BudgetSettings,
}

/// run_js records paths, not result content. Charge those reads against every
/// direct observation so switching interfaces cannot double the allowance.
#[derive(Debug, Clone, Default)]
struct FileReads {
    unclassified: u32,
    observations: BTreeMap<[u8; 32], u32>,
}
impl FileReads {
    fn count(&self) -> u32 {
        self.unclassified
            .saturating_add(self.observations.values().copied().max().unwrap_or(0))
    }
}

impl Default for BudgetCounters {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            paused_at: None,
            paused_total: Duration::ZERO,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            file_reads: BTreeMap::new(),
            no_progress_rounds: 0,
            last_checkpoint_at: 0,
            warned: false,
            exhausted: None,
            round_changed: false,
            spawn_credits: SpawnCredits::default(),
            lane_observations: HashSet::new(),
        }
    }
}

impl BudgetCounters {
    /// Waiting 位相 (ユーザ回答・reply・子 run 待ち) は max_elapsed に計上しない。
    /// pause は冪等で、重ねても最初の pause 時点を維持する。
    pub(crate) fn pause(&mut self) {
        if self.paused_at.is_none() {
            self.paused_at = Some(Instant::now());
        }
    }

    /// 待機時間を除外対象として確定する。pause 中でなければ no-op。
    pub(crate) fn resume(&mut self) {
        if let Some(paused_at) = self.paused_at.take() {
            self.paused_total = self.paused_total.saturating_add(paused_at.elapsed());
        }
    }

    /// ツール呼び出しが外部の出来事 (shell job の終了・新しい入力) を待って
    /// ブロックしていた時間を、Waiting 位相と同じく max_elapsed から除く。
    pub(crate) fn exclude_idle(&mut self, idle: Duration) {
        self.paused_total = self.paused_total.saturating_add(idle);
    }

    /// max_elapsed 判定に使う、待機時間を除いた実行経過時間。
    fn active_elapsed(&self) -> Duration {
        let paused = match self.paused_at {
            Some(paused_at) => self.paused_total.saturating_add(paused_at.elapsed()),
            None => self.paused_total,
        };
        self.started_at.elapsed().saturating_sub(paused)
    }

    pub(crate) fn usage(&mut self, usage: Usage) {
        self.cumulative_input_tokens = self
            .cumulative_input_tokens
            .saturating_add(usage.input_tokens);
        self.cumulative_output_tokens = self
            .cumulative_output_tokens
            .saturating_add(usage.output_tokens);
    }

    pub(crate) fn read(&mut self, path: &Path) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let count = &mut self.file_reads.entry(path).or_default().unclassified;
        *count = count.saturating_add(1);
    }

    /// Key the returned bytes and their effective position, not ignored arguments
    /// or the read tool's pagination footer. Image ranges and empty EOF ranges
    /// carry no position information. Never rewrite the result sent to the model.
    pub(crate) fn read_result(
        &mut self,
        path: &Path,
        input: &serde_json::Value,
        result: &tools::ToolResult,
    ) {
        use sha2::{Digest, Sha256};
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let mut hash = Sha256::new();
        if result.images.is_empty() {
            let detail = result.detail.as_ref();
            let Some(content) = detail
                .and_then(|d| d["bytes_returned"].as_u64())
                .and_then(|n| usize::try_from(n).ok())
                .and_then(|n| result.content.get(..n))
            else {
                // The limiter can replace metadata with a fresh artifact ID.
                // Display text is not evidence of new file contents.
                self.read(&path);
                return;
            };
            let range = (!content.is_empty()).then(|| {
                [
                    detail
                        .and_then(|d| d["offset"].as_u64())
                        .or_else(|| input["offset"].as_u64())
                        .unwrap_or(1),
                    detail
                        .and_then(|d| d["byte_offset"].as_u64())
                        .or_else(|| input["byte_offset"].as_u64())
                        .unwrap_or(0),
                ]
            });
            hash.update(b"text");
            hash.update(serde_json::json!(range).to_string());
            hash.update(content.as_bytes());
        } else {
            hash.update(b"images");
            for image in &result.images {
                for bytes in [image.media_type.as_bytes(), image.data.as_bytes()] {
                    hash.update((bytes.len() as u64).to_be_bytes());
                    hash.update(bytes);
                }
            }
        }
        let count = self
            .file_reads
            .entry(path)
            .or_default()
            .observations
            .entry(hash.finalize().into())
            .or_default();
        *count = count.saturating_add(1);
    }

    pub(crate) const fn mark_progress(&mut self) {
        self.round_changed = true;
    }

    /// A read's own success must not forgive its reread streak.
    pub(crate) const fn mark_read_progress(&mut self) {
        self.round_changed = true;
    }

    /// A successfully started child run is progress, so a delegation-only
    /// Orchestrator is not mistaken for a stalled loop. Delegation does not
    /// consume `max_tool_calls`, so a loop that keeps spawning children without
    /// ever collecting a result would otherwise never stop: after
    /// [`MAX_UNOBSERVED_SPAWN_CREDITS`] spawns without a newly observed child
    /// completion, further spawns are no longer progress. Returns whether it counted.
    pub(crate) fn mark_spawned(&mut self, identity: &str) -> bool {
        self.spawn_credits.spawned.insert(identity.into())
            && !self.spawn_credits.observed.contains(identity)
            && self.mark_child_spawned()
    }

    pub(crate) const fn mark_child_spawned(&mut self) -> bool {
        self.spawn_credits.unobserved = self.spawn_credits.unobserved.saturating_add(1);
        if self.spawn_credits.unobserved > MAX_UNOBSERVED_SPAWN_CREDITS {
            return false;
        }
        self.mark_progress();
        true
    }

    /// Counts the first observed terminal result of a child run as progress and
    /// restores the spawn credits. Re-observing the same child is neither;
    /// returns whether it counted.
    pub(crate) fn mark_child_completed(&mut self, run_id: &str) -> bool {
        if !self.spawn_credits.observed.insert(run_id.to_owned()) {
            return false;
        }
        self.spawn_credits.unobserved = 0;
        self.mark_progress();
        true
    }

    #[cfg(test)]
    pub(crate) fn mark_lane_spawned(&mut self, lane_id: &str) -> bool {
        self.mark_spawned(&format!("lane:{lane_id}"))
    }

    pub(crate) fn queue_lane_presentation(&mut self, text: &str, lanes: Vec<String>) {
        use sha2::{Digest, Sha256};
        self.spawn_credits
            .lane_presentations
            .push(LanePresentation {
                nudge_episode_ids: Vec::new(),
                fingerprint: format!("{:x}", Sha256::digest(text)),
                lanes,
            });
    }

    pub(crate) fn observe_lane_presentations(&mut self, visible: &[providers::Message]) -> bool {
        use sha2::{Digest, Sha256};
        let pending = std::mem::take(&mut self.spawn_credits.lane_presentations);
        let changed = !pending.is_empty();
        let visible: std::collections::HashSet<String> = presentation_texts(visible)
            .map(|text| format!("{:x}", Sha256::digest(text)))
            .collect();
        for presentation in pending {
            if visible.contains(&presentation.fingerprint) {
                for lane in presentation.lanes {
                    self.mark_lane_completed(&lane);
                }
            }
            // Compacted-away details were not presented: no credit. A later
            // explicit inbox/list detail read may still observe that lane.
        }
        changed
    }

    pub(crate) fn mark_lane_completed(&mut self, lane_id: &str) -> bool {
        let counted = self.mark_child_completed(&format!("lane:{lane_id}"));
        self.spawn_credits.uncollected_lane_ids.remove(lane_id);
        if self.spawn_credits.uncollected_lane_ids.is_empty() {
            self.spawn_credits.uncollected_lane_open_count = 0;
        }
        counted
    }

    pub(crate) fn finish_round(&mut self) {
        self.no_progress_rounds = if self.round_changed {
            0
        } else {
            self.no_progress_rounds.saturating_add(1)
        };
        self.round_changed = false;
    }

    pub(crate) fn publish(
        &mut self,
        tool_calls: u32,
        context: &BudgetContext<'_>,
    ) -> BudgetDecision {
        if let Some(breach) = &self.exhausted {
            return BudgetDecision::Exhausted(breach.clone());
        }
        let elapsed = self.active_elapsed();
        if tool_calls > self.last_checkpoint_at && tool_calls.is_multiple_of(50) {
            self.last_checkpoint_at = tool_calls;
            context
                .bus
                .emit(Event::new(OrchestratorEvent::TaskCheckpoint {
                    task_id: context.task_id.into(),
                    run_id: context.run_id.into(),
                    tool_call_count: tool_calls,
                    cumulative_input_tokens: self.cumulative_input_tokens,
                    cumulative_output_tokens: self.cumulative_output_tokens,
                    elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                }));
        }
        let settings = context.settings;
        let rereads = self
            .file_reads
            .values()
            .map(FileReads::count)
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        let used_tokens = self
            .cumulative_input_tokens
            .saturating_add(self.cumulative_output_tokens);
        // Quantity caps may be `Unlimited`, which never trips; the runaway
        // diagnostics below always apply.
        let checks = [
            (
                settings.max_tool_calls.is_reached_by(tool_calls),
                "max_tool_calls",
                settings.max_tool_calls.bounded().map(u64::from),
            ),
            (
                settings.max_elapsed.is_exceeded_by(elapsed),
                "max_elapsed_ms",
                settings
                    .max_elapsed
                    .bounded()
                    .map(|limit| u64::try_from(limit.as_millis()).unwrap_or(u64::MAX)),
            ),
            (
                settings.max_tokens.is_exceeded_by(used_tokens),
                "max_tokens",
                settings.max_tokens.bounded(),
            ),
            (
                rereads > settings.max_file_rereads,
                "max_file_rereads",
                Some(u64::from(settings.max_file_rereads)),
            ),
            (
                self.no_progress_rounds > settings.max_no_progress_rounds,
                "max_no_progress_rounds",
                Some(u64::from(settings.max_no_progress_rounds)),
            ),
        ];
        for (index, (exceeded, threshold, limit)) in checks.into_iter().enumerate() {
            if exceeded {
                let breach = BudgetBreach {
                    code: if index == 4 {
                        diagnostic_codes::NO_PROGRESS
                    } else {
                        diagnostic_codes::BUDGET_EXHAUSTED
                    },
                    detail: format!(
                        "task {} exhausted {threshold}={}; tool_call_count={tool_calls}; action=stop_run",
                        context.task_id,
                        limit.unwrap_or_default()
                    ),
                };
                context.bus.emit(Event::new(DiagnosticEvent {
                    source: "budget_tracker".into(),
                    severity: DiagnosticSeverity::Error,
                    code: breach.code.into(),
                    detail: breach.detail.clone(),
                    run_id: Some(context.run_id.into()),
                    thread_id: None,
                    call_id: None,
                }));
                self.exhausted = Some(breach.clone());
                return BudgetDecision::Exhausted(breach);
            }
        }
        let remaining_tool_calls = settings.max_tool_calls.remaining(tool_calls);
        let remaining_tokens = settings.max_tokens.remaining(used_tokens);
        let calls_near_end = remaining_tool_calls
            .zip(settings.max_tool_calls.bounded())
            .is_some_and(|(remaining, limit)| remaining <= limit / 5);
        let tokens_near_end = remaining_tokens
            .zip(settings.max_tokens.bounded())
            .is_some_and(|(remaining, limit)| remaining <= limit / 5);
        if !self.warned && (calls_near_end || tokens_near_end) {
            self.warned = true;
            context.bus.emit(Event::new(DiagnosticEvent {
                source: "budget_tracker".into(),
                severity: DiagnosticSeverity::Warning,
                code: diagnostic_codes::BUDGET_WARNING.into(),
                detail: format!(
                    "task {} remaining_tool_calls={}{}",
                    context.task_id,
                    remaining_tool_calls
                        .map_or_else(|| "unlimited".to_owned(), |calls| calls.to_string()),
                    remaining_tokens
                        .map_or_else(String::new, |tokens| format!("; remaining_tokens={tokens}"))
                ),
                run_id: Some(context.run_id.into()),
                thread_id: None,
                call_id: None,
            }));
        }
        BudgetDecision::Continue
    }
}
