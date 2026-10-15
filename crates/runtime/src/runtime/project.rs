//! Per-run project binding: every run works in the project it was started in.
//!
//! A root run takes its project from [`RunConfig::project_root`] or, when the
//! host leaves it unset, from the project active at registration. Children
//! always inherit their parent's project, so switching the active project never
//! moves work that is already in flight.

use super::*;
use crate::rules::ProjectTrust;

/// Composes the model a project's runs use; `None` keeps the runtime model.
pub type ProjectModelResolver =
    Arc<dyn Fn(&std::path::Path) -> Option<Arc<dyn AgentModel>> + Send + Sync>;

/// Names a project root for project-partitioned records such as learned memory.
pub type ProjectSlugResolver = Arc<dyn Fn(&std::path::Path) -> String + Send + Sync>;

/// Project-scoped inputs resolved once per project root and shared by its runs.
pub(crate) struct ProjectContext {
    /// The canonical root, so every spelling of a project mounts and reads the same path.
    pub(crate) root: PathBuf,
    pub(crate) rules: Option<Arc<RulesSource>>,
    pub(crate) model: Arc<dyn AgentModel>,
    /// Whether `<root>/.evorch/skills` may reach this project's runs.
    pub(crate) repo_skills: bool,
    slug: OnceLock<String>,
}

impl Shared {
    /// The project new root runs bind to when the host does not name one.
    pub(crate) fn active_project_root(&self) -> Option<PathBuf> {
        self.active_project_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn project(&self, root: &std::path::Path) -> Arc<ProjectContext> {
        let root = canonical_root(root);
        let mut projects = self
            .projects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(project) = projects.get(&root) {
            return Arc::clone(project);
        }
        let model = self
            .project_models
            .get()
            .and_then(|resolve| resolve(&root))
            .unwrap_or_else(|| Arc::clone(&self.model));
        let declared = self.declared_trust(&root);
        let project = Arc::new(ProjectContext {
            root: root.clone(),
            rules: self.rules().map(|rules| {
                let rules = rules.with_project_root(Some(root.clone()));
                Arc::new(match declared {
                    Some(trust) => rules.with_trust(trust),
                    None => rules,
                })
            }),
            model,
            repo_skills: declared.is_none_or(|trust| trust == ProjectTrust::Approved),
            slug: OnceLock::new(),
        });
        projects.insert(root, Arc::clone(&project));
        project
    }

    pub(crate) fn declared_trust(&self, root: &std::path::Path) -> Option<ProjectTrust> {
        self.project_trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&canonical_root(root))
            .copied()
    }

    /// The live allowed-directory list of a project. Created empty on first use,
    /// so a run started before the host declares anything still sees later changes.
    pub(crate) fn project_allowed_directories(&self, root: &std::path::Path) -> sandbox::LivePaths {
        self.allowed_directories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(canonical_root(root))
            .or_default()
            .clone()
    }

    pub(crate) fn run_project(&self, config: &RunConfig) -> Option<Arc<ProjectContext>> {
        config
            .project_root
            .as_deref()
            .map(|root| self.project(root))
    }

    /// The project of a registered run, if it is bound to one.
    pub(crate) fn project_of(&self, run: RunId) -> Option<Arc<ProjectContext>> {
        let root = lock_runs(&self.runs)
            .get(&run)
            .and_then(|entry| entry.config.project_root.clone())?;
        Some(self.project(&root))
    }

    /// The record partition of a run's project, when the host names projects.
    pub(crate) fn project_slug(&self, root: &std::path::Path) -> Option<String> {
        let resolve = self.project_slugs.get()?;
        let project = self.project(root);
        Some(project.slug.get_or_init(|| resolve(&project.root)).clone())
    }

    /// The model that serves `config`'s project.
    pub(crate) fn model_for(&self, config: &RunConfig) -> Arc<dyn AgentModel> {
        self.run_project(config).map_or_else(
            || Arc::clone(&self.model),
            |project| Arc::clone(&project.model),
        )
    }

    /// Children inherit their parent's project; roots fall back to the active one.
    pub(crate) fn bind_project(&self, parent: Option<RunId>, config: &mut RunConfig) {
        if let Some(parent) = parent
            && let Some(root) = lock_runs(&self.runs)
                .get(&parent)
                .and_then(|entry| entry.config.project_root.clone())
        {
            config.project_root = Some(root);
        }
        if config.project_root.is_none() {
            config.project_root = self.active_project_root();
        }
    }
}

impl AgentRuntime {
    /// Lets the host compose each project's model, for example from its
    /// selected role profile. Set once before starting runs; first wins.
    #[must_use]
    pub fn with_project_models(self, resolver: ProjectModelResolver) -> Self {
        let _ = self.shared.project_models.set(resolver);
        self
    }

    /// Declares whether a project's own instructions (`AGENTS.md` rules and
    /// `.evorch/skills`) may reach its runs. Applies to runs started afterwards.
    pub fn set_project_trust(&self, root: PathBuf, trust: ProjectTrust) {
        let root = canonical_root(&root);
        self.shared
            .project_trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), trust);
        // Rebuilt on next use; running runs keep the context they started with.
        self.shared
            .projects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&root);
    }

    /// Declares the trusted allowed directories of a project (ADR 0032). Running
    /// runs see the new list from their next command or file tool call; a
    /// background shell job keeps the mounts it started with. Missing paths are
    /// skipped so they never block a run.
    pub fn set_project_allowed_directories(&self, root: PathBuf, directories: Vec<PathBuf>) {
        let mut paths = Vec::with_capacity(directories.len());
        for directory in directories {
            match directory.canonicalize() {
                Ok(path) if path.is_dir() => {
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
                Ok(_) => {
                    tracing::warn!(directory = %directory.display(), "allowed directory is not a directory; skipped");
                }
                Err(error) => {
                    tracing::warn!(%error, directory = %directory.display(), "allowed directory is unavailable; skipped");
                }
            }
        }
        self.shared.project_allowed_directories(&root).set(paths);
    }

    /// The live list of a project's declared, existing allowed directories, the same
    /// handle its runs mount. It exists whether or not runs are sandboxed.
    pub fn project_allowed_directories(&self, root: &std::path::Path) -> sandbox::LivePaths {
        self.shared.project_allowed_directories(root)
    }

    /// Lets the host name each project for project-partitioned records, so a
    /// run's learned memory lands in the project it worked in. First wins.
    #[must_use]
    pub fn with_project_slugs(self, resolver: ProjectSlugResolver) -> Self {
        let _ = self.shared.project_slugs.set(resolver);
        self
    }
}

/// Project-keyed state (trust, contexts, allowed directories) is keyed by the
/// canonical root so a host and a run naming the same project differently agree.
fn canonical_root(root: &std::path::Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}
