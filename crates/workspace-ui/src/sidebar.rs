use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::project::{canonical_directory, validate_allowed_directory};
use crate::{
    ProjectError, ProjectId, ProjectRecord, SidebarError, ThreadError, ThreadId, ThreadRecord,
    TrustState,
};

pub const SIDEBAR_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarState {
    pub version: u32,
    pub projects: Vec<ProjectRecord>,
    pub selected_project: Option<ProjectId>,
    pub threads: Vec<ThreadRecord>,
    pub active_thread: Option<ThreadId>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            version: SIDEBAR_SCHEMA_VERSION,
            projects: Vec::new(),
            selected_project: None,
            threads: Vec::new(),
            active_thread: None,
        }
    }
}

impl SidebarState {
    pub fn add_project(
        &mut self,
        id: ProjectId,
        name: impl Into<String>,
        repo_root: &Path,
    ) -> Result<(), ProjectError> {
        let repo_root = canonical_directory(repo_root)?;
        if self
            .projects
            .iter()
            .any(|project| project.repo_root == repo_root)
        {
            return Err(ProjectError::DuplicateProject);
        }
        self.projects.push(ProjectRecord {
            id,
            name: name.into(),
            repo_root,
            allowed_directories: Vec::new(),
        });
        Ok(())
    }

    pub fn select_project(&mut self, id: &ProjectId) -> Result<(), ProjectError> {
        if !self.projects.iter().any(|project| &project.id == id) {
            return Err(ProjectError::UnknownProject);
        }
        self.selected_project = Some(id.clone());
        Ok(())
    }

    pub fn add_allowed_directory(
        &mut self,
        project_id: &ProjectId,
        path: &Path,
        trust: TrustState,
    ) -> Result<(), ProjectError> {
        self.projects
            .iter_mut()
            .find(|project| &project.id == project_id)
            .ok_or(ProjectError::UnknownProject)?
            .add_allowed_directory(path, trust)
    }

    pub fn set_allowed_trust(
        &mut self,
        project_id: &ProjectId,
        path: &Path,
        trust: TrustState,
    ) -> Result<(), ProjectError> {
        self.projects
            .iter_mut()
            .find(|project| &project.id == project_id)
            .ok_or(ProjectError::UnknownProject)?
            .set_allowed_trust(path, trust)
    }

    pub fn create_thread(
        &mut self,
        id: ThreadId,
        project_id: ProjectId,
        title: impl Into<String>,
    ) -> Result<(), ThreadError> {
        if !self.projects.iter().any(|project| project.id == project_id) {
            return Err(ThreadError::UnknownProject);
        }
        if self.threads.iter().any(|thread| thread.id == id) {
            return Err(ThreadError::DuplicateThread);
        }
        self.threads.push(ThreadRecord::new(id, project_id, title));
        Ok(())
    }

    pub fn switch_thread(&mut self, id: &ThreadId) -> Result<(), ThreadError> {
        if !self.threads.iter().any(|thread| &thread.id == id) {
            return Err(ThreadError::UnknownThread);
        }
        self.active_thread = Some(id.clone());
        Ok(())
    }

    pub fn set_pinned(&mut self, id: &ThreadId, pinned: bool) -> Result<(), ThreadError> {
        self.thread_mut(id)?.pinned = pinned;
        Ok(())
    }

    pub fn set_paused(&mut self, id: &ThreadId, paused: bool) -> Result<(), ThreadError> {
        self.thread_mut(id)?.paused = paused;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), SidebarError> {
        if self.version != SIDEBAR_SCHEMA_VERSION {
            return Err(SidebarError::Validation(format!(
                "unsupported sidebar version {}; supported version is {SIDEBAR_SCHEMA_VERSION}",
                self.version
            )));
        }
        let mut projects = BTreeSet::new();
        for project in &self.projects {
            if !projects.insert(&project.id) {
                return Err(SidebarError::Validation(format!(
                    "duplicate project '{}'",
                    project.id
                )));
            }
            if !project.repo_root.is_absolute() || !project.repo_root.is_dir() {
                return Err(SidebarError::Validation(format!(
                    "invalid project root '{}'",
                    project.repo_root.display()
                )));
            }
            let mut validated_project = project.clone();
            validated_project.allowed_directories.clear();
            let mut allowed_directories = project.allowed_directories.iter().collect::<Vec<_>>();
            allowed_directories.sort_by_key(|directory| directory.path.components().count());
            for directory in allowed_directories {
                let validated = validate_allowed_directory(
                    &validated_project,
                    &directory.path,
                    directory.trust,
                )?;
                if validated.path != directory.path {
                    return Err(ProjectError::NotCanonical.into());
                }
                validated_project.allowed_directories.push(validated);
            }
        }
        if self
            .selected_project
            .as_ref()
            .is_some_and(|id| !projects.contains(id))
        {
            return Err(SidebarError::Validation(
                "selected project does not exist".to_owned(),
            ));
        }
        let mut threads = BTreeSet::new();
        for thread in &self.threads {
            if !threads.insert(&thread.id) {
                return Err(SidebarError::Validation(format!(
                    "duplicate thread '{}'",
                    thread.id
                )));
            }
            if !projects.contains(&thread.project_id) {
                return Err(SidebarError::Validation(format!(
                    "thread '{}' references unknown project",
                    thread.id
                )));
            }
        }
        if self
            .active_thread
            .as_ref()
            .is_some_and(|id| !threads.contains(id))
        {
            return Err(SidebarError::Validation(
                "active thread does not exist".to_owned(),
            ));
        }
        Ok(())
    }

    fn thread_mut(&mut self, id: &ThreadId) -> Result<&mut ThreadRecord, ThreadError> {
        self.threads
            .iter_mut()
            .find(|thread| &thread.id == id)
            .ok_or(ThreadError::UnknownThread)
    }
}
