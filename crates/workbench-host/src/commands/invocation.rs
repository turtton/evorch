//! Starting the run a CLI invocation owns.

use std::path::PathBuf;

use runtime::{Role, RunConfig, RunId};

use super::CommandService;

/// What the core needs to start one invocation's run.
pub(crate) struct InvocationRun {
    pub thread: String,
    pub role: Role,
    pub prompt: String,
    pub project_root: PathBuf,
    /// Names the prompt's host-input receipt; the invocation's own id.
    pub submission_id: String,
}

impl CommandService {
    /// Start a one-shot run of `run.role` on `run.thread` (a new thread, or the
    /// next incarnation of an existing one) and bind the thread to it, so that
    /// its questions and approvals reach every client. The run neither waits for
    /// further input nor keeps itself alive after its turn: when it ends, the
    /// invocation is done with it.
    pub(crate) fn start_invocation_run(&mut self, run: InvocationRun) -> Result<RunId, String> {
        if self.root_recovery_pending(&run.thread) {
            return Err("restart_busy: restoring conversation; retry the same request".into());
        }
        let permit = match &self.ownership {
            None => None,
            Some(owner) => {
                match owner.attach(&run.thread) {
                    Ok(_) => (),
                    Err(runtime::ownership::RegistryError::Absent) => {
                        owner
                            .start(&run.thread)
                            .map_err(|_| "thread could not be reserved".to_owned())?;
                    }
                    Err(_) => return Err("thread ownership unavailable".into()),
                }
                Some(
                    owner
                        .owned_permit(&run.thread)
                        .map_err(|error| error.to_string())?,
                )
            }
        };
        let mut config = RunConfig {
            project_root: Some(run.project_root.clone()),
            ownership: permit.clone(),
            initial_prompt_id: Some(run.submission_id),
            ..RunConfig::default()
        };
        config.memory = self.capture_memory(config.project_root.as_deref());
        let _guard = self.handle.enter();
        let run_id = self
            .runtime
            .delegate_chat_seeded(&run.thread, run.role, run.prompt, config, None)
            .map_err(|error| error.to_string())?;
        self.bindings
            .thread_roots
            .insert(run.thread.clone(), run.project_root);
        if let Some(permit) = permit {
            self.bindings
                .chat_permits
                .insert(run.thread.clone(), permit);
        }
        self.bindings.stop_marked.remove(&run.thread);
        self.bindings.stopped_by_us.remove(&run.thread);
        self.bindings.chat_runs.insert(run.thread, run_id);
        self.publish_lane_bindings();
        Ok(run_id)
    }
}
