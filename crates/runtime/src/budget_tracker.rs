//! Per-run budget accounting; tool counts come from the escalation detector.

use event_bus::{
    DiagnosticEvent, DiagnosticSeverity, Event, EventBus, OrchestratorEvent,
    event::diagnostic_codes,
};
use providers::Usage;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::Instant;

#[cfg(test)]
#[path = "budget_tracker_tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BudgetSettings {
    pub max_tool_calls: u32,
    pub max_elapsed: Duration,
    pub max_tokens: Option<u64>,
    /// Reads after the first read of the same path.
    pub max_file_rereads: u32,
    pub max_no_progress_rounds: u32,
    pub max_identical_tool_call_repeats: u32,
}

impl Default for BudgetSettings {
    fn default() -> Self {
        Self {
            max_tool_calls: 400,
            max_elapsed: Duration::from_secs(2 * 60 * 60),
            max_tokens: None,
            max_file_rereads: 20,
            max_no_progress_rounds: 100,
            max_identical_tool_call_repeats: 10,
        }
    }
}

impl From<&config::BudgetConfig> for BudgetSettings {
    fn from(config: &config::BudgetConfig) -> Self {
        Self {
            max_tool_calls: config.max_tool_calls,
            max_tokens: config.max_tokens,
            max_elapsed: Duration::from_secs(config.max_elapsed_secs),
            max_no_progress_rounds: config.max_no_progress_rounds,
            max_file_rereads: config.max_file_rereads,
            max_identical_tool_call_repeats: config.max_identical_tool_call_repeats,
        }
    }
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
        let checks = [
            (
                tool_calls >= settings.max_tool_calls,
                "max_tool_calls",
                u64::from(settings.max_tool_calls),
            ),
            (
                elapsed > settings.max_elapsed,
                "max_elapsed_ms",
                u64::try_from(settings.max_elapsed.as_millis()).unwrap_or(u64::MAX),
            ),
            (
                settings.max_tokens.is_some_and(|limit| {
                    self.cumulative_input_tokens
                        .saturating_add(self.cumulative_output_tokens)
                        > limit
                }),
                "max_tokens",
                settings.max_tokens.unwrap_or_default(),
            ),
            (
                rereads > settings.max_file_rereads,
                "max_file_rereads",
                u64::from(settings.max_file_rereads),
            ),
            (
                self.no_progress_rounds > settings.max_no_progress_rounds,
                "max_no_progress_rounds",
                u64::from(settings.max_no_progress_rounds),
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
                        "task {} exhausted {threshold}={limit}; tool_call_count={tool_calls}",
                        context.task_id
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
        let remaining_tool_calls = settings.max_tool_calls.saturating_sub(tool_calls);
        let remaining_tokens = settings.max_tokens.map(|limit| {
            limit.saturating_sub(
                self.cumulative_input_tokens
                    .saturating_add(self.cumulative_output_tokens),
            )
        });
        if !self.warned
            && (remaining_tool_calls <= settings.max_tool_calls / 5
                || remaining_tokens
                    .zip(settings.max_tokens)
                    .is_some_and(|(remaining, limit)| remaining <= limit / 5))
        {
            self.warned = true;
            context.bus.emit(Event::new(DiagnosticEvent {
                source: "budget_tracker".into(),
                severity: DiagnosticSeverity::Warning,
                code: diagnostic_codes::BUDGET_WARNING.into(),
                detail: format!(
                    "task {} remaining_tool_calls={remaining_tool_calls}{}",
                    context.task_id,
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
