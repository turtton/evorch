use std::time::{Duration, Instant};

use event_bus::WorkspaceWait;

use super::TelemetryOverlay;

/// A live wait. Holder changes update its details without restarting its timer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceWaitEntry {
    pub waiting: WorkspaceWait,
    started_at: Instant,
}

impl WorkspaceWaitEntry {
    pub fn elapsed_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started_at)
    }
}

impl TelemetryOverlay {
    pub(super) fn update_workspace_wait(
        &mut self,
        run_id: &str,
        call_id: &str,
        waiting: Option<&WorkspaceWait>,
        now: Instant,
    ) {
        let key = (run_id.to_owned(), call_id.to_owned());
        if let Some(waiting) = waiting {
            self.workspace_waits
                .entry(key)
                .and_modify(|entry| entry.waiting = waiting.clone())
                .or_insert_with(|| WorkspaceWaitEntry {
                    waiting: waiting.clone(),
                    started_at: now,
                });
        } else {
            self.workspace_waits.remove(&key);
        }
    }

    pub(super) fn clear_workspace_waits(&mut self, run_id: &str) {
        self.workspace_waits.retain(|(run, _), _| run != run_id);
    }

    /// Include waits from any run owned by the thread, including its subagents.
    pub fn workspace_waits<'a>(
        &'a self,
        run_ids: &'a [String],
    ) -> impl Iterator<Item = (&'a str, &'a str, &'a WorkspaceWaitEntry)> {
        self.workspace_waits
            .iter()
            .filter(move |((run, _), _)| run_ids.contains(run))
            .map(|((run, call), entry)| (run.as_str(), call.as_str(), entry))
    }
}

#[cfg(test)]
mod tests {
    use event_bus::{AgentRunPhase, Event, LifecycleEvent, WorkspaceLockHolder};

    use super::*;

    fn changed(run: &str, call: &str, waiting: Option<WorkspaceWait>) -> Event {
        Event::new(LifecycleEvent::WorkspaceWaitChanged {
            run_id: run.into(),
            call_id: call.into(),
            waiting,
        })
    }

    fn wait() -> WorkspaceWait {
        WorkspaceWait {
            workspace_root: "/tmp/workspace".into(),
            tool_name: "shell".into(),
            command: Some("rg needle".into()),
            holder: None,
        }
    }

    #[test]
    fn holder_updates_keep_elapsed_and_clears_only_the_matching_call() {
        let mut overlay = TelemetryOverlay::new();
        let now = Instant::now();
        let runs = vec!["root".into(), "child".into()];
        overlay.apply_event_at(&changed("child", "first", Some(wait())), now);
        overlay.apply_event_at(&changed("child", "second", Some(wait())), now);
        overlay.apply_event_at(&changed("unrelated", "first", Some(wait())), now);
        let mut updated = wait();
        updated.holder = Some(WorkspaceLockHolder {
            run_id: "holder".into(),
            call_id: "holder-call".into(),
            tool_name: "shell".into(),
            command: Some("gh pr checks --watch".into()),
        });
        overlay.apply_event_at(
            &changed("child", "first", Some(updated.clone())),
            now + Duration::from_secs(30),
        );
        let waits = overlay.workspace_waits(&runs).collect::<Vec<_>>();
        assert_eq!(waits.len(), 2);
        assert_eq!(waits[0].2.waiting, updated);
        assert_eq!(
            waits[0].2.elapsed_at(now + Duration::from_secs(90)),
            Duration::from_secs(90)
        );
        overlay.apply_event_at(&changed("child", "first", None), now);
        let waits = overlay.workspace_waits(&runs).collect::<Vec<_>>();
        assert_eq!(waits.len(), 1);
        assert_eq!(waits[0].1, "second");
        overlay.apply_event_at(&changed("child", "first", None), now);
        assert_eq!(overlay.workspace_waits(&runs).count(), 1);
    }

    #[test]
    fn terminal_states_restart_and_history_discard_live_waits() {
        let now = Instant::now();
        let runs = vec!["run".into()];
        let other = vec!["other".into()];
        for to in [
            AgentRunPhase::Stopped,
            AgentRunPhase::Done,
            AgentRunPhase::Error,
        ] {
            let mut overlay = TelemetryOverlay::new();
            overlay.apply_event_at(&changed("run", "call", Some(wait())), now);
            overlay.apply_event_at(&changed("other", "call", Some(wait())), now);
            overlay.apply_event_at(
                &Event::new(LifecycleEvent::AgentRunStateChanged {
                    run_id: "run".into(),
                    from: AgentRunPhase::Running,
                    to,
                    reason: None,
                }),
                now,
            );
            assert_eq!(overlay.workspace_waits(&runs).count(), 0);
            assert_eq!(overlay.workspace_waits(&other).count(), 1);
            overlay.apply_event_at(
                &Event::new(LifecycleEvent::AgentRunStarted {
                    run_id: "other".into(),
                    parent_run_id: None,
                    agent_name: "chat:other".into(),
                    role: "worker".into(),
                }),
                now,
            );
            assert_eq!(overlay.workspace_waits(&other).count(), 0);
            overlay.apply_event_at(&changed("run", "call", Some(wait())), now);
            overlay.finish_history();
            assert_eq!(overlay.workspace_waits(&runs).count(), 0);
        }
    }
}
