//! What a run was told about its project's allowed directories (ADR 0032) and
//! its thread's read-only directories (ADR 0037).
//!
//! The list a run starts with is a fixed section of its system message. Later
//! changes are appended as `[workspace update]` messages at the next request
//! boundary, so the already-sent prefix and its prompt cache stay intact.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent_loop::LoopState;

const SECTION_PREFIX: &str = "Allowed directories (evorch): ";
const UPDATE_PREFIX: &str = "[workspace update]";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryAccess {
    ReadOnly,
    ReadWrite,
}

impl DirectoryAccess {
    const fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReadWrite => "read-write",
        }
    }
}

impl From<sandbox::PathAccess> for DirectoryAccess {
    fn from(access: sandbox::PathAccess) -> Self {
        match access {
            sandbox::PathAccess::ReadOnly => Self::ReadOnly,
            sandbox::PathAccess::ReadWrite => Self::ReadWrite,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToldDirectory {
    pub path: PathBuf,
    pub access: DirectoryAccess,
}

/// The allowed directories a run was last told, saved in its restore snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToldDirectories {
    /// Told by the system message or the latest appended update.
    #[serde(default)]
    pub last: Vec<ToldDirectory>,
    /// History was replaced (compaction, fork); the next boundary restates the whole list.
    #[serde(default)]
    pub rewrite_pending: bool,
    /// A non-empty list was told once, so a restatement must say even an empty list.
    #[serde(default)]
    pub mentioned: bool,
}

impl ToldDirectories {
    pub(crate) fn started(list: Vec<ToldDirectory>) -> Self {
        Self {
            mentioned: !list.is_empty(),
            last: list,
            rewrite_pending: false,
        }
    }

    /// Whether nothing was ever told, so snapshots can omit the record.
    pub fn is_unused(&self) -> bool {
        self.last.is_empty() && !self.rewrite_pending && !self.mentioned
    }

    /// The message that brings the run up to `current`, which then becomes the told list.
    pub(crate) fn update(&mut self, current: Vec<ToldDirectory>) -> Option<String> {
        let text = if std::mem::take(&mut self.rewrite_pending) {
            (self.mentioned || !current.is_empty()).then(|| restatement(&current))
        } else {
            changes(&self.last, &current)
        };
        self.mentioned |= !current.is_empty();
        self.last = current;
        text
    }
}

/// A project's live allowed directories with the access `policy` grants (ADR 0032).
pub(crate) fn regions(
    shared: &crate::runtime::Shared,
    root: &Path,
    policy: &crate::ExecutionPolicy,
) -> sandbox::AllowedRegions {
    sandbox::AllowedRegions {
        paths: shared.project_allowed_directories(root),
        access: policy.allowed_access(),
    }
}

/// The current list of `regions`, sorted by path so equal lists compare equal.
pub(crate) fn snapshot(regions: &sandbox::AllowedRegions) -> Vec<ToldDirectory> {
    let access = DirectoryAccess::from(regions.access);
    let mut list: Vec<_> = regions
        .paths
        .get()
        .iter()
        .map(|path| ToldDirectory {
            path: path.clone(),
            access,
        })
        .collect();
    list.sort_by(|a, b| a.path.cmp(&b.path));
    list.dedup_by(|a, b| a.path == b.path);
    list
}

/// What a run is told: the project's allowed directories and the thread's read-only
/// directories the run mounted (ADR 0037), sorted by path. Thread directories are
/// mounted after allowed ones, so an allowed directory equal to or inside one of
/// them is listed read-only, which is the access the boundary gives it.
pub(crate) fn told_list(
    regions: Option<&sandbox::AllowedRegions>,
    thread_read_dirs: &[PathBuf],
) -> Vec<ToldDirectory> {
    let mut list = regions.map_or_else(Vec::new, snapshot);
    for entry in &mut list {
        if thread_read_dirs
            .iter()
            .any(|dir| entry.path.starts_with(dir))
        {
            entry.access = DirectoryAccess::ReadOnly;
        }
    }
    list.extend(thread_read_dirs.iter().map(|path| ToldDirectory {
        path: path.clone(),
        access: DirectoryAccess::ReadOnly,
    }));
    list.sort_by(|a, b| a.path.cmp(&b.path));
    list.dedup_by(|a, b| a.path == b.path);
    list
}

/// The system message section for a run starting with `list`; none when empty.
pub(crate) fn section_text(list: &[ToldDirectory]) -> Option<String> {
    (!list.is_empty()).then(|| {
        format!(
            "{SECTION_PREFIX}besides the workspace, you may use these directories. Changes arrive later as `{UPDATE_PREFIX}` messages.\n{}",
            entries(list)
        )
    })
}

fn entries(list: &[ToldDirectory]) -> String {
    list.iter()
        .map(|entry| format!("* {} ({})", entry.path.display(), entry.access.label()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn restatement(list: &[ToldDirectory]) -> String {
    if list.is_empty() {
        format!("{UPDATE_PREFIX} Current allowed directories: none. This supersedes earlier lists.")
    } else {
        format!(
            "{UPDATE_PREFIX} Current allowed directories; this supersedes earlier lists.\n{}",
            entries(list)
        )
    }
}

fn changes(last: &[ToldDirectory], current: &[ToldDirectory]) -> Option<String> {
    let find = |list: &[ToldDirectory], path: &PathBuf| {
        list.iter()
            .find(|entry| &entry.path == path)
            .map(|entry| entry.access)
    };
    let mut lines = Vec::new();
    for entry in current {
        match find(last, &entry.path) {
            None => lines.push(format!(
                "+ {} ({})",
                entry.path.display(),
                entry.access.label()
            )),
            Some(before) if before != entry.access => lines.push(format!(
                "~ {} ({}, was {})",
                entry.path.display(),
                entry.access.label(),
                before.label()
            )),
            Some(_) => {}
        }
    }
    for entry in last {
        if find(current, &entry.path).is_none() {
            lines.push(format!("- {}", entry.path.display()));
        }
    }
    (!lines.is_empty()).then(|| {
        format!(
            "{UPDATE_PREFIX} Allowed directories changed (+ added, - removed, ~ access changed):\n{}",
            lines.join("\n")
        )
    })
}

impl LoopState {
    /// Called with the procedure context, so an interrupted request still
    /// retries its exact saved input first. Existing history is never edited.
    pub(crate) fn append_allowed_directories_context(&mut self) {
        if self.todo_context_deferred {
            return;
        }
        if self.allowed_directories.is_none() && self.thread_read_dirs.is_empty() {
            return;
        }
        let current = told_list(self.allowed_directories.as_ref(), &self.thread_read_dirs);
        if let Some(text) = self.told_directories.update(current) {
            self.context.push_user(&text);
            self.publish_message_count();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn told(path: &str, access: DirectoryAccess) -> ToldDirectory {
        ToldDirectory {
            path: PathBuf::from(path),
            access,
        }
    }

    use DirectoryAccess::{ReadOnly, ReadWrite};

    #[test]
    fn unchanged_list_appends_nothing() {
        // Given: a run told /a
        let mut state = ToldDirectories::started(vec![told("/a", ReadWrite)]);
        // When / Then: the same list produces no update
        assert_eq!(state.update(vec![told("/a", ReadWrite)]), None);
        assert_eq!(state.last, [told("/a", ReadWrite)]);
    }

    #[test]
    fn additions_removals_and_access_changes_are_one_update() {
        // Given: a run told /a and /c
        let mut state = ToldDirectories::started(vec![told("/a", ReadWrite), told("/c", ReadOnly)]);
        // When: /a goes away, /b arrives and /c becomes writable
        let text = state
            .update(vec![told("/b", ReadOnly), told("/c", ReadWrite)])
            .unwrap();
        // Then: one message lists every change, and the new list is the told one
        assert_eq!(
            text,
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed):\n+ /b (read-only)\n~ /c (read-write, was read-only)\n- /a"
        );
        assert_eq!(state.last, [told("/b", ReadOnly), told("/c", ReadWrite)]);
        assert_eq!(state.update(state.last.clone()), None);
    }

    #[test]
    fn a_saved_list_yields_only_the_difference_after_restore() {
        // Given: a snapshot saved after /a was told
        let saved: ToldDirectories = serde_json::from_value(serde_json::json!({
            "last": [{"path": "/a", "access": "read_only"}],
            "mentioned": true,
        }))
        .unwrap();
        let mut restored = saved.clone();
        // When / Then: the current list differs by /b only
        assert_eq!(
            restored
                .update(vec![told("/a", ReadOnly), told("/b", ReadOnly)])
                .unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed):\n+ /b (read-only)"
        );
        // An older snapshot without the record is a run that was told nothing.
        let mut legacy = ToldDirectories::default();
        assert!(
            legacy
                .update(vec![told("/a", ReadOnly)])
                .unwrap()
                .contains("+ /a")
        );
    }

    #[test]
    fn rewrite_restates_the_whole_list_once() {
        // Given: compaction replaced history of a run told /a
        let mut state = ToldDirectories::started(vec![told("/a", ReadOnly)]);
        state.rewrite_pending = true;
        // When / Then: the next boundary restates everything, then only changes follow
        assert_eq!(
            state.update(vec![told("/a", ReadOnly)]).unwrap(),
            "[workspace update] Current allowed directories; this supersedes earlier lists.\n* /a (read-only)"
        );
        assert_eq!(state.update(vec![told("/a", ReadOnly)]), None);
        // A run that once had directories is told explicitly that none remain.
        state.rewrite_pending = true;
        assert_eq!(
            state.update(Vec::new()).unwrap(),
            "[workspace update] Current allowed directories: none. This supersedes earlier lists."
        );
    }

    #[test]
    fn rewrite_stays_silent_for_runs_that_never_had_directories() {
        // Given: a run that never had allowed directories
        let mut state = ToldDirectories::started(Vec::new());
        state.rewrite_pending = true;
        // When / Then: compaction adds nothing to its history
        assert_eq!(state.update(Vec::new()), None);
        assert!(state.is_unused());
    }

    #[test]
    fn section_lists_paths_with_access_and_is_absent_when_empty() {
        assert_eq!(section_text(&[]), None);
        assert_eq!(
            section_text(&[told("/a", ReadWrite), told("/b", ReadOnly)]).unwrap(),
            "Allowed directories (evorch): besides the workspace, you may use these directories. Changes arrive later as `[workspace update]` messages.\n* /a (read-write)\n* /b (read-only)"
        );
    }

    #[test]
    fn snapshot_sorts_the_live_list_with_the_runs_access() {
        let regions = sandbox::AllowedRegions {
            paths: sandbox::LivePaths::new(vec!["/z".into(), "/a".into()]),
            access: sandbox::PathAccess::ReadOnly,
        };
        assert_eq!(
            snapshot(&regions),
            [told("/a", ReadOnly), told("/z", ReadOnly)]
        );
    }

    fn writable(paths: &[&str]) -> sandbox::AllowedRegions {
        sandbox::AllowedRegions {
            paths: sandbox::LivePaths::new(paths.iter().map(PathBuf::from).collect()),
            access: sandbox::PathAccess::ReadWrite,
        }
    }

    // Given: a Worker's writable allowed directories and its thread's read-only ones,
    //        overlapping on one path and covering another allowed directory
    // When: building the list the run is told
    // Then: thread directories are read-only, win where they cover an allowed directory,
    //       and the list is sorted with one entry per path
    #[test]
    fn thread_directories_are_read_only_and_win_where_they_cover_allowed_ones() {
        let regions = writable(&["/z", "/b/c", "/a", "/b"]);
        let thread = [
            PathBuf::from("/y"),
            PathBuf::from("/b"),
            PathBuf::from("/y"),
        ];
        assert_eq!(
            told_list(Some(&regions), &thread),
            [
                told("/a", ReadWrite),
                told("/b", ReadOnly),
                told("/b/c", ReadOnly),
                told("/y", ReadOnly),
                told("/z", ReadWrite),
            ]
        );
        assert_eq!(told_list(None, &thread[..1]), [told("/y", ReadOnly)]);
        assert_eq!(told_list(None, &[]), []);
        assert_eq!(told_list(Some(&regions), &[]), snapshot(&regions));
    }

    // Given: a run started with allowed and thread directories, saved and restored
    // When: the same declarations are read again, then history is compacted
    // Then: no update is appended, and the restatement keeps thread directories read-only
    #[test]
    fn a_restored_run_with_thread_directories_gets_no_false_update() {
        let regions = writable(&["/allowed", "/shared"]);
        let thread = [PathBuf::from("/shared"), PathBuf::from("/drafts")];
        let started = ToldDirectories::started(told_list(Some(&regions), &thread));
        let saved = serde_json::to_value(&started).unwrap();
        let mut restored: ToldDirectories = serde_json::from_value(saved).unwrap();
        assert_eq!(restored, started);
        assert_eq!(restored.update(told_list(Some(&regions), &thread)), None);
        restored.rewrite_pending = true;
        assert_eq!(
            restored.update(told_list(Some(&regions), &thread)).unwrap(),
            "[workspace update] Current allowed directories; this supersedes earlier lists.\n* /allowed (read-write)\n* /drafts (read-only)\n* /shared (read-only)"
        );
        // Restored again after a thread directory is withdrawn, the run is told only that.
        assert_eq!(
            restored
                .update(told_list(Some(&regions), &thread[..1]))
                .unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed):\n- /drafts"
        );
    }
}
