use std::fmt;
use std::path::{Component, Path, PathBuf};

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
    /// Whether the project's own instructions (`AGENTS.md`, repository skills) reach its runs.
    #[serde(default)]
    pub trust: TrustState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Membership {
    ProjectRoot,
    AllowedDirectory { trust: TrustState },
    Outside,
}

impl ProjectRecord {
    pub fn resolve_membership(&self, path: &Path) -> Membership {
        if path == self.repo_root {
            return Membership::ProjectRoot;
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
        let directory = validate_allowed_directory(self, path, trust)?;
        if is_too_broad(&directory.path) {
            return Err(ProjectError::TooBroad);
        }
        self.allowed_directories.push(directory);
        Ok(())
    }

    pub(crate) fn set_allowed_trust(
        &mut self,
        path: &Path,
        trust: TrustState,
    ) -> Result<(), ProjectError> {
        let path = stored_identity(path)?;
        let directory = self
            .allowed_directories
            .iter_mut()
            .find(|directory| directory.path == path)
            .ok_or(ProjectError::UnknownAllowedDirectory)?;
        directory.trust = trust;
        Ok(())
    }

    pub(crate) fn remove_allowed_directory(&mut self, path: &Path) -> Result<(), ProjectError> {
        let path = stored_identity(path)?;
        let before = self.allowed_directories.len();
        self.allowed_directories
            .retain(|directory| directory.path != path);
        if self.allowed_directories.len() == before {
            return Err(ProjectError::UnknownAllowedDirectory);
        }
        Ok(())
    }
}

pub(crate) fn validate_allowed_directory(
    project: &ProjectRecord,
    path: &Path,
    trust: TrustState,
) -> Result<AllowedDirectory, ProjectError> {
    let canonical = canonical_directory(path)?;
    check_placement(project, &canonical)?;
    Ok(AllowedDirectory {
        path: canonical,
        trust,
    })
}

/// Load-time check. A directory that no longer exists keeps its record and is
/// placed by its stored path, so a removed checkout never blocks startup.
pub(crate) fn validate_stored_allowed_directory(
    project: &ProjectRecord,
    directory: &AllowedDirectory,
) -> Result<(), ProjectError> {
    let path = stored_identity(&directory.path)?;
    if path.is_dir() {
        validate_allowed_directory(project, &path, directory.trust)?;
    } else {
        check_placement(project, &path)?;
    }
    Ok(())
}

fn check_placement(project: &ProjectRecord, path: &Path) -> Result<(), ProjectError> {
    if path.starts_with(&project.repo_root) {
        return Err(ProjectError::NestedInProjectRoot);
    }
    if project
        .allowed_directories
        .iter()
        .any(|directory| path == directory.path)
    {
        return Err(ProjectError::DuplicateAllowedDirectory);
    }
    if project
        .allowed_directories
        .iter()
        .any(|directory| path.starts_with(&directory.path))
    {
        return Err(ProjectError::NestedInExistingAllowed);
    }
    Ok(())
}

/// The stored form of a path: absolute and canonical while it exists, and
/// free of `.` / `..` once it no longer does.
fn stored_identity(path: &Path) -> Result<PathBuf, ProjectError> {
    if !path.is_absolute() {
        return Err(ProjectError::NotAbsolute);
    }
    let canonical = match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(_)
            if path
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_))) =>
        {
            path.to_path_buf()
        }
        Err(_) => return Err(ProjectError::NotCanonical),
    };
    if canonical != path {
        return Err(ProjectError::NotCanonical);
    }
    Ok(canonical)
}

/// Granting the filesystem root or the whole home directory would remove the boundary.
pub(crate) fn is_too_broad(path: &Path) -> bool {
    path == Path::new("/")
        || std::env::var_os("HOME")
            .and_then(|home| PathBuf::from(home).canonicalize().ok())
            .is_some_and(|home| home == path)
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
