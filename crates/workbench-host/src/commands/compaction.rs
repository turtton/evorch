use runtime::RunConfig;

use super::{CommandService, HostEvent};

pub(super) struct CompactionJob {
    result: tokio::sync::oneshot::Receiver<HostEvent>,
    pub(super) task: tokio::task::JoinHandle<()>,
}

impl CommandService {
    pub(super) fn compact_chat(
        &mut self,
        thread_id: String,
        permit: Option<runtime::ownership::OwnerPermit>,
        restart: Option<crate::bootstrap::restart::HostCommandPermit>,
    ) -> Vec<HostEvent> {
        let result: Result<String, String> = (|| {
            let run = *self
                .bindings
                .chat_runs
                .get(&thread_id)
                .or_else(|| self.bindings.goal_runs.get(&thread_id))
                .ok_or("No active conversation run to compact")?;
            if self.runtime.inspect_agent(run).ok().is_some_and(|run| {
                matches!(
                    run.phase,
                    event_bus::AgentRunPhase::Pending
                        | event_bus::AgentRunPhase::Running
                        | event_bus::AgentRunPhase::Waiting
                )
            }) {
                self.runtime
                    .compact(run)
                    .map_err(|error| format!("Could not request context compaction: {error}"))?;
                return Ok("Context compaction requested for the next turn boundary".into());
            }
            if self
                .runtime
                .restore_diagnostics(run)
                .map_err(|error| format!("Could not request context compaction: {error}"))?
                .is_none()
            {
                return Err(
                    "Could not request context compaction: no saved conversation context".into(),
                );
            }
            let mut authority = RunConfig {
                ownership: permit,
                ..Default::default()
            };
            self.renew_team_authority(&thread_id, run, None, &mut authority)?;
            let runtime = self.runtime.clone();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let thread = thread_id.clone();
            let completion = self.background_work.clone();
            let task = self.handle.spawn(async move {
                // Include provider work and the final persisted snapshot in host drain.
                let _accepted = restart;
                let event = match runtime.compact_saved_chat(run, authority).await {
                    Ok(()) => HostEvent::applied_notice(
                        thread,
                        "Context compaction complete. Use /continue when ready to resume",
                    ),
                    Err(error) => HostEvent::notice(
                        thread,
                        format!("Could not compact saved conversation: {error}"),
                    ),
                };
                let _ = sender.send(event);
                completion.notify_one();
            });
            self.compaction_jobs.insert(
                thread_id.clone(),
                CompactionJob {
                    result: receiver,
                    task,
                },
            );
            Ok("Compacting saved conversation without starting an agent turn".into())
        })();
        vec![match result {
            Ok(text) => HostEvent::applied_notice(thread_id, text),
            Err(text) => HostEvent::notice(thread_id, text),
        }]
    }

    pub(super) fn poll_compactions(&mut self) -> Vec<HostEvent> {
        let mut events = Vec::new();
        self.compaction_jobs
            .retain(|thread, job| match job.result.try_recv() {
                Ok(event) => {
                    events.push(event);
                    false
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => true,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    events.push(HostEvent::notice(
                        thread.clone(),
                        "Saved context compaction task ended without a result; retry /compaction",
                    ));
                    false
                }
            });
        events
    }
}
