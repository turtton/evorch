//! Resolving the project ids clients send to the directories runs work in.
//!
//! Protocol clients name a project by an opaque `ProjectId`; the commands need
//! its root directory. The directory is a seam: the process that composes the
//! host (the GUI today, the core binary later) knows its projects and registers
//! them here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use workbench_protocol::ids::ProjectId;

/// Maps a project id to the root directory its threads' runs work in.
pub trait ProjectDirectory: Send {
    fn root_of(&self, project: &ProjectId) -> Option<PathBuf>;
    /// Projects exposed by this directory. Older resolvers may expose none.
    fn entries(&self) -> Vec<(ProjectId, PathBuf)> {
        Vec::new()
    }
}

/// A fixed set of projects, registered by the composing process.
#[derive(Debug, Clone, Default)]
pub struct StaticProjects {
    roots: BTreeMap<String, PathBuf>,
}

impl StaticProjects {
    pub fn insert(&mut self, project: &ProjectId, root: PathBuf) {
        self.roots.insert(project.as_str().to_owned(), root);
    }

    pub fn remove(&mut self, project: &ProjectId) {
        self.roots.remove(project.as_str());
    }
}

impl ProjectDirectory for StaticProjects {
    fn entries(&self) -> Vec<(ProjectId, PathBuf)> {
        self.roots
            .iter()
            .filter_map(|(id, root)| Some((ProjectId::new(id).ok()?, root.clone())))
            .collect()
    }
    fn root_of(&self, project: &ProjectId) -> Option<PathBuf> {
        self.roots.get(project.as_str()).cloned()
    }
}
