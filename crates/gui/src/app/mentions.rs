use std::path::PathBuf;
use std::sync::mpsc;

use super::WorkbenchState;
use crate::model::composer::MentionIndex;
use crate::model::tasks::AgentRunSource;

pub(super) struct Job {
    root: PathBuf,
    result: mpsc::Receiver<MentionIndex>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Rebuilds the `@` index whenever a mention starts, so new files and
    /// skills appear without a filesystem watcher.
    pub(super) fn refresh_mention_index(&mut self, ctx: &egui::Context) {
        self.poll_mention_index();
        let active = self.composer.mention_query().is_some();
        let started = active && !self.mention_active;
        self.mention_active = active;
        let Some(root) = self.active_repo_root() else {
            return;
        };
        let stale = self.composer.mentions.root.as_ref() != Some(&root);
        if (started || (active && stale))
            && self.mention_job.as_ref().is_none_or(|job| job.root != root)
        {
            let (send, result) = mpsc::sync_channel(1);
            let worker_root = root.clone();
            let spawned = std::thread::Builder::new()
                .name("mention-index".into())
                .spawn(move || {
                    let skills = runtime::skill::build_standard_registry(Some(&worker_root));
                    let _ = send.send(MentionIndex::build(&worker_root, &skills));
                });
            match spawned {
                Ok(_) => self.mention_job = Some(Job { root, result }),
                Err(error) => tracing::warn!(%error, "failed to start mention indexing"),
            }
        }
        if self.mention_job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn poll_mention_index(&mut self) {
        let Some(job) = &self.mention_job else {
            return;
        };
        match job.result.try_recv() {
            Ok(index) => {
                self.composer.mentions = index;
                self.mention_job = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.mention_job = None,
        }
    }

    /// Blocks until any in-flight `@` index build has landed.
    pub fn wait_mention_index(&mut self) {
        if let Some(job) = self.mention_job.take()
            && let Ok(index) = job.result.recv()
        {
            self.composer.mentions = index;
        }
    }
}
