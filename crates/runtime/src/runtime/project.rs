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
    pub(crate) root: PathBuf,
    pub(crate) rules: Option<Arc<RulesSource>>,
    /// `None` while the project is not a git repository root (isolated runs fail closed).
    worktrees: Option<WorktreeManager>,
    pub(crate) model: Arc<dyn AgentModel>,
    /// Whether `<root>/.evorch/skills` may reach this project's runs.
    pub(crate) repo_skills: bool,
    slug: OnceLock<String>,
}

impl ProjectContext {
    pub(crate) fn isolated(&self, workspace: &WorkspaceContext) -> Option<IsolatedWorkspace> {
        Some(IsolatedWorkspace {
            manager: self.worktrees.clone()?,
            factory: Arc::clone(&workspace.factory),
        })
    }
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
        let mut projects = self
            .projects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(project) = projects.get(root) {
            return Arc::clone(project);
        }
        let worktrees = self.workspace.as_ref().and_then(|_| {
            crate::workspace::Project::new(root.to_path_buf())
                .map(WorktreeManager::new)
                .inspect_err(|error| {
                    tracing::warn!(
                        %error,
                        root = %root.display(),
                        "isolated workspaces are unavailable for this project"
                    );
                })
                .ok()
        });
        let model = self
            .project_models
            .get()
            .and_then(|resolve| resolve(root))
            .unwrap_or_else(|| Arc::clone(&self.model));
        let declared = self.declared_trust(root);
        let project = Arc::new(ProjectContext {
            root: root.to_path_buf(),
            rules: self.rules().map(|rules| {
                let rules = rules.with_project_root(Some(root.to_path_buf()));
                Arc::new(match declared {
                    Some(trust) => rules.with_trust(trust),
                    None => rules,
                })
            }),
            worktrees,
            model,
            repo_skills: declared.is_none_or(|trust| trust == ProjectTrust::Approved),
            slug: OnceLock::new(),
        });
        projects.insert(root.to_path_buf(), Arc::clone(&project));
        project
    }

    pub(crate) fn declared_trust(&self, root: &std::path::Path) -> Option<ProjectTrust> {
        self.project_trust
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .copied()
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
        Some(
            self.project(root)
                .slug
                .get_or_init(|| resolve(root))
                .clone(),
        )
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

    /// Lets the host name each project for project-partitioned records, so a
    /// run's learned memory lands in the project it worked in. First wins.
    #[must_use]
    pub fn with_project_slugs(self, resolver: ProjectSlugResolver) -> Self {
        let _ = self.shared.project_slugs.set(resolver);
        self
    }
}
