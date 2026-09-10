use super::WorkbenchState;
use crate::model::{composer::SlashCommandRegistry, tasks::AgentRunSource};
use std::{future::Future, sync::mpsc};

pub(super) enum Outcome {
    Discovery(SlashCommandRegistry),
    Output(String),
}

pub(super) struct Job {
    thread: Option<workspace_ui::ThreadId>,
    result: mpsc::Receiver<Result<Outcome, String>>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Job {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn start_external(
        &mut self,
        work: impl Future<Output = Result<Outcome, String>> + Send + 'static,
    ) {
        if self.external_job.is_some() {
            self.push_notice("An external command is already running");
            return;
        }
        let (send, result) = mpsc::sync_channel(1);
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("external-command".into())
            .spawn(move || {
                let outcome = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())
                    .and_then(|runtime| {
                        runtime.block_on(async {
                            tokio::select! {
                                _ = cancelled => Err("external command cancelled".into()),
                                result = work => result,
                            }
                        })
                    });
                let _ = send.send(outcome);
            });
        match worker {
            Ok(_) => {
                self.external_job = Some(Job {
                    thread: self.sidebar.active_thread.clone(),
                    result,
                    cancel: Some(cancel),
                })
            }
            Err(error) => self.push_notice(error.to_string()),
        }
    }

    pub fn cancel_external_command(&mut self) {
        if let Some(job) = &mut self.external_job
            && let Some(cancel) = job.cancel.take()
        {
            let _ = cancel.send(());
        }
    }

    pub fn external_command_running(&self) -> bool {
        self.external_job.is_some()
    }

    pub(super) fn poll_external(&mut self) {
        let Some(job) = &self.external_job else {
            return;
        };
        let result = match job.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("external command worker stopped".into()),
        };
        let same_thread = job.thread == self.sidebar.active_thread;
        self.external_job = None;
        match result {
            Ok(Outcome::Discovery(registry)) => self.composer.registry = registry,
            Ok(Outcome::Output(output)) if same_thread => self.push_notice(output),
            Err(error) if same_thread => self.push_notice(error),
            Ok(Outcome::Output(_)) | Err(_) => {}
        }
    }
}
