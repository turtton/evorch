//! Read-only directories a thread's runs may read besides the workspace (ADR 0037).
//!
//! The host declares the list per thread. A run reads its thread's list once,
//! when it builds its sandbox, and mounts it as `WorkspaceMounts::ro_binds`, so
//! the shell sandbox and the direct file tools both get it through the one
//! boundary (ADR 0033). Delegated children resolve the thread the way scratch
//! does and stay read-only; an escalation copies the list to its child thread.
//! A run that can delegate also follows later declarations, only to tell what a
//! Worker it delegates next would mount; its own mounts stay fixed.

use std::path::{Path, PathBuf};

use crate::AgentRuntime;
use crate::agent_loop::LoopState;

impl AgentRuntime {
    /// Declares the read-only directories of `thread_id`'s runs (ADR 0037). Runs
    /// started afterwards mount them read-only whatever their role; running runs
    /// keep the mounts they started with. Paths are resolved to their real
    /// location, entries that are not existing directories are skipped, and an
    /// empty result removes the declaration.
    pub fn set_thread_read_only_directories(&self, thread_id: &str, directories: Vec<PathBuf>) {
        let mut paths = Vec::with_capacity(directories.len());
        for directory in directories {
            match directory.canonicalize() {
                Ok(path) if path.is_dir() => {
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
                Ok(_) => {
                    tracing::warn!(thread_id, directory = %directory.display(), "thread read-only directory is not a directory; skipped");
                }
                Err(error) => {
                    tracing::warn!(%error, thread_id, directory = %directory.display(), "thread read-only directory is unavailable; skipped");
                }
            }
        }
        let mut declared = self.thread_read_dirs_lock();
        if paths.is_empty() {
            declared.remove(thread_id);
        } else {
            declared.insert(thread_id.to_owned(), paths);
        }
    }

    /// The canonical directories declared for `thread_id`, in declaration order.
    pub(crate) fn thread_read_only_directories(&self, thread_id: &str) -> Vec<PathBuf> {
        self.thread_read_dirs_lock()
            .get(thread_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Gives `child` a copy of `source`'s declaration when an escalation hands the
    /// thread off. Callers holding the goal lock take this lock after it, never before.
    pub(crate) fn inherit_thread_read_only_directories(&self, source: &str, child: &str) {
        let mut declared = self.thread_read_dirs_lock();
        match declared.get(source).cloned() {
            Some(paths) => {
                declared.insert(child.to_owned(), paths);
            }
            None => {
                declared.remove(child);
            }
        }
    }

    /// The part of `thread_id`'s declaration a run in `workspace_root` with `scratch`
    /// may mount; see [`mountable_thread_read_dirs`]. A directory that is gone, or that
    /// now resolves elsewhere (a link swapped in after the declaration), is skipped.
    pub(crate) fn mountable_thread_read_only_directories(
        &self,
        thread_id: &str,
        workspace_root: &Path,
        scratch: Option<&Path>,
    ) -> Vec<PathBuf> {
        self.mountable_declared_directories(
            thread_id,
            self.thread_read_only_directories(thread_id),
            workspace_root,
            scratch,
        )
    }

    /// [`Self::mountable_thread_read_only_directories`] for an already read declaration.
    fn mountable_declared_directories(
        &self,
        thread_id: &str,
        declared: Vec<PathBuf>,
        workspace_root: &Path,
        scratch: Option<&Path>,
    ) -> Vec<PathBuf> {
        let declared = declared
            .into_iter()
            .filter(|dir| match dir.canonicalize() {
                Ok(real) if &real == dir && real.is_dir() => true,
                Ok(real) => {
                    tracing::warn!(thread_id, directory = %dir.display(), resolved = %real.display(), "thread read-only directory changed since it was declared; not mounted");
                    false
                }
                Err(error) => {
                    tracing::warn!(%error, thread_id, directory = %dir.display(), "thread read-only directory is unavailable; not mounted");
                    false
                }
            })
            .collect::<Vec<_>>();
        if declared.is_empty() {
            return declared;
        }
        let outputs = tools::output::output_root().ok();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        mountable_thread_read_dirs(
            &declared,
            workspace_root,
            scratch,
            outputs.as_deref(),
            home.as_deref(),
            self.shared
                .run_store
                .get()
                .map(|store| store.db_path.as_path()),
        )
    }

    fn thread_read_dirs_lock(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Vec<PathBuf>>> {
        self.shared
            .thread_read_dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The declared `dirs` that may join a run's boundary as read-only mounts.
///
/// Read-only mounts come after the scratch and before the caches and the project
/// root (`SandboxBoundary::regions`). A directory around the scratch, tool output
/// root or project root would turn them read-only or expose their neighbours, and
/// one inside them would be listed as read-only while another mount decides its
/// access, so both are skipped. `/` and `$HOME` (and anything around `$HOME`) are
/// never mounted. `dirs` are canonical; the other paths are resolved here.
pub(crate) fn mountable_thread_read_dirs(
    dirs: &[PathBuf],
    workspace_root: &Path,
    scratch: Option<&Path>,
    outputs: Option<&Path>,
    home: Option<&Path>,
    database: Option<&Path>,
) -> Vec<PathBuf> {
    dirs.iter().filter(|dir| {
        let reason = read_directory_rejection(dir, Some(workspace_root), scratch, outputs, home, database);
        if let Some(reason) = reason {
            tracing::warn!(directory = %dir.display(), reason, "thread read-only directory overlaps a protected path; not mounted");
        }
        reason.is_none()
    }).cloned().collect()
}

fn read_directory_rejection(
    dir: &Path,
    project: Option<&Path>,
    scratch: Option<&Path>,
    outputs: Option<&Path>,
    home: Option<&Path>,
    database: Option<&Path>,
) -> Option<&'static str> {
    if dir.parent().is_none() {
        return Some("the filesystem root");
    }
    if home.is_some_and(|home| resolve_existing(home).starts_with(dir)) {
        return Some("the home directory or one of its parents");
    }
    for (name, path) in [
        ("project root", project),
        ("scratch", scratch),
        ("tool output root", outputs),
    ] {
        if let Some(path) = path {
            let path = resolve_existing(path);
            if path.starts_with(dir) || dir.starts_with(path) {
                return Some(name);
            }
        }
    }
    if database.is_some_and(|db| {
        storage::watch_exclusions(db)
            .iter()
            .any(|path| resolve_existing(path).starts_with(dir))
    }) {
        return Some("database");
    }
    None
}

impl LoopState {
    /// The thread's declared read-only directories this run mounts under `root`,
    /// read once when its sandbox is built.
    pub(crate) fn thread_read_dirs_to_mount(&self, root: &Path) -> Vec<PathBuf> {
        self.runtime()
            .and_then(|runtime| {
                let thread = runtime.thread_of_run(self.task.run_id)?;
                Some(runtime.mountable_thread_read_only_directories(
                    &thread,
                    root,
                    self.scratch_dir(),
                ))
            })
            .unwrap_or_default()
    }

    /// The thread's read-only directories a Worker this run delegated now would mount.
    /// It resolves the same thread and mounts under the same root, but reads the current
    /// declaration, which can differ from this run's own mounts. The result is checked
    /// again only when the declaration changes, not at every request boundary.
    pub(crate) fn thread_read_dirs_for_delegates(&mut self) -> Vec<PathBuf> {
        let Some(root) = self.thread_mount_root.clone() else {
            return Vec::new();
        };
        let Some(runtime) = self.runtime() else {
            return Vec::new();
        };
        let Some(thread) = runtime.thread_of_run(self.task.run_id) else {
            return Vec::new();
        };
        let declared = runtime.thread_read_only_directories(&thread);
        if self.delegated_thread_dirs.declared.as_ref() != Some(&declared) {
            let mountable = runtime.mountable_declared_directories(
                &thread,
                declared.clone(),
                &root,
                self.scratch_dir(),
            );
            self.delegated_thread_dirs = DelegatedThreadDirs {
                declared: Some(declared),
                mountable,
            };
        }
        self.delegated_thread_dirs.mountable.clone()
    }
}

/// The mountable part of the thread declaration it was computed from.
#[derive(Debug, Default)]
pub(crate) struct DelegatedThreadDirs {
    declared: Option<Vec<PathBuf>>,
    mountable: Vec<PathBuf>,
}

/// GUI preflight for a proposed thread RO grant. The runtime repeats these
/// rules at run start with the actual thread scratch directory.
pub fn validate_thread_read_directory(
    dir: &Path,
    project: Option<&Path>,
    database: Option<&Path>,
) -> Result<PathBuf, String> {
    let dir = dir
        .canonicalize()
        .ok()
        .filter(|path| path.is_dir())
        .ok_or_else(|| format!("Draft directory does not exist: {}", dir.display()))?;
    let outputs = tools::output::output_root().ok();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(reason) = read_directory_rejection(
        &dir,
        project,
        None,
        outputs.as_deref(),
        home.as_deref(),
        database,
    ) {
        return Err(if reason == "database" {
            "The draft directory also holds the candidate database, which agents must not read"
                .into()
        } else {
            format!("Draft directory overlaps the {reason}")
        });
    }
    Ok(dir)
}

/// Resolve existing ancestors too: SQLite sidecars may not exist yet, and their
/// parent may be a symlink. Never compare a lexical DB path to a canonical root.
fn resolve_existing(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut rest = Vec::new();
    let mut current = path.as_path();
    loop {
        if let Ok(resolved) = current.canonicalize() {
            return rest
                .iter()
                .rev()
                .fold(resolved, |resolved, part| resolved.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                current = parent;
            }
            _ => return path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_sidecars_and_missing_database_under_symlinked_parent_are_protected() {
        let base = tempfile::tempdir().unwrap();
        let drafts = base.path().join("drafts");
        let db_parent = base.path().join("db");
        std::fs::create_dir(&drafts).unwrap();
        std::fs::create_dir(&db_parent).unwrap();
        let drafts = drafts.canonicalize().unwrap();
        let db = db_parent.join("candidate.db");
        // The main DB is outside drafts, but its WAL is linked into them.
        std::os::unix::fs::symlink(drafts.join("wal"), db.with_file_name("candidate.db-wal"))
            .unwrap();
        std::fs::write(drafts.join("wal"), "private").unwrap();
        assert!(validate_thread_read_directory(&drafts, None, Some(&db)).is_err());
        let linked = base.path().join("linked");
        std::os::unix::fs::symlink(&drafts, &linked).unwrap();
        assert!(
            validate_thread_read_directory(&drafts, None, Some(&linked.join("not-yet-created.db")))
                .is_err()
        );
    }

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    // Given: declared directories around, inside and beside the protected paths
    // When: choosing what a run may mount
    // Then: only the unrelated ones remain, in declaration order
    #[test]
    fn protected_paths_their_parents_and_their_children_are_never_mounted() {
        let declared = paths(&[
            "/srv/reference",
            "/",
            "/home/u",
            "/home",
            "/home/u/.config/evorch/drafts",
            "/home/u/src",
            "/home/u/src/project",
            "/home/u/src/project/docs",
            "/var/tmp/evorch-scratch-1-x",
            "/var/tmp/evorch-scratch-1-x/t-thread/sub",
            "/var/tmp/evorch-output-1",
            "/var/tmp/evorch-output-1/run",
            "/var/tmp",
            "/home/u/src/project-sibling",
            "/opt/data",
        ]);
        let mounted = mountable_thread_read_dirs(
            &declared,
            Path::new("/home/u/src/project"),
            Some(Path::new("/var/tmp/evorch-scratch-1-x/t-thread")),
            Some(Path::new("/var/tmp/evorch-output-1")),
            Some(Path::new("/home/u")),
            None,
        );
        assert_eq!(
            mounted,
            paths(&[
                "/srv/reference",
                "/home/u/.config/evorch/drafts",
                "/home/u/src/project-sibling",
                "/opt/data",
            ])
        );
    }

    // Given: no scratch, output root or home / When: filtering / Then: only the project
    // root and the filesystem root still exclude directories
    #[test]
    fn absent_protected_paths_are_not_checked() {
        let declared = paths(&["/", "/var/tmp", "/home/u", "/project/sub", "/srv/a"]);
        assert_eq!(
            mountable_thread_read_dirs(&declared, Path::new("/project"), None, None, None, None),
            paths(&["/var/tmp", "/home/u", "/srv/a"])
        );
    }

    // Given: a protected path reached through a symbolic link / When: filtering
    // Then: it is compared by its real location
    #[test]
    fn protected_paths_are_compared_by_their_real_location() {
        let base = tempfile::tempdir().unwrap();
        let real = base.path().canonicalize().unwrap();
        let project = real.join("project");
        std::fs::create_dir(&project).unwrap();
        let link = real.join("link");
        std::os::unix::fs::symlink(&project, &link).unwrap();
        let declared = vec![project.join("docs"), real.join("other")];
        assert_eq!(
            mountable_thread_read_dirs(&declared, &link, None, None, None, None),
            vec![real.join("other")]
        );
    }

    struct NeverModel;
    #[async_trait::async_trait]
    impl crate::AgentModel for NeverModel {
        fn selected_model(&self, _: crate::Role, _: Option<&str>) -> String {
            "test".into()
        }
        async fn complete(
            &self,
            _: &crate::AgentInvocationContext,
            _: crate::Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, crate::RuntimeError> {
            panic!("declarations never invoke a model")
        }
    }

    fn runtime() -> AgentRuntime {
        let bus = std::sync::Arc::new(event_bus::EventBus::new(16));
        let executor = std::sync::Arc::new(tools::ToolExecutor::new(bus.clone()));
        AgentRuntime::new(bus, executor, std::sync::Arc::new(NeverModel))
    }

    // Given: a directory, a link to it, a file, a missing path and a second directory
    // When: they are declared for a thread, and later nothing is
    // Then: only existing directories stay, once each by their real location and in
    //       order, other threads have none, and the empty declaration clears the thread
    #[test]
    fn declarations_keep_existing_directories_once_by_their_real_location() {
        let base = tempfile::tempdir().unwrap();
        let real = base.path().canonicalize().unwrap();
        let (first, second) = (real.join("first"), real.join("second"));
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::os::unix::fs::symlink(&first, real.join("link")).unwrap();
        std::fs::write(real.join("file"), "x").unwrap();
        let runtime = runtime();
        runtime.set_thread_read_only_directories(
            "thread-a",
            vec![
                real.join("link"),
                real.join("file"),
                real.join("missing"),
                second.clone(),
                first.clone(),
            ],
        );
        assert_eq!(
            runtime.thread_read_only_directories("thread-a"),
            vec![first.clone(), second]
        );
        assert!(runtime.thread_read_only_directories("thread-b").is_empty());
        runtime.set_thread_read_only_directories("thread-a", vec![real.join("missing")]);
        assert!(runtime.thread_read_only_directories("thread-a").is_empty());
        assert!(runtime.shared.thread_read_dirs.lock().unwrap().is_empty());
    }

    // Given: three declared directories / When: one is replaced by a link to `/` and
    //        another is deleted before a run builds its sandbox
    // Then: only the untouched one is mounted, so a swapped link never widens what runs read
    #[test]
    fn directories_swapped_or_removed_after_the_declaration_are_not_mounted() {
        let base = tempfile::tempdir().unwrap();
        let real = base.path().canonicalize().unwrap();
        let (kept, swapped, removed) = (real.join("kept"), real.join("swapped"), real.join("gone"));
        for dir in [&kept, &swapped, &removed] {
            std::fs::create_dir(dir).unwrap();
        }
        let runtime = runtime();
        runtime.set_thread_read_only_directories(
            "thread",
            vec![kept.clone(), swapped.clone(), removed.clone()],
        );
        std::fs::remove_dir(&swapped).unwrap();
        std::os::unix::fs::symlink("/", &swapped).unwrap();
        std::fs::remove_dir(&removed).unwrap();
        let project = Path::new("/nonexistent-project");
        assert_eq!(
            runtime.mountable_thread_read_only_directories("thread", project, None),
            vec![kept]
        );
    }

    // Given: a thread root with read-only directories / When: it escalates
    // Then: the child thread gets a copy and the source keeps its own
    #[test]
    fn an_escalation_copies_the_source_threads_directories_to_its_child() {
        let base = tempfile::tempdir().unwrap();
        let drafts = base.path().canonicalize().unwrap();
        let runtime = runtime();
        let source = runtime.reserve_run_id();
        runtime.bind_thread_root("source", source).unwrap();
        runtime.set_thread_read_only_directories("source", vec![drafts.clone()]);
        let target = runtime.reserve_run_id();
        runtime
            .handoff_thread_goal(source, target, None, |_| {})
            .unwrap();
        let child = event_bus::escalation_thread_id(&target.to_string());
        assert_eq!(runtime.thread_of_run(target).as_deref(), Some(&*child));
        assert_eq!(
            runtime.thread_read_only_directories(&child),
            vec![drafts.clone()]
        );
        assert_eq!(runtime.thread_read_only_directories("source"), vec![drafts]);
    }
}
