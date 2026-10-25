use std::path::PathBuf;
use std::sync::mpsc;

use workspace_ui::TrustState;

use super::WorkbenchState;
use crate::model::composer::{MentionIndex, mention_skills};
use crate::model::project_bridge::repo_skills_admitted;
use crate::model::tasks::AgentRunSource;

/// What an index covers: the project root and its approved allowed
/// directories, and whether the project's trust admits its repository skills.
/// Missing directories are left out by the index job, not here, so checking a
/// scope never touches the filesystem.
#[derive(Clone, PartialEq, Eq)]
struct Scope {
    root: PathBuf,
    allowed: Vec<PathBuf>,
    repo_skills: bool,
}

pub(super) struct Job {
    scope: Scope,
    result: mpsc::Receiver<MentionIndex>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Rebuilds the `@` index whenever a mention starts, so new files and
    /// skills appear without a filesystem watcher, and keeps it within the
    /// selected project's current allowed directories and trust.
    pub(super) fn refresh_mention_index(&mut self, ctx: &egui::Context) {
        let active = self.composer.mention_query().is_some();
        let started = active && !self.mention_active;
        self.mention_active = active;
        let scope = self.mention_scope();
        self.poll_mention_index(scope.as_ref());
        // A revoked or removed directory stops completing before any rebuild
        // lands; the rebuild then hands its share of the path budget on.
        let allowed = scope
            .as_ref()
            .map_or(&[][..], |scope| scope.allowed.as_slice());
        self.composer.mentions.retain_allowed(allowed);
        // Repository skills of a project that is not trusted would not be
        // inlined, so they stop completing before the rebuild lands as well.
        if scope.as_ref().is_some_and(|scope| !scope.repo_skills) {
            self.composer.mentions.withhold_repo_skills();
        }
        if let Some(scope) = scope.filter(|_| active) {
            let stale = !self.composer.mentions.is_built_for(
                &scope.root,
                &scope.allowed,
                scope.repo_skills,
            );
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
                let Scope {
                    root,
                    allowed,
                    repo_skills,
                } = worker_scope;
                let skills = mention_skills(Some(&root), repo_skills);
                let _ = send.send(MentionIndex::build(&root, &allowed, &skills, repo_skills));
            });
        match spawned {
            Ok(_) => self.mention_job = Some(Job { scope, result }),
            Err(error) => tracing::warn!(%error, "failed to start mention indexing"),
        }
    }

    /// The selected project's root plus its approved allowed directories,
    /// the ones the runtime declares to the agent when they exist, and whether
    /// the runtime gives its runs the project's repository skills.
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
            .collect();
        Some(Scope {
            root: project.repo_root.clone(),
            allowed,
            repo_skills: repo_skills_admitted(Some(project)),
        })
    }

    /// Takes a finished index, discarding it when the scope changed meanwhile.
    fn poll_mention_index(&mut self, scope: Option<&Scope>) {
        let Some(job) = &self.mention_job else {
            return;
        };
        match job.result.try_recv() {
            Ok(index) => {
                if scope == Some(&job.scope) {
                    self.composer.mentions = index;
                }
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
            && self.mention_scope() == Some(job.scope)
        {
            self.composer.mentions = index;
        }
    }
}

#[cfg(test)]
#[path = "mentions_tests.rs"]
mod tests;
