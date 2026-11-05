//! Per-run budget accounting; tool counts come from the escalation detector.

use event_bus::{
    DiagnosticEvent, DiagnosticSeverity, Event, EventBus, OrchestratorEvent,
    event::diagnostic_codes,
};
use providers::Usage;
use std::{
    collections::{BTreeMap, BTreeSet},
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
    /// Reads after the first read of the same path.
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
    /// Receipt acceptance is not model observation. Retained until a successful
    /// provider request, including across a crash/restore before presentation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) lane_presentations: Vec<LanePresentation>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LanePresentation {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) nudge_episode_ids: Vec<String>,
    fingerprint: String,
    lanes: Vec<String>,
}

pub(crate) struct BudgetCounters {
    started_at: Instant,
    paused_at: Option<Instant>,
    paused_total: Duration,
    cumulative_input_tokens: u64,
    cumulative_output_tokens: u64,
    file_reads: BTreeMap<PathBuf, u32>,
    no_progress_rounds: u32,
    last_checkpoint_at: u32,
    warned: bool,
    exhausted: Option<BudgetBreach>,
    round_changed: bool,
    /// Children whose terminal result was already counted as progress.
    pub(crate) spawn_credits: SpawnCredits,
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
        let count = self.file_reads.entry(path).or_default();
        *count = count.saturating_add(1);
    }

    pub(crate) const fn mark_progress(&mut self) {
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
        let visible: std::collections::HashSet<String> = visible
            .iter()
            .filter(|m| m.role == providers::Role::User)
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                providers::ContentBlock::Text { text } => {
                    Some(format!("{:x}", Sha256::digest(text)))
                }
                _ => None,
            })
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
        self.mark_child_completed(&format!("lane:{lane_id}"))
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
            .copied()
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
                        "task {} exhausted {threshold}={}; tool_call_count={tool_calls}",
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
