use std::collections::BTreeMap;

use event_bus::OrchestratorEvent;

use super::{GoalLedger, LedgerError};

impl GoalLedger {
    /// 旧呼び出し元向け。壊れた履歴を部分的な成功として扱わない。
    ///
    /// # Panics
    /// 不正な履歴なら失敗する。外部入力には `replay_checked` を使うこと。
    pub fn replay<'a>(
        events: impl Iterator<Item = &'a OrchestratorEvent>,
    ) -> BTreeMap<String, Self> {
        Self::replay_checked(events).expect("goal replay failed: invalid durable event history")
    }

    /// 全イベントを検証し、入力順に適用エラーを収集する。
    ///
    /// # Errors
    /// 所属不明・重複作成・不正遷移を含む履歴なら全エラーを返す。
    pub fn replay_checked<'a>(
        events: impl Iterator<Item = &'a OrchestratorEvent>,
    ) -> Result<BTreeMap<String, Self>, Vec<LedgerError>> {
        let mut ledgers = BTreeMap::<String, Self>::new();
        let mut errors = Vec::new();
        for event in events {
            if let OrchestratorEvent::GoalCreated { goal_id, .. } = event {
                match ledgers.entry(goal_id.clone()) {
                    std::collections::btree_map::Entry::Vacant(slot) => {
                        slot.insert(Self::new(event));
                    }
                    std::collections::btree_map::Entry::Occupied(_) => {
                        errors.push(LedgerError::DuplicateCreation);
                    }
                }
                continue;
            }
            if matches!(
                event,
                OrchestratorEvent::ShellCommandDenied { goal_id: None, .. }
            ) {
                continue;
            }
            let mut owners = ledgers
                .values_mut()
                .filter(|ledger| ledger.owns_event(event));
            match (owners.next(), owners.next()) {
                (Some(ledger), None) => {
                    if let Err(error) = ledger.apply(event) {
                        errors.push(error);
                    }
                }
                (None, _) | (Some(_), Some(_)) => {
                    errors.push(LedgerError::UnresolvedEvent(format!("{event:?}")));
                }
            }
        }
        if errors.is_empty() {
            Ok(ledgers)
        } else {
            Err(errors)
        }
    }
}
