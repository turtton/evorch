use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ProjectError;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(String);

impl ProjectId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    Approved,
    #[default]
    Unapproved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedDirectory {
    pub path: PathBuf,
    pub trust: TrustState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub id: ProjectId,
    pub name: String,
    pub repo_root: PathBuf,
    pub allowed_directories: Vec<AllowedDirectory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Membership {
    ProjectRoot,
    RuntimeWorktree { run_dir: PathBuf },
    AllowedDirectory { trust: TrustState },
    Outside,
}

impl ProjectRecord {
    pub fn resolve_membership(&self, path: &Path) -> Membership {
        if path == self.repo_root {
            return Membership::ProjectRoot;
        }
        let worktrees = self.repo_root.join(".evorch/worktrees");
        if path.starts_with(&worktrees) {
            let run_dir = path
                .strip_prefix(&worktrees)
                .ok()
                .and_then(|relative| relative.components().next())
                .map_or_else(|| path.to_path_buf(), |run| worktrees.join(run));
            return Membership::RuntimeWorktree { run_dir };
        }
        self.allowed_directories
            .iter()
            .find(|directory| path.starts_with(&directory.path))
            .map_or(Membership::Outside, |directory| {
                Membership::AllowedDirectory {
                    trust: directory.trust,
                }
            })
    }

    pub(crate) fn add_allowed_directory(
        &mut self,
        path: &Path,
        trust: TrustState,
    ) -> Result<(), ProjectError> {
        let canonical = canonical_directory(path)?;
        if canonical.starts_with(&self.repo_root) {
            return Err(ProjectError::NestedInProjectRoot);
        }
        if self
            .allowed_directories
            .iter()
            .any(|directory| canonical == directory.path)
        {
            return Err(ProjectError::DuplicateAllowedDirectory);
        }
        if self
            .allowed_directories
            .iter()
            .any(|directory| canonical.starts_with(&directory.path))
        {
            return Err(ProjectError::NestedInExistingAllowed);
        }
        self.allowed_directories.push(AllowedDirectory {
            path: canonical,
            trust,
        });
        Ok(())
    }
}

pub(crate) fn canonical_directory(path: &Path) -> Result<PathBuf, ProjectError> {
    if !path.is_absolute() {
        return Err(ProjectError::NotAbsolute);
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| ProjectError::Canonicalize(path.display().to_string()))?;
    if !canonical.is_dir() {
        return Err(ProjectError::NotADirectory);
    }
    Ok(canonical)
}
