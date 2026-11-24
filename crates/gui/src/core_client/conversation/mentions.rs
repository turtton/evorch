//! The composer's `@` index of the selected project's files and folders, built off
//! the UI thread as the embedded workbench builds its own, without skills: the core
//! offers no list of them yet.
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use crate::composer_input::MentionIndex;
use crate::composer_input::mentions::MAX_INDEXED_PATHS;
use crate::core_client::local::LocalRoots;

/// What an index covers: the project's root on this machine and its approved,
/// live allowed directories.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Scope {
    root: PathBuf,
    allowed: Vec<PathBuf>,
}

struct Job {
    scope: Scope,
    result: mpsc::Receiver<MentionIndex>,
}

#[derive(Default)]
pub(super) struct MentionWork {
    pub index: MentionIndex,
    scope: Option<Scope>,
    /// A mention was being typed last frame.
    active: bool,
    job: Option<Job>,
}

impl MentionWork {
    /// The selection's directories; without a project root on this machine nothing
    /// is indexed.
    pub fn set_scope(&mut self, roots: &LocalRoots) {
        self.scope = roots.project_root.ready().map(|root| Scope {
            root: root.to_path_buf(),
            allowed: roots.allowed.clone(),
        });
    }

    /// Rebuilds the index whenever a mention starts, so new files appear without a
    /// filesystem watcher, and keeps it within the current scope.
    pub fn refresh(&mut self, ctx: &egui::Context, mention: bool) {
        let started = mention && !self.active;
        self.active = mention;
        self.poll();
        let root = self.scope.as_ref().map(|scope| scope.root.as_path());
        if self.index.root.is_some() && self.index.root.as_deref() != root {
            self.index = MentionIndex::default();
        }
        let allowed = self
            .scope
            .as_ref()
            .map_or(&[][..], |scope| scope.allowed.as_slice());
        // A revoked directory stops completing before any rebuild lands.
        self.index.retain_allowed(allowed);
        if let Some(scope) = self.scope.clone().filter(|_| mention) {
            let stale = !self.index.is_built_for(&scope.root, &scope.allowed, false);
            if (started || stale) && self.job.as_ref().is_none_or(|job| job.scope != scope) {
                self.spawn(scope);
            }
        }
        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn spawn(&mut self, scope: Scope) {
        let (send, result) = mpsc::sync_channel(1);
        let Scope { root, allowed } = scope.clone();
        let spawned = std::thread::Builder::new()
            .name("mention-index".into())
            .spawn(move || {
                let index =
                    MentionIndex::index_within(&root, &allowed, [], false, MAX_INDEXED_PATHS);
                let _ = send.send(index);
            });
        match spawned {
            Ok(_) => self.job = Some(Job { scope, result }),
            Err(error) => tracing::warn!(%error, "failed to start mention indexing"),
        }
    }

    /// Takes a finished index, discarding it when the scope changed meanwhile.
    fn poll(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        match job.result.try_recv() {
            Ok(index) => self.land(index),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.job = None,
        }
    }

    fn land(&mut self, index: MentionIndex) {
        if let Some(job) = self.job.take()
            && self.scope.as_ref() == Some(&job.scope)
        {
            self.index = index;
        }
    }

    /// Blocks until an index build in flight has landed.
    #[cfg(test)]
    pub fn wait(&mut self) {
        if let Some(index) = self.job.as_ref().and_then(|job| job.result.recv().ok()) {
            self.land(index);
        }
    }

    #[cfg(test)]
    pub fn building(&self) -> bool {
        self.job.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_input::mention_query;
    use crate::core_client::local::LocalRoot;

    fn roots(root: &std::path::Path, allowed: &[&std::path::Path]) -> LocalRoots {
        LocalRoots {
            project_root: LocalRoot::Ready(root.into()),
            allowed: allowed.iter().map(|path| path.to_path_buf()).collect(),
            ..LocalRoots::default()
        }
    }

    fn labels(work: &MentionWork, input: &str) -> Vec<String> {
        let query = mention_query(input, None).expect("a mention");
        work.index
            .complete(&query)
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    fn files() -> (tempfile::TempDir, tempfile::TempDir) {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("README.md"), "").unwrap();
        let shared = tempfile::tempdir().unwrap();
        std::fs::write(shared.path().join("notes.md"), "").unwrap();
        (project, shared)
    }

    // Given: a project with an allowed directory
    // When: no mention is typed, then one is
    // Then: nothing is built until the mention, which lists both
    #[test]
    fn a_mention_builds_the_index_of_its_scope() {
        let (project, shared) = files();
        let ctx = egui::Context::default();
        let mut work = MentionWork::default();
        work.set_scope(&roots(project.path(), &[shared.path()]));
        work.refresh(&ctx, false);
        assert!(!work.building());

        work.refresh(&ctx, true);
        work.wait();

        let notes = format!("@{}/notes.md", shared.path().display());
        assert!(labels(&work, "@").contains(&"@README.md".to_owned()));
        assert_eq!(labels(&work, "@notes.md"), [notes]);
    }

    // Given: an index built while its directory was allowed
    // When: the directory is revoked
    // Then: its files stop completing before any rebuild lands
    #[test]
    fn a_revoked_directory_drops_its_entries_at_once() {
        let (project, shared) = files();
        let ctx = egui::Context::default();
        let mut work = MentionWork::default();
        work.set_scope(&roots(project.path(), &[shared.path()]));
        work.refresh(&ctx, true);
        work.wait();

        work.set_scope(&roots(project.path(), &[]));
        work.refresh(&ctx, true);

        assert!(labels(&work, "@notes.md").is_empty());
        work.wait();
        assert!(work.index.is_built_for(project.path(), &[], false));
    }

    // Given: a build for one project in flight
    // When: another project is selected before it lands
    // Then: its result is discarded
    #[test]
    fn a_result_for_an_old_scope_is_discarded() {
        let (project, other) = files();
        let ctx = egui::Context::default();
        let mut work = MentionWork::default();
        work.set_scope(&roots(project.path(), &[]));
        work.refresh(&ctx, true);

        work.set_scope(&roots(other.path(), &[]));
        work.wait();

        assert!(work.index.entries.is_empty());
        assert_eq!(work.index.root, None);
    }
}
