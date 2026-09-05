//! run の進捗追跡と stall 判定。

use std::time::Duration;

use event_bus::{AgentRunPhase, StallSignal};
use tokio::time::Instant;

use super::ledger::OrchestrationSettings;

/// attached run の進捗状態。
#[derive(Debug, Clone)]
pub struct ProgressTrack {
    /// 最後に進捗を観測した時刻。
    pub last_progress: Instant,
    /// 実行中ツールの開始時刻。
    pub tool_in_flight: Option<Instant>,
    /// 連続ツールエラー数。
    pub consecutive_tool_errors: u32,
    /// 現在の run 位相。
    pub phase: AgentRunPhase,
    /// 最後の進捗以降に送った nudge 数。
    pub nudges_sent: u32,
}

impl ProgressTrack {
    /// 現在時刻を起点に追跡を開始する。
    pub fn new(phase: AgentRunPhase) -> Self {
        Self {
            last_progress: Instant::now(),
            tool_in_flight: None,
            consecutive_tool_errors: 0,
            phase,
            nudges_sent: 0,
        }
    }

    /// 通常進捗を記録し、連続 nudge をリセットする。
    pub fn progress(&mut self, now: Instant) {
        self.last_progress = now;
        self.nudges_sent = 0;
    }
}

/// 現時点で stall と判定すべきかを返す純粋関数。
pub fn judge(
    track: &ProgressTrack,
    now: Instant,
    settings: &OrchestrationSettings,
) -> Option<StallSignal> {
    if track.consecutive_tool_errors >= settings.repeated_error_threshold {
        return Some(StallSignal::RepeatedErrors {
            count: track.consecutive_tool_errors,
        });
    }
    let base = Duration::from_secs(settings.stall_after_secs);
    let elapsed = now.saturating_duration_since(track.last_progress);
    match track.phase {
        AgentRunPhase::Running | AgentRunPhase::Pending => {
            let window = if track.tool_in_flight.is_some() {
                base.saturating_mul(settings.in_flight_tool_multiplier)
            } else {
                base
            };
            (elapsed > window).then_some(StallSignal::NoProgress)
        }
        AgentRunPhase::Waiting => (elapsed > base).then_some(StallSignal::WaitingTooLong),
        AgentRunPhase::Done | AgentRunPhase::Error => None,
    }
}
