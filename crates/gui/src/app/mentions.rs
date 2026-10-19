use std::path::PathBuf;
use std::sync::mpsc;

use workspace_ui::TrustState;

use super::WorkbenchState;
use crate::model::composer::MentionIndex;
use crate::model::tasks::AgentRunSource;

/// What an index covers: the project root and its allowed directories.
#[derive(Clone, PartialEq, Eq)]
struct Scope {
    root: PathBuf,
    allowed: Vec<PathBuf>,
}

pub(super) struct Job {
    scope: Scope,
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
        let scope = if active { self.mention_scope() } else { None };
        if let Some(scope) = scope {
            let index = &self.composer.mentions;
            let stale = index.root.as_ref() != Some(&scope.root) || index.allowed != scope.allowed;
            if (started || stale)
                && self
                    .mention_job
                    .as_ref()
                    .is_none_or(|job| job.scope != scope)
            {
                self.spawn_mention_index(scope);
            }
        }
        if self.mention_job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn spawn_mention_index(&mut self, scope: Scope) {
        let (send, result) = mpsc::sync_channel(1);
        let worker_scope = scope.clone();
        let spawned = std::thread::Builder::new()
            .name("mention-index".into())
            .spawn(move || {
                let Scope { root, allowed } = worker_scope;
                let skills = runtime::skill::build_standard_registry(Some(&root));
                let _ = send.send(MentionIndex::build(&root, &allowed, &skills));
            });
        match spawned {
            Ok(_) => self.mention_job = Some(Job { scope, result }),
            Err(error) => tracing::warn!(%error, "failed to start mention indexing"),
        }
    }

    /// The selected project's root plus the approved allowed directories that
    /// still exist, matching what the runtime declares to the agent.
    fn mention_scope(&self) -> Option<Scope> {
        let project_id = self.sidebar.selected_project.as_ref()?;
        let project = self
            .sidebar
            .projects
            .iter()
            .find(|project| &project.id == project_id)?;
        let allowed = project
            .allowed_directories
            .iter()
            .filter(|directory| directory.trust == TrustState::Approved)
            .map(|directory| directory.path.clone())
            .filter(|path| path.is_dir())
            .collect();
        Some(Scope {
            root: project.repo_root.clone(),
            allowed,
        })
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
