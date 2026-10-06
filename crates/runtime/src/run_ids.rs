//! run ID の採番。

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::RunId;

/// runtime が新規 run に割り当てる ID の採番器。
#[derive(Debug, Default)]
pub(crate) struct RunIds(Mutex<Mode>);

#[derive(Debug)]
enum Mode {
    /// 時刻順の乱数 ID。同一ミリ秒や時計の逆行でも直前の ID より大きい値を返す。
    TimeOrdered { last: Option<RunId> },
    /// `run-1` からの連番。スクリプト化したテストやデモで ID を固定する用途。
    Sequential { next: u64 },
}

impl Default for Mode {
    fn default() -> Self {
        Self::TimeOrdered { last: None }
    }
}

impl RunIds {
    fn mode(&self) -> std::sync::MutexGuard<'_, Mode> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 連番採番に切り替える。採番済みの場合は切り替えずに `false` を返す。
    pub(crate) fn use_sequential(&self) -> bool {
        let mut mode = self.mode();
        if !mode.is_fresh() {
            return false;
        }
        *mode = Mode::Sequential { next: 1 };
        true
    }

    pub(crate) fn next(&self) -> RunId {
        match &mut *self.mode() {
            Mode::Sequential { next } => {
                let id = RunId::new(*next);
                *next = next.saturating_add(1);
                id
            }
            Mode::TimeOrdered { last } => {
                let fresh = RunId::from_parts(unix_ms(), random_bits());
                let id = match *last {
                    Some(previous) if previous >= fresh => previous.successor(),
                    _ => fresh,
                };
                *last = Some(id);
                id
            }
        }
    }

    /// 永続化済みや復元済みの ID より後の ID だけを採番するよう進める。
    pub(crate) fn observe(&self, id: RunId) {
        match &mut *self.mode() {
            Mode::Sequential { next } => {
                if let Some(following) = id.sequential_value().and_then(|n| n.checked_add(1)) {
                    *next = (*next).max(following);
                }
            }
            Mode::TimeOrdered { last } => {
                if last.is_none_or(|previous| previous < id) {
                    *last = Some(id);
                }
            }
        }
    }

    /// まだ 1 件も採番・観測していないか。
    pub(crate) fn is_fresh(&self) -> bool {
        self.mode().is_fresh()
    }
}

impl Mode {
    fn is_fresh(&self) -> bool {
        match self {
            Self::Sequential { next } => *next == 1,
            Self::TimeOrdered { last } => last.is_none(),
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
        // 時刻 0 の値は旧連番 ID と区別できないため、時計異常時も 1 以上にする。
        .max(1)
}

fn random_bits() -> u128 {
    let mut bytes = [0_u8; 16];
    // OS 乱数が使えない環境でも採番は止めない。単調増加は直前 ID との比較で保証する。
    let _ = getrandom::fill(&mut bytes);
    u128::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 時刻順採番 / When: 連続採番 / Then: 旧連番と区別できる値が作成順に並ぶ
    #[test]
    fn time_ordered_ids_increase_and_round_trip() {
        let ids = RunIds::default();
        let first = ids.next();
        let second = ids.next();

        assert!(first < second);
        assert!(first.sequential_value().is_none());
        assert_eq!(second.to_string().parse::<RunId>(), Ok(second));
    }

    // Given: 未来時刻の ID を観測済み / When: 採番 / Then: 観測済み ID より大きい
    #[test]
    fn time_ordered_ids_stay_after_observed_ids() {
        let ids = RunIds::default();
        let future = RunId::from_parts(u64::MAX >> 16, 0);
        ids.observe(future);

        assert!(ids.next() > future);
    }

    // Given: 連番採番で run-5 を観測 / When: 採番 / Then: run-6 から続く
    #[test]
    fn sequential_ids_continue_after_observed_ids() {
        let ids = RunIds::default();
        assert!(ids.use_sequential());
        ids.observe(RunId::new(5));

        assert_eq!(ids.next(), RunId::new(6));
        assert!(!ids.use_sequential());
    }
}
