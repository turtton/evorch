//! Turns the ledger's commit notices into `usage_changed` events (ADR 0045 E4).
//!
//! The storage layer calls [`UsageNotices::record`] on the thread that wrote the
//! rows, after the commit. It only notes the run and wakes the task, so the writing
//! side never waits for a client or for the projection. The task publishes the
//! runs noted so far as one event and then holds back for an interval: however
//! many requests commit, a client is told at most once per interval, and the first
//! notice after a quiet spell goes out at once. The event is only a reason to ask
//! again; it carries no value.
use std::{collections::BTreeSet, future::Future, sync::Mutex, time::Duration};
use tokio::sync::Notify;
use workbench_protocol::{
    ids::RunId,
    namespaces::diagnostics::{MAX_USAGE_CHANGED_RUNS, UsageChange},
};

/// The shortest time between two `usage_changed` events.
pub(super) const USAGE_NOTICE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Pending {
    runs: BTreeSet<String>,
    all_runs: bool,
    dirty: bool,
}

#[derive(Default)]
pub(super) struct UsageNotices {
    pending: Mutex<Pending>,
    wake: Notify,
}

impl UsageNotices {
    /// Note the runs of a committed write (`None` for a request without a run).
    pub(super) fn record(&self, runs: &[Option<String>]) {
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.dirty = true;
            for run in runs {
                match run {
                    Some(run) if !pending.all_runs => {
                        pending.runs.insert(run.clone());
                    }
                    Some(_) => {}
                    None => pending.all_runs = true,
                }
            }
            if pending.runs.len() > MAX_USAGE_CHANGED_RUNS {
                pending.all_runs = true;
            }
            if pending.all_runs {
                pending.runs.clear();
            }
        }
        self.wake.notify_one();
    }

    fn take(&self) -> Option<UsageChange> {
        let pending = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !pending.dirty {
                return None;
            }
            std::mem::take(&mut *pending)
        };
        let runs: Result<Vec<RunId>, _> = pending.runs.into_iter().map(RunId::new).collect();
        Some(match runs {
            Ok(runs) if !pending.all_runs => UsageChange {
                runs,
                all_runs: false,
            },
            _ => UsageChange {
                runs: Vec::new(),
                all_runs: true,
            },
        })
    }

    /// Publish what was noted, at most once per `interval`, until `stop` resolves.
    pub(super) async fn run(
        &self,
        publish: impl Fn(UsageChange),
        interval: Duration,
        stop: impl Future<Output = ()>,
    ) {
        tokio::pin!(stop);
        loop {
            tokio::select! {
                () = &mut stop => return,
                () = self.wake.notified() => {}
            }
            if let Some(change) = self.take() {
                publish(change);
            }
            tokio::select! {
                () = &mut stop => return,
                () = tokio::time::sleep(interval) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::{sync::mpsc, time::Instant};

    fn run(id: &str) -> Option<String> {
        Some(id.to_owned())
    }

    fn start(
        notices: &Arc<UsageNotices>,
    ) -> (
        mpsc::UnboundedReceiver<(Instant, UsageChange)>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (events, received) = mpsc::unbounded_channel();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let notices = Arc::clone(notices);
        tokio::spawn(async move {
            notices
                .run(
                    |change| {
                        let _ = events.send((Instant::now(), change));
                    },
                    USAGE_NOTICE_INTERVAL,
                    async {
                        let _ = stopped.await;
                    },
                )
                .await;
        });
        (received, stop)
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_commits_is_one_event_naming_every_run_and_the_next_waits_an_interval() {
        let notices = Arc::new(UsageNotices::default());
        let (mut events, _stop) = start(&notices);
        notices.record(&[run("run-b"), run("run-a")]);
        notices.record(&[run("run-b")]);
        let (first_at, first) = events.recv().await.unwrap();
        assert_eq!(
            first,
            UsageChange {
                runs: vec![RunId::new("run-a").unwrap(), RunId::new("run-b").unwrap()],
                all_runs: false,
            }
        );

        // Commits during the hold-back are kept for one later event, however many.
        for _ in 0..50 {
            notices.record(&[run("run-c")]);
        }
        let (second_at, second) = events.recv().await.unwrap();
        assert_eq!(second.runs, vec![RunId::new("run-c").unwrap()]);
        assert!(second_at - first_at >= USAGE_NOTICE_INTERVAL);
        assert!(events.try_recv().is_err(), "the 50 commits made one event");
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_announced_without_a_commit() {
        let notices = Arc::new(UsageNotices::default());
        let (mut events, stop) = start(&notices);
        stop.send(()).unwrap();
        assert!(
            events.recv().await.is_none(),
            "the task ended without an event"
        );
    }

    #[test]
    fn a_request_without_a_run_or_too_many_runs_is_a_change_of_every_run() {
        let notices = UsageNotices::default();
        notices.record(&[run("run-1"), None]);
        assert_eq!(
            notices.take(),
            Some(UsageChange {
                runs: vec![],
                all_runs: true
            })
        );
        assert_eq!(notices.take(), None, "the notice was handed out once");

        let many: Vec<Option<String>> = (0..=MAX_USAGE_CHANGED_RUNS)
            .map(|index| run(&format!("run-{index}")))
            .collect();
        notices.record(&many);
        let change = notices.take().unwrap();
        assert!(change.all_runs && change.runs.is_empty());
        assert!(change.validate().is_ok());

        let exactly: Vec<Option<String>> = (0..MAX_USAGE_CHANGED_RUNS)
            .map(|index| run(&format!("run-{index}")))
            .collect();
        notices.record(&exactly);
        let change = notices.take().unwrap();
        assert_eq!(change.runs.len(), MAX_USAGE_CHANGED_RUNS);
        assert!(!change.all_runs);
    }

    #[test]
    fn an_id_the_wire_cannot_carry_is_a_change_of_every_run() {
        let notices = UsageNotices::default();
        notices.record(&[run("")]);
        let change = notices.take().unwrap();
        assert!(change.all_runs && change.runs.is_empty());
    }
}
