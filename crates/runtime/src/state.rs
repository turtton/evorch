//! AgentRun の位相状態機械 (event-sourced)。

use event_bus::{AgentRunPhase, LifecycleEvent};

use crate::error::RuntimeError;
use crate::run::RunId;

/// 位相ペアの遷移妥当性を判定する。
///
/// 有効な遷移は次の 8 本のみ:
///
/// - `Pending -> Running` (起動)
/// - `Pending -> Error` (起動失敗)
/// - `Running -> Waiting / Done / Error`
/// - `Waiting -> Running / Done / Error`
///
/// `Done` と `Error` は終端位相であり、そこからの遷移は存在しない。
pub fn is_valid_transition(from: AgentRunPhase, to: AgentRunPhase) -> bool {
    use AgentRunPhase::{Done, Error, Pending, Running, Waiting};
    matches!(
        (from, to),
        (Pending, Running)
            | (Pending, Error)
            | (Running, Waiting)
            | (Running, Done)
            | (Running, Error)
            | (Waiting, Running)
            | (Waiting, Done)
            | (Waiting, Error)
    )
}

/// 単一 AgentRun の位相状態。
///
/// 状態はイベント経由でのみ変化する (event-sourced)。`phase` への直接書き換えを
/// 防ぐためフィールドは非公開で、変化は [`RunState::apply`] (イベントの畳み込み)
/// と [`RunState::transition`] (イベント生成 + 適用) のみが行う。
///
/// `run_id` の選別 (イベントの宛先判定) はこの層の外 — T4 の dispatcher — が行う。
/// [`RunState::apply`] は自分の run へ振り分け済みのイベントを受け取り、
/// `from` 位相の一致と遷移妥当性のみを検証する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunState {
    phase: AgentRunPhase,
}

impl RunState {
    /// `Pending` 位相で状態を生成する。
    pub fn new() -> Self {
        Self {
            phase: AgentRunPhase::Pending,
        }
    }

    /// 現在の位相。
    pub const fn phase(&self) -> AgentRunPhase {
        self.phase
    }

    /// ライフサイクルイベントを畳み込んで状態を進める。
    ///
    /// [`LifecycleEvent::AgentRunStateChanged`] 以外のイベントは位相に影響しない
    /// ため無視して `Ok(())` を返す。`AgentRunStateChanged` はイベントの `from` が
    /// 現在位相と一致し、かつ遷移が有効な場合のみ適用する。それ以外は
    /// [`RuntimeError::InvalidTransition`] を返し、位相は変化しない。
    ///
    /// # Errors
    /// `from` 位相の不一致または無効な遷移の場合
    /// [`RuntimeError::InvalidTransition`] を返す。
    pub fn apply(&mut self, event: &LifecycleEvent) -> Result<(), RuntimeError> {
        let LifecycleEvent::AgentRunStateChanged { from, to, .. } = event else {
            return Ok(());
        };
        if *from != self.phase || !is_valid_transition(*from, *to) {
            return Err(RuntimeError::InvalidTransition {
                from: self.phase,
                to: *to,
            });
        }
        self.phase = *to;
        Ok(())
    }

    /// 遷移イベントを生成して自身に適用する。
    ///
    /// 状態変化の単一の真実源はイベント: このメソッドは
    /// [`LifecycleEvent::AgentRunStateChanged`] を組み立て、[`RunState::apply`]
    /// 経由で適用したうえで、emit 用にイベントを返す。
    ///
    /// # Errors
    /// 現在位相からの遷移が無効な場合 [`RuntimeError::InvalidTransition`] を返す。
    pub fn transition(
        &mut self,
        run_id: RunId,
        to: AgentRunPhase,
        reason: Option<String>,
    ) -> Result<LifecycleEvent, RuntimeError> {
        let event = LifecycleEvent::AgentRunStateChanged {
            run_id: run_id.to_string(),
            from: self.phase,
            to,
            reason,
        };
        self.apply(&event)?;
        Ok(event)
    }
}

impl Default for RunState {
    /// [`RunState::new`] と同じ `Pending` 位相。
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::AgentRunPhase::{Done, Error, Pending, Running, Waiting};

    // Given: 全 5 位相の 25 通り from/to ペアと期待される遷移行列
    // When: is_valid_transition を全ペアに適用する
    // Then: Pending→Running/Error、Running→Waiting/Done/Error、Waiting→Running/Done/Error
    //       の 8 本のみ有効と判定し、Done/Error は終端として全遷移を拒否する
    #[test]
    fn is_valid_transition_matches_matrix_for_all_25_pairs() {
        let phases = [Pending, Running, Waiting, Done, Error];
        let expected: [[bool; 5]; 5] = [
            [false, true, false, false, true],   // from Pending
            [false, false, true, true, true],    // from Running
            [false, true, false, true, true],    // from Waiting
            [false, false, false, false, false], // from Done (終端)
            [false, false, false, false, false], // from Error (終端)
        ];

        for (i, &from) in phases.iter().enumerate() {
            for (j, &to) in phases.iter().enumerate() {
                assert_eq!(
                    is_valid_transition(from, to),
                    expected[i][j],
                    "ペア {from:?} -> {to:?} の判定が期待と異なる"
                );
            }
        }
    }

    // Given: Pending の RunState と Pending→Running のイベント
    // When: apply を呼ぶ
    // Then: Ok で畳み込まれ、位相が Running に進む
    #[test]
    fn apply_folds_valid_state_changed_event() {
        let mut state = RunState::new();
        let event = LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".to_string(),
            from: Pending,
            to: Running,
            reason: None,
        };

        let result = state.apply(&event);

        assert_eq!(result, Ok(()));
        assert_eq!(state.phase(), Running);
    }

    // Given: Running の RunState
    // When: Running→Pending (無効遷移) のイベントを apply する
    // Then: InvalidTransition で拒否され、位相は Running のまま
    #[test]
    fn apply_rejects_invalid_transition() {
        let mut state = RunState::new();
        state
            .transition(RunId::new(1), Running, None)
            .expect("Pending -> Running は有効");
        let event = LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".to_string(),
            from: Running,
            to: Pending,
            reason: None,
        };

        let result = state.apply(&event);

        assert_eq!(
            result,
            Err(RuntimeError::InvalidTransition {
                from: Running,
                to: Pending,
            })
        );
        assert_eq!(state.phase(), Running);
    }

    // Given: Running の RunState
    // When: 適用済みの古い Pending→Running イベント (replay) を apply する
    // Then: from 位相の不一致で InvalidTransition となり、位相は変化しない
    #[test]
    fn apply_rejects_stale_event_with_mismatched_from_phase() {
        let mut state = RunState::new();
        state
            .transition(RunId::new(1), Running, None)
            .expect("Pending -> Running は有効");
        let stale = LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".to_string(),
            from: Pending,
            to: Running,
            reason: None,
        };

        assert_eq!(
            state.apply(&stale),
            Err(RuntimeError::InvalidTransition {
                from: Running,
                to: Running,
            })
        );
        assert_eq!(state.phase(), Running);
    }

    // Given: Pending の RunState
    // When: AgentRunStateChanged 以外の lifecycle イベントを apply する
    // Then: 位相に影響しないため Ok で無視され、位相は Pending のまま
    #[test]
    fn apply_ignores_non_run_lifecycle_events() {
        let mut state = RunState::new();
        let started = LifecycleEvent::Started {
            session_id: "s-1".to_string(),
        };

        assert_eq!(state.apply(&started), Ok(()));
        assert_eq!(state.phase(), Pending);
    }

    // Given: Pending の RunState (run_id = run-1)
    // When: Running へ transition する
    // Then: run_id/from/to/reason を正しく運ぶイベントを返し、位相が Running に進む
    #[test]
    fn transition_produces_event_and_advances_phase() {
        let mut state = RunState::new();

        let event = state
            .transition(RunId::new(1), Running, None)
            .expect("Pending -> Running は有効");

        assert_eq!(
            event,
            LifecycleEvent::AgentRunStateChanged {
                run_id: "run-1".to_string(),
                from: Pending,
                to: Running,
                reason: None,
            }
        );
        assert_eq!(state.phase(), Running);
    }

    // Given: Pending の RunState (起動失敗経路)
    // When: reason 付きで Error へ transition する
    // Then: reason がイベントに載り、位相は Error になる
    #[test]
    fn transition_from_pending_to_error_carries_reason() {
        let mut state = RunState::new();

        let event = state
            .transition(RunId::new(2), Error, Some("spawn failed".to_string()))
            .expect("Pending -> Error は有効 (起動失敗)");

        assert_eq!(
            event,
            LifecycleEvent::AgentRunStateChanged {
                run_id: "run-2".to_string(),
                from: Pending,
                to: Error,
                reason: Some("spawn failed".to_string()),
            }
        );
        assert_eq!(state.phase(), Error);
    }

    // Given: 新規 RunState
    // When: Pending→Running→Waiting→Running→Done と順に遷移する
    // Then: 全遷移が成功し、位相は Done で終わる
    #[test]
    fn full_lifecycle_pending_running_waiting_running_done() {
        let mut state = RunState::new();
        let run_id = RunId::new(3);

        state.transition(run_id, Running, None).expect("-> Running");
        state.transition(run_id, Waiting, None).expect("-> Waiting");
        state
            .transition(run_id, Running, None)
            .expect("-> Running (再開)");
        state.transition(run_id, Done, None).expect("-> Done");

        assert_eq!(state.phase(), Done);
    }

    // Given: Done (終端位相) まで進んだ RunState
    // When: Running へ transition する
    // Then: InvalidTransition で拒否され、位相は Done のまま
    #[test]
    fn transition_from_terminal_phase_is_rejected() {
        let mut state = RunState::new();
        let run_id = RunId::new(4);
        state.transition(run_id, Running, None).expect("-> Running");
        state.transition(run_id, Done, None).expect("-> Done");

        assert_eq!(
            state.transition(run_id, Running, None),
            Err(RuntimeError::InvalidTransition {
                from: Done,
                to: Running,
            })
        );
        assert_eq!(state.phase(), Done);
    }
}
