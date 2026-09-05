//! run と goal の対応、および finish gate の実装。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use event_bus::GateEvidence;

use crate::orchestration::GoalGate;
use crate::run::RunId;

use super::delivery::DeliveryPort;
use super::gate::{self, GateVerdict};
use super::ledger::{GoalLedger, OrchestrationSettings};

/// supervisor と finish tool が共有する goal registry。
#[derive(Clone)]
pub struct GoalRegistry {
    pub(crate) ledgers: Arc<Mutex<BTreeMap<String, GoalLedger>>>,
    delivery: Arc<dyn DeliveryPort>,
    settings: OrchestrationSettings,
}

impl GoalRegistry {
    /// 共有 ledger 上に registry を構築する。
    pub fn new(
        ledgers: Arc<Mutex<BTreeMap<String, GoalLedger>>>,
        delivery: Arc<dyn DeliveryPort>,
        settings: OrchestrationSettings,
    ) -> Self {
        Self {
            ledgers,
            delivery,
            settings,
        }
    }

    fn goal_for_run(&self, run: RunId) -> Option<String> {
        let run_id = run.to_string();
        self.ledgers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find_map(|(goal_id, ledger)| {
                ledger
                    .snapshot()
                    .attached_runs
                    .iter()
                    .any(|attached| attached.run_id == run_id)
                    .then(|| goal_id.clone())
            })
    }

    /// 指定 goal の最新 remote head を反映して gate を評価する。
    pub async fn evaluate_finish_for_goal(&self, goal_id: &str) -> Option<GateVerdict> {
        let (repo, pr_number, fallback_head) = {
            let ledgers = self
                .ledgers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let snapshot = ledgers.get(goal_id)?.snapshot();
            (
                snapshot.repo.clone(),
                snapshot.pull_request.as_ref().map(|pr| pr.number),
                snapshot.pull_request.as_ref().map(|pr| pr.head_sha.clone()),
            )
        };
        let current_head = if let Some(number) = pr_number {
            match self.delivery.pr_status(&repo, number).await {
                Ok(GateEvidence::PullRequest { head_sha, .. }) => Some(head_sha),
                Ok(
                    GateEvidence::Ci { .. }
                    | GateEvidence::Criteria { .. }
                    | GateEvidence::Review { .. },
                )
                | Err(_) => fallback_head,
            }
        } else {
            fallback_head
        };
        let ledgers = self
            .ledgers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ledger = ledgers.get(goal_id)?;
        Some(gate::evaluate(
            &ledger.gate_inputs(current_head.as_deref(), self.settings.clone()),
        ))
    }
}

impl GoalGate for GoalRegistry {
    fn evaluate_finish<'a>(
        &'a self,
        run: RunId,
    ) -> Pin<Box<dyn Future<Output = Option<GateVerdict>> + Send + 'a>> {
        Box::pin(async move {
            let goal_id = self.goal_for_run(run)?;
            self.evaluate_finish_for_goal(&goal_id).await
        })
    }
}
