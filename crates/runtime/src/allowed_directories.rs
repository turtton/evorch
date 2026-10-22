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
const OWN_ACCESS: &str = "Access labels describe this run's own permissions.";

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

/// What a Worker delegated now gets on a listed directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegatedAccess {
    ReadOnly,
    ReadWrite,
    /// Not mounted for the Worker: a thread read-only directory this run mounted, outside
    /// every allowed directory and the Worker's usual boundary, whose declaration was
    /// withdrawn since.
    NoAccess,
}

impl From<DirectoryAccess> for DelegatedAccess {
    fn from(access: DirectoryAccess) -> Self {
        match access {
            DirectoryAccess::ReadOnly => Self::ReadOnly,
            DirectoryAccess::ReadWrite => Self::ReadWrite,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToldDirectory {
    pub path: PathBuf,
    pub access: DirectoryAccess,
    /// A Worker delegated now, only when its access differs from this run's access.
    /// Older snapshots did not describe delegation and default to no known difference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegated_access: Option<DelegatedAccess>,
}

impl ToldDirectory {
    fn delegated(&self) -> DelegatedAccess {
        self.delegated_access
            .unwrap_or_else(|| DelegatedAccess::from(self.access))
    }
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
    /// A Worker's differing access was told by the latest whole list or a later change,
    /// so a later message whose entries do not differ must say so rather than leave
    /// that sentence standing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delegation_mentioned: bool,
}

impl ToldDirectories {
    pub(crate) fn started(list: Vec<ToldDirectory>) -> Self {
        Self {
            mentioned: !list.is_empty(),
            delegation_mentioned: differs(&list),
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
        let told_before = self.delegation_mentioned;
        let (text, whole) = if std::mem::take(&mut self.rewrite_pending) {
            (
                (self.mentioned || !current.is_empty()).then(|| restatement(&current, told_before)),
                true,
            )
        } else if workers_change_unlabelled(&self.last, &current) {
            // A changes message would say "read-only, was read-only" or a bare removal.
            (Some(restatement(&current, told_before)), true)
        } else {
            (changes(&self.last, &current, told_before), false)
        };
        self.told(current, whole);
        text
    }

    /// Record `current` as told, as a `whole` list or as the changes to the previous one.
    /// A whole list supersedes earlier sentences about Workers, so a difference counts
    /// as told only if it has one. Changes leave earlier sentences standing for entries
    /// they do not label, and those entries kept their difference.
    fn told(&mut self, current: Vec<ToldDirectory>, whole: bool) {
        self.mentioned |= !current.is_empty();
        self.delegation_mentioned = differs(&current) || (!whole && self.delegation_mentioned);
        self.last = current;
    }

    /// Cancel a pending restatement that would only repeat the initial System.
    ///
    /// Compaction never summarizes the leading System and a fork keeps it, so when
    /// that System still carries exactly the section for `current` and the history
    /// holds no directory update, the run already knows `current` and nothing else.
    /// Raw messages are inspected: checkpoints keep them even after an update was
    /// summarized away, and a fork's raw history ends at its cut, so updates its
    /// saved told list describes beyond the cut do not count.
    pub(crate) fn settle_redundant_rewrite(
        &mut self,
        context: &crate::AgentContext,
        current: &[ToldDirectory],
    ) {
        if self.rewrite_pending && initial_section_suffices(context, current) {
            self.rewrite_pending = false;
            self.told(current.to_vec(), true);
        }
    }
}

/// Whether a Worker delegated now gets other access than told where no `+` or `~` line
/// labels it: on an entry this run keeps with its own access, or on a removed entry
/// inside another one that gives the Worker other access there, as when a directory
/// listed only for Workers is no longer declared.
fn workers_change_unlabelled(last: &[ToldDirectory], current: &[ToldDirectory]) -> bool {
    last.iter().any(
        |before| match current.iter().find(|entry| entry.path == before.path) {
            Some(entry) => {
                entry.access == before.access && entry.delegated_access != before.delegated_access
            }
            None => current
                .iter()
                .filter(|entry| before.path.starts_with(&entry.path))
                .max_by_key(|entry| entry.path.components().count())
                .is_some_and(|around| around.delegated() != before.delegated()),
        },
    )
}

/// Whether a Worker delegated now gets different access on some entry.
fn differs(list: &[ToldDirectory]) -> bool {
    list.iter().any(|entry| entry.delegated_access.is_some())
}

/// Whether the leading System has the exact section for `current` and no later
/// directory update could contradict it.
fn initial_section_suffices(context: &crate::AgentContext, current: &[ToldDirectory]) -> bool {
    fn texts(message: &providers::Message) -> impl Iterator<Item = &str> {
        message.content.iter().filter_map(|block| match block {
            providers::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
    }
    let Some(section) = section_text(current) else {
        return false;
    };
    // A checkpoint keeps System messages visible, so the raw first message is the
    // first one the model sees.
    let system_has_section = context.messages.first().is_some_and(|message| {
        message.role == providers::Role::System
            && texts(message).any(|text| text.split("\n\n").any(|part| part == section))
    });
    let restated = format!("{UPDATE_PREFIX} Current allowed directories");
    let changed = format!("{UPDATE_PREFIX} Allowed directories changed");
    let had_updates = context.messages.iter().any(|message| {
        message.role == providers::Role::User
            && texts(message).any(|text| text.starts_with(&restated) || text.starts_with(&changed))
    });
    system_has_section && !had_updates
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
            delegated_access: None,
        })
        .collect();
    list.sort_by(|a, b| a.path.cmp(&b.path));
    list.dedup_by(|a, b| a.path == b.path);
    list
}

/// Whether a run with `policy` delegates, so its list says what Workers would get.
pub(crate) fn can_delegate(policy: &crate::ExecutionPolicy) -> bool {
    policy.capabilities.can_delegate && policy.authorize("delegate").is_ok()
}

/// What a run is told: the project's allowed directories and the thread's read-only
/// directories the run mounted (ADR 0037), sorted by path. Thread directories are
/// mounted after allowed ones, so an allowed directory equal to or inside one of
/// them is listed read-only, which is the access the boundary gives it.
///
/// A run that can delegate is also told what a Worker delegated now would get. That
/// Worker mounts `delegated_thread_dirs`, the thread's current declaration, which can
/// differ from the run's own mounts fixed when it started. Outside those and the allowed
/// directories, its usual boundary `worker_base` decides, which still reads system paths
/// and caches. A declared directory inside a listed one, where the Worker gets other
/// access than around it, is listed with the run's own access there so the sentence can
/// name it.
pub(crate) fn told_list(
    regions: Option<&sandbox::AllowedRegions>,
    thread_read_dirs: &[PathBuf],
    policy: &crate::ExecutionPolicy,
    delegated_thread_dirs: &[PathBuf],
    worker_base: Option<&sandbox::SandboxBoundary>,
) -> Vec<ToldDirectory> {
    let covered = |dirs: &[PathBuf], path: &Path| dirs.iter().any(|dir| path.starts_with(dir));
    let by_path = |list: &mut Vec<ToldDirectory>| {
        list.sort_by(|a, b| a.path.cmp(&b.path));
        list.dedup_by(|a, b| a.path == b.path);
    };
    let allowed = regions.map_or_else(Vec::new, snapshot);
    let mut list = allowed.clone();
    for entry in &mut list {
        if covered(thread_read_dirs, &entry.path) {
            entry.access = DirectoryAccess::ReadOnly;
        }
    }
    list.extend(thread_read_dirs.iter().map(|path| ToldDirectory {
        path: path.clone(),
        access: DirectoryAccess::ReadOnly,
        delegated_access: None,
    }));
    by_path(&mut list);
    if can_delegate(policy) {
        let worker_access = DelegatedAccess::from(DirectoryAccess::from(
            crate::ExecutionPolicy::for_role(agents::Role::Worker).allowed_access(),
        ));
        let delegated = |path: &Path| {
            if covered(delegated_thread_dirs, path) {
                DelegatedAccess::ReadOnly
            } else if allowed.iter().any(|region| path.starts_with(&region.path)) {
                worker_access
            } else {
                worker_base
                    .and_then(|base| base.access(path))
                    .map_or(DelegatedAccess::NoAccess, |access| {
                        DirectoryAccess::from(access).into()
                    })
            }
        };
        let mut declared_since = delegated_thread_dirs.to_vec();
        declared_since.sort();
        for dir in declared_since {
            let Some(around) = list
                .iter()
                .filter(|entry| dir.starts_with(&entry.path))
                .max_by_key(|entry| entry.path.components().count())
            else {
                continue;
            };
            if around.path != dir && delegated(&around.path) != DelegatedAccess::ReadOnly {
                let access = around.access;
                list.push(ToldDirectory {
                    path: dir,
                    access,
                    delegated_access: None,
                });
            }
        }
        by_path(&mut list);
        for entry in &mut list {
            let delegated = delegated(&entry.path);
            entry.delegated_access = (delegated != entry.access.into()).then_some(delegated);
        }
    }
    list
}

/// The system message section for a run starting with `list`; none when empty.
pub(crate) fn section_text(list: &[ToldDirectory]) -> Option<String> {
    (!list.is_empty()).then(|| {
        format!(
            "{SECTION_PREFIX}besides the workspace, you may use these directories.{} Changes arrive later as `{UPDATE_PREFIX}` messages.\n{}",
            delegation_note(&list.iter().collect::<Vec<_>>(), Scope::Listed, false),
            entries(list)
        )
    })
}

/// The entries a delegation note speaks about.
#[derive(Clone, Copy)]
enum Scope {
    /// Every entry of a full list.
    Listed,
    /// The `+` and `~` lines of a change.
    Labelled,
}

impl Scope {
    const fn every(self) -> &'static str {
        match self {
            Self::Listed => "all of them",
            Self::Labelled => "every added or changed directory",
        }
    }

    const fn any(self) -> &'static str {
        match self {
            Self::Listed => "any of them",
            Self::Labelled => "any added or changed directory",
        }
    }
}

/// At most one sentence per message about the `labelled` entries, never one per entry.
///
/// Only when a Worker delegated now gets different access on some of them does it say
/// whose access the labels describe and what the Worker gets: the most permissive
/// differing access for every entry, naming the entries where the Worker gets another
/// (thread read-only ones, ADR 0037, or withdrawn ones it cannot access) as exceptions.
/// After such a sentence was told (`told_before`), entries without a difference are
/// said to have the same access, so the earlier sentence does not stand for them.
fn delegation_note(labelled: &[&ToldDirectory], scope: Scope, told_before: bool) -> String {
    use DelegatedAccess::{NoAccess, ReadOnly, ReadWrite};
    if labelled.is_empty() {
        return String::new();
    }
    let Some(primary) = [ReadWrite, ReadOnly, NoAccess].into_iter().find(|access| {
        labelled
            .iter()
            .any(|entry| entry.delegated_access == Some(*access))
    }) else {
        return if told_before {
            format!(
                " Delegated Workers have the same access to {}.",
                scope.every()
            )
        } else {
            String::new()
        };
    };
    let mut note = match primary {
        NoAccess => format!(
            " {OWN_ACCESS} Delegated Workers cannot access {}",
            scope.any()
        ),
        ReadOnly => format!(
            " {OWN_ACCESS} Delegated Workers have read-only access to {}",
            scope.every()
        ),
        ReadWrite => format!(
            " {OWN_ACCESS} Delegated Workers have read-write access to {}",
            scope.every()
        ),
    };
    let exceptions: Vec<String> = [ReadOnly, ReadWrite, NoAccess]
        .into_iter()
        .filter(|access| *access != primary)
        .filter_map(|access| exception(labelled, access))
        .collect();
    if !exceptions.is_empty() {
        note.push_str(" except ");
        note.push_str(&exceptions.join(", and "));
    }
    note.push('.');
    note
}

/// The clause naming the entries a Worker gets `access` on. An entry is folded into
/// the nearest entry around it when the Worker gets the same access there.
fn exception(labelled: &[&ToldDirectory], access: DelegatedAccess) -> Option<String> {
    let group: Vec<&ToldDirectory> = labelled
        .iter()
        .copied()
        .filter(|entry| entry.delegated() == access)
        .collect();
    let named: Vec<String> = group
        .iter()
        .filter(|entry| {
            labelled
                .iter()
                .filter(|parent| parent.path != entry.path && entry.path.starts_with(&parent.path))
                .max_by_key(|parent| parent.path.components().count())
                .is_none_or(|parent| parent.delegated() != access)
        })
        .map(|entry| format!("`{}`", entry.path.display()))
        .collect();
    let paths = named.join(", ");
    let verb = if named.len() == 1 { "stays" } else { "stay" };
    // "too" only where the Worker keeps this run's own access.
    let too = if group.iter().all(|entry| entry.delegated_access.is_none()) {
        " too"
    } else {
        ""
    };
    match access {
        _ if named.is_empty() => None,
        DelegatedAccess::NoAccess => Some(format!("{paths}, which they cannot access")),
        DelegatedAccess::ReadOnly => Some(format!("{paths}, which {verb} read-only for them{too}")),
        DelegatedAccess::ReadWrite => {
            Some(format!("{paths}, which {verb} read-write for them{too}"))
        }
    }
}

fn entries(list: &[ToldDirectory]) -> String {
    list.iter()
        .map(|entry| format!("* {} ({})", entry.path.display(), entry.access.label()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn restatement(list: &[ToldDirectory], told_before: bool) -> String {
    if list.is_empty() {
        format!("{UPDATE_PREFIX} Current allowed directories: none. This supersedes earlier lists.")
    } else {
        format!(
            "{UPDATE_PREFIX} Current allowed directories; this supersedes earlier lists.{}\n{}",
            delegation_note(&list.iter().collect::<Vec<_>>(), Scope::Listed, told_before),
            entries(list)
        )
    }
}

fn changes(last: &[ToldDirectory], current: &[ToldDirectory], told_before: bool) -> Option<String> {
    let find = |list: &[ToldDirectory], path: &PathBuf| {
        list.iter()
            .find(|entry| &entry.path == path)
            .map(|entry| entry.access)
    };
    let mut lines = Vec::new();
    let mut labelled = Vec::new();
    for entry in current {
        match find(last, &entry.path) {
            None => {
                lines.push(format!(
                    "+ {} ({})",
                    entry.path.display(),
                    entry.access.label()
                ));
                labelled.push(entry);
            }
            Some(before) if before != entry.access => {
                lines.push(format!(
                    "~ {} ({}, was {})",
                    entry.path.display(),
                    entry.access.label(),
                    before.label()
                ));
                labelled.push(entry);
            }
            Some(_) => {}
        }
    }
    for entry in last {
        if find(current, &entry.path).is_none() {
            lines.push(format!("- {}", entry.path.display()));
        }
    }
    // Removal lines carry no access label, so they need no explanation.
    let note = delegation_note(&labelled, Scope::Labelled, told_before);
    let heading_end = if note.is_empty() {
        ":".to_owned()
    } else {
        format!(".{note}")
    };
    (!lines.is_empty()).then(|| {
        format!(
            "{UPDATE_PREFIX} Allowed directories changed (+ added, - removed, ~ access changed){heading_end}\n{}",
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
        let current = self.directory_list();
        self.told_directories
            .settle_redundant_rewrite(&self.context, &current);
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
            delegated_access: None,
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

    fn orchestrator() -> crate::ExecutionPolicy {
        crate::ExecutionPolicy::for_role(agents::Role::Orchestrator)
    }

    /// The list while the thread still declares what the run mounted.
    fn listed(
        regions: Option<&sandbox::AllowedRegions>,
        thread_read_dirs: &[PathBuf],
        policy: &crate::ExecutionPolicy,
    ) -> Vec<ToldDirectory> {
        told_list(regions, thread_read_dirs, policy, thread_read_dirs, None)
    }

    // Given: a read-only Orchestrator whose list mixes plain allowed directories with
    //        thread read-only ones, including one nested inside a plain allowed one
    // When: the list is shown at start, restated, or announced as additions
    // Then: one sentence per message names only the exceptions, folded to their roots
    #[test]
    fn delegation_note_is_one_sentence_naming_only_thread_read_only_exceptions() {
        let regions = sandbox::AllowedRegions {
            access: sandbox::PathAccess::ReadOnly,
            ..writable(&[
                "/allowed",
                "/allowed/ro",
                "/allowed/ro/nested",
                "/other",
                "/allowed/ro",
            ])
        };
        let thread = [PathBuf::from("/allowed/ro"), PathBuf::from("/thread")];
        let list = listed(Some(&regions), &thread, &orchestrator());
        let different: Vec<_> = list
            .iter()
            .filter(|entry| entry.delegated_access.is_some())
            .map(|entry| entry.path.as_path())
            .collect();
        assert_eq!(different, [Path::new("/allowed"), Path::new("/other")]);
        let entries = "* /allowed (read-only)\n* /allowed/ro (read-only)\n* /allowed/ro/nested (read-only)\n* /other (read-only)\n* /thread (read-only)";
        assert_eq!(
            section_text(&list).unwrap(),
            format!(
                "Allowed directories (evorch): besides the workspace, you may use these directories. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them except `/allowed/ro`, `/thread`, which stay read-only for them too. Changes arrive later as `[workspace update]` messages.\n{entries}"
            )
        );
        assert_eq!(
            restatement(&list, false),
            format!(
                "[workspace update] Current allowed directories; this supersedes earlier lists. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them except `/allowed/ro`, `/thread`, which stay read-only for them too.\n{entries}"
            )
        );
        let added = changes(&[], &list, false).unwrap();
        assert!(added.starts_with("[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Access labels describe this run's own permissions. Delegated Workers have read-write access to every added or changed directory except `/allowed/ro`, `/thread`, which stay read-only for them too.\n+ /allowed (read-only)\n"), "{added}");
        for text in [
            section_text(&list).unwrap(),
            restatement(&list, false),
            added,
        ] {
            assert_eq!(text.matches("Delegated Workers").count(), 1);
            assert!(!text.contains("read-only for you"));
        }
        // A single exception reads in the singular.
        let single = listed(Some(&regions), &thread[1..], &orchestrator());
        assert!(
            section_text(&single)
                .unwrap()
                .contains("all of them except `/thread`, which stays read-only for them too.")
        );
    }

    // Given: a read-only Orchestrator whose Workers may write every listed directory
    // When / Then: no path is repeated in the explanation, each appears once as an entry
    #[test]
    fn a_uniform_delegation_difference_names_no_paths() {
        let regions = sandbox::AllowedRegions {
            access: sandbox::PathAccess::ReadOnly,
            ..writable(&["/a", "/b"])
        };
        let list = listed(Some(&regions), &[], &orchestrator());
        let section = section_text(&list).unwrap();
        assert_eq!(
            section,
            "Allowed directories (evorch): besides the workspace, you may use these directories. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them. Changes arrive later as `[workspace update]` messages.\n* /a (read-only)\n* /b (read-only)"
        );
        for text in [section, restatement(&list, false)] {
            assert_eq!(text.matches("/a").count(), 1, "{text}");
            assert_eq!(text.matches("/b").count(), 1, "{text}");
        }
        // Changes explain only the labelled lines: additions, not removals.
        let wider = listed(
            Some(&sandbox::AllowedRegions {
                access: sandbox::PathAccess::ReadOnly,
                ..writable(&["/b", "/c"])
            }),
            &[PathBuf::from("/t")],
            &orchestrator(),
        );
        assert_eq!(
            changes(&list, &wider, true).unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Access labels describe this run's own permissions. Delegated Workers have read-write access to every added or changed directory except `/t`, which stays read-only for them too.\n+ /c (read-only)\n+ /t (read-only)\n- /a"
        );
        // Entries a Worker gets this run's access on are said to be the same once
        // an earlier message said the Worker gets different access.
        let thread_only = listed(Some(&regions), &[PathBuf::from("/t")], &orchestrator());
        assert_eq!(
            changes(&list, &thread_only, true).unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Delegated Workers have the same access to every added or changed directory.\n+ /t (read-only)"
        );
        assert_eq!(
            changes(&list, &list[1..], true).unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed):\n- /a"
        );
    }

    // Given: runs whose delegated Workers get the same access as the run itself: a Worker,
    //        roles that cannot delegate, and an Orchestrator whose entries are all thread
    //        read-only directories or inside one (ADR 0037)
    // When: their list is shown at start, restated after compaction, and announced as additions
    // Then: no message says whose access the labels describe or mentions delegation
    #[test]
    fn runs_whose_workers_get_the_same_access_get_no_explanation_anywhere() {
        let read_only = sandbox::AllowedRegions {
            access: sandbox::PathAccess::ReadOnly,
            ..writable(&["/shared/a"])
        };
        let thread = [PathBuf::from("/shared"), PathBuf::from("/t")];
        let mut no_delegate = orchestrator();
        no_delegate.capabilities.can_delegate = false;
        let worker_list = listed(Some(&writable(&["/a"])), &thread[1..], &worker());
        for list in [
            worker_list.clone(),
            listed(
                Some(&read_only),
                &[],
                &crate::ExecutionPolicy::for_role(agents::Role::Explorer),
            ),
            listed(Some(&read_only), &[], &no_delegate),
            listed(Some(&read_only), &thread, &orchestrator()),
        ] {
            let mut compacted = ToldDirectories::started(list.clone());
            compacted.rewrite_pending = true;
            for text in [
                section_text(&list).unwrap(),
                compacted.update(list.clone()).unwrap(),
                ToldDirectories::default().update(list.clone()).unwrap(),
            ] {
                assert!(!text.contains("Access labels"), "{text}");
                assert!(!text.contains("Delegated Workers"), "{text}");
            }
        }
        // Their wording is the one used before delegation was described.
        assert_eq!(
            section_text(&worker_list).unwrap(),
            "Allowed directories (evorch): besides the workspace, you may use these directories. Changes arrive later as `[workspace update]` messages.\n* /a (read-write)\n* /t (read-only)"
        );
        let mut compacted = ToldDirectories::started(worker_list.clone());
        compacted.rewrite_pending = true;
        assert_eq!(
            compacted.update(worker_list.clone()).unwrap(),
            "[workspace update] Current allowed directories; this supersedes earlier lists.\n* /a (read-write)\n* /t (read-only)"
        );
        assert_eq!(
            ToldDirectories::default().update(worker_list).unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed):\n+ /a (read-write)\n+ /t (read-only)"
        );
    }

    // Given: runs whose delegated Workers would not get different access
    // When / Then: no delegation sentence is added
    #[test]
    fn runs_without_a_different_delegated_access_get_no_note() {
        let regions = sandbox::AllowedRegions {
            access: sandbox::PathAccess::ReadOnly,
            ..writable(&["/allowed", "/other"])
        };
        let thread = [PathBuf::from("/thread")];
        for list in [
            listed(Some(&regions), &[PathBuf::from("/")], &orchestrator()),
            listed(None, &thread, &orchestrator()),
            listed(
                Some(&regions),
                &[],
                &crate::ExecutionPolicy::for_role(agents::Role::Explorer),
            ),
            listed(Some(&writable(&["/allowed"])), &[], &worker()),
        ] {
            assert!(!section_text(&list).unwrap().contains("Delegated Workers"));
        }
        let mut no_delegate = orchestrator();
        no_delegate.capabilities.can_delegate = false;
        assert!(
            listed(Some(&regions), &[], &no_delegate)
                .iter()
                .all(|entry| entry.delegated_access.is_none())
        );
        no_delegate.capabilities.can_delegate = true;
        no_delegate.capabilities.allowed_tools.remove("delegate");
        assert!(
            listed(Some(&regions), &[], &no_delegate)
                .iter()
                .all(|entry| entry.delegated_access.is_none())
        );
    }

    // Given: a snapshot saved before delegation was described
    // When: it is restored by an Orchestrator, then a thread restricts the same path
    // Then: it loads, and each delegation-only change restates once without a `~` line
    #[test]
    fn legacy_snapshot_and_delegation_only_changes_restate_instead_of_false_access_diffs() {
        let mut state: ToldDirectories = serde_json::from_value(serde_json::json!({
            "last": [{"path": "/shared", "access": "read_only"}],
            "mentioned": true,
        }))
        .unwrap();
        assert_eq!(state.last[0].delegated_access, None);
        assert_eq!(state.update(state.last.clone()), None);
        let regions = sandbox::AllowedRegions {
            access: sandbox::PathAccess::ReadOnly,
            ..writable(&["/shared"])
        };
        let current = listed(Some(&regions), &[], &orchestrator());
        let text = state.update(current.clone()).unwrap();
        assert!(text.contains("Current allowed directories"));
        assert!(text.contains("Delegated Workers have read-write access to all of them."));
        assert!(!text.contains("~ /shared"));
        let saved = serde_json::to_string(&state).unwrap();
        let mut restored: ToldDirectories = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored.update(current), None);
        // A restored thread can newly restrict the very same path for the Worker,
        // which retracts the difference told before.
        let covered = listed(Some(&regions), &[PathBuf::from("/shared")], &orchestrator());
        let text = restored.update(covered.clone()).unwrap();
        assert_eq!(
            text,
            "[workspace update] Current allowed directories; this supersedes earlier lists. Delegated Workers have the same access to all of them.\n* /shared (read-only)"
        );
        assert_eq!(restored.update(covered), None);
    }

    // Given: a read-only Orchestrator that mounted thread directories at start, one of
    //        them inside an allowed directory, while the thread's declaration changes
    // When: listing what a Worker delegated now would get
    // Then: Workers follow the current declaration while the run keeps its own access
    #[test]
    fn delegated_access_follows_the_current_declaration_not_the_runs_mounts() {
        let regions = sandbox::AllowedRegions {
            access: sandbox::PathAccess::ReadOnly,
            ..writable(&["/allowed", "/docs"])
        };
        let mounted = [
            PathBuf::from("/allowed/inner"),
            PathBuf::from("/ref"),
            PathBuf::from("/shared"),
        ];
        let workers = |declared: &[&str]| {
            let declared: Vec<_> = declared.iter().map(PathBuf::from).collect();
            told_list(Some(&regions), &mounted, &orchestrator(), &declared, None)
                .into_iter()
                .map(|entry| {
                    assert_eq!(entry.access, ReadOnly, "{}", entry.path.display());
                    let delegated = entry.delegated();
                    (entry.path, delegated)
                })
                .collect::<Vec<_>>()
        };
        let as_mounted = [
            (PathBuf::from("/allowed"), DelegatedAccess::ReadWrite),
            (PathBuf::from("/allowed/inner"), DelegatedAccess::ReadOnly),
            (PathBuf::from("/docs"), DelegatedAccess::ReadWrite),
            (PathBuf::from("/ref"), DelegatedAccess::ReadOnly),
            (PathBuf::from("/shared"), DelegatedAccess::ReadOnly),
        ];
        assert_eq!(workers(&["/allowed/inner", "/ref", "/shared"]), as_mounted);
        // Newly declared: the allowed directory becomes read-only for Workers too.
        let mut declared = as_mounted.clone();
        declared[2].1 = DelegatedAccess::ReadOnly;
        assert_eq!(
            workers(&["/allowed/inner", "/ref", "/shared", "/docs"]),
            declared
        );
        // Withdrawn: a directory inside an allowed one is writable again, and one
        // outside them is not mounted for Workers at all.
        let mut withdrawn = as_mounted.clone();
        withdrawn[1].1 = DelegatedAccess::ReadWrite;
        withdrawn[3].1 = DelegatedAccess::NoAccess;
        assert_eq!(workers(&["/shared"]), withdrawn);
        let list = told_list(
            Some(&regions),
            &mounted,
            &orchestrator(),
            &[PathBuf::from("/shared")],
            None,
        );
        assert_eq!(
            section_text(&list).unwrap(),
            "Allowed directories (evorch): besides the workspace, you may use these directories. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them except `/shared`, which stays read-only for them too, and `/ref`, which they cannot access. Changes arrive later as `[workspace update]` messages.\n* /allowed (read-only)\n* /allowed/inner (read-only)\n* /docs (read-only)\n* /ref (read-only)\n* /shared (read-only)"
        );
        // Without a writable entry, the sentence starts from what Workers cannot use.
        let thread_only = told_list(None, &mounted[1..], &orchestrator(), &mounted[2..], None);
        assert_eq!(
            restatement(&thread_only, false),
            "[workspace update] Current allowed directories; this supersedes earlier lists. Access labels describe this run's own permissions. Delegated Workers cannot access any of them except `/shared`, which stays read-only for them too.\n* /ref (read-only)\n* /shared (read-only)"
        );
    }

    // Given: entries nested across different Worker access
    // When / Then: an exception folds only into the nearest entry around it with the same access
    #[test]
    fn exceptions_fold_only_into_the_nearest_entry_with_the_same_worker_access() {
        let regions = sandbox::AllowedRegions {
            access: sandbox::PathAccess::ReadOnly,
            ..writable(&["/x/y"])
        };
        let mounted = [PathBuf::from("/x"), PathBuf::from("/x/y/z")];
        let list = told_list(
            Some(&regions),
            &mounted,
            &orchestrator(),
            &mounted[1..],
            None,
        );
        assert_eq!(
            restatement(&list, false),
            "[workspace update] Current allowed directories; this supersedes earlier lists. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them except `/x/y/z`, which stays read-only for them too, and `/x`, which they cannot access.\n* /x (read-only)\n* /x/y (read-only)\n* /x/y/z (read-only)"
        );
    }

    // Given: a read-only Orchestrator, and directories declared read-only for the thread
    //        after it started: inside its allowed directory, inside a thread directory
    //        it mounted, and outside both
    // When: listing what a Worker delegated now would get
    // Then: only one where the Worker gets other access than around it joins the list,
    //       with the run's own access there, so the one sentence can name it
    #[test]
    fn a_directory_declared_after_start_inside_a_listed_one_is_named_for_workers() {
        let regions = sandbox::AllowedRegions {
            access: sandbox::PathAccess::ReadOnly,
            ..writable(&["/allowed"])
        };
        let mounted = [PathBuf::from("/ref")];
        let declared = [
            "/allowed/locked/deeper",
            "/elsewhere",
            "/ref",
            "/ref/inner",
            "/allowed/locked",
        ]
        .map(PathBuf::from);
        let list = told_list(Some(&regions), &mounted, &orchestrator(), &declared, None);
        assert_eq!(
            section_text(&list).unwrap(),
            "Allowed directories (evorch): besides the workspace, you may use these directories. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them except `/allowed/locked`, `/ref`, which stay read-only for them too. Changes arrive later as `[workspace update]` messages.\n* /allowed (read-only)\n* /allowed/locked (read-only)\n* /ref (read-only)"
        );
        let mut state = ToldDirectories::started(told_list(
            Some(&regions),
            &mounted,
            &orchestrator(),
            &mounted,
            None,
        ));
        assert_eq!(
            state.update(list).unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Delegated Workers have the same access to every added or changed directory.\n+ /allowed/locked (read-only)"
        );
        // Inside a mounted directory whose declaration was withdrawn, Workers see only it.
        let narrowed = told_list(
            Some(&regions),
            &mounted,
            &orchestrator(),
            &[PathBuf::from("/ref/inner")],
            None,
        );
        assert_eq!(
            restatement(&narrowed, true),
            "[workspace update] Current allowed directories; this supersedes earlier lists. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them except `/ref/inner`, which stays read-only for them too, and `/ref`, which they cannot access.\n* /allowed (read-only)\n* /ref (read-only)\n* /ref/inner (read-only)"
        );
        // A run that cannot delegate lists only its own mounts.
        let mut no_delegate = orchestrator();
        no_delegate.capabilities.can_delegate = false;
        assert_eq!(
            told_list(Some(&regions), &mounted, &no_delegate, &declared, None),
            [told("/allowed", ReadOnly), told("/ref", ReadOnly)]
        );
    }

    // Given: a read-only Orchestrator that mounted thread directories at start inside a
    //        system path, inside a Cargo cache and elsewhere, all withdrawn since
    // When: listing what a Worker delegated now would get
    // Then: its usual boundary still lets it read the first two, so only the last is
    //       one it cannot access
    #[test]
    fn a_withdrawn_directory_inside_the_workers_usual_boundary_stays_readable_for_them() {
        let cargo = tempfile::tempdir().unwrap();
        let cargo_home = cargo.path().canonicalize().unwrap();
        let crate_source = cargo_home.join("registry/src/index/a-crate");
        std::fs::create_dir_all(&crate_source).unwrap();
        let base =
            sandbox::SandboxBoundary::new(PathBuf::from("/project")).cargo_cache(&cargo_home);
        let mounted = [
            PathBuf::from("/usr/share"),
            crate_source.clone(),
            PathBuf::from("/ref"),
        ];
        let list = told_list(None, &mounted, &orchestrator(), &[], Some(&base));
        let worker = |path: &Path| {
            list.iter()
                .find(|entry| entry.path == path)
                .map(ToldDirectory::delegated)
        };
        assert_eq!(
            worker(Path::new("/usr/share")),
            Some(DelegatedAccess::ReadOnly)
        );
        assert_eq!(worker(&crate_source), Some(DelegatedAccess::ReadOnly));
        assert_eq!(worker(Path::new("/ref")), Some(DelegatedAccess::NoAccess));
        // Withdrawing only those the Worker reads anyway changes nothing it is told.
        let readable = told_list(None, &mounted[..2], &orchestrator(), &[], Some(&base));
        assert_eq!(
            readable,
            told_list(
                None,
                &mounted[..2],
                &orchestrator(),
                &mounted[..2],
                Some(&base)
            )
        );
        assert!(
            !section_text(&readable)
                .unwrap()
                .contains("Delegated Workers")
        );
    }

    // Given: an Orchestrator told that Workers can write its allowed directories, then
    //        told a directory declared inside one while running, which Workers only read
    // When: that declaration alone is withdrawn, and later a nested allowed directory goes
    // Then: the list is restated so the write access Workers regain is said rather than
    //       a bare removal; a removal that leaves Workers' access as told stays a removal
    #[test]
    fn withdrawing_a_directory_named_for_workers_restates_the_access_they_regain() {
        let allowed = |paths: &[&str]| sandbox::AllowedRegions {
            access: sandbox::PathAccess::ReadOnly,
            ..writable(paths)
        };
        let list = |paths: &[&str], declared: &[PathBuf]| {
            told_list(Some(&allowed(paths)), &[], &orchestrator(), declared, None)
        };
        let mut state = ToldDirectories::started(list(&["/a", "/a/sub"], &[]));
        assert_eq!(
            state
                .update(list(&["/a", "/a/sub"], &[PathBuf::from("/a/locked")]))
                .unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Delegated Workers have the same access to every added or changed directory.\n+ /a/locked (read-only)"
        );
        assert_eq!(
            state.update(list(&["/a", "/a/sub"], &[])).unwrap(),
            "[workspace update] Current allowed directories; this supersedes earlier lists. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them.\n* /a (read-only)\n* /a/sub (read-only)"
        );
        assert_eq!(
            state.update(list(&["/a"], &[])).unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed):\n- /a/sub"
        );
    }

    // Given: an Orchestrator told that Workers can write its allowed directory
    // When: the thread declares it read-only, a covered directory is added, history is
    //       compacted, the declaration is withdrawn, and covered directories are added
    // Then: the message where the difference ends retracts it once; a full list ends
    //       the need, while additions leave the earlier sentence standing for the rest
    #[test]
    fn a_difference_told_once_is_retracted_and_restored_by_later_messages() {
        let regions = |paths: &[&str]| sandbox::AllowedRegions {
            access: sandbox::PathAccess::ReadOnly,
            ..writable(paths)
        };
        let shared = [PathBuf::from("/shared")];
        let list = |paths: &[&str], declared: &[PathBuf]| {
            told_list(Some(&regions(paths)), &[], &orchestrator(), declared, None)
        };
        let mut state = ToldDirectories::started(list(&["/shared"], &[]));
        assert!(state.delegation_mentioned);
        assert_eq!(
            state.update(list(&["/shared"], &shared)).unwrap(),
            "[workspace update] Current allowed directories; this supersedes earlier lists. Delegated Workers have the same access to all of them.\n* /shared (read-only)"
        );
        // That full list superseded the earlier sentence, so nothing more is said.
        assert!(!state.delegation_mentioned);
        assert_eq!(
            state
                .update(list(&["/shared", "/shared/new"], &shared))
                .unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed):\n+ /shared/new (read-only)"
        );
        state.rewrite_pending = true;
        assert_eq!(
            state
                .update(list(&["/shared", "/shared/new"], &shared))
                .unwrap(),
            "[workspace update] Current allowed directories; this supersedes earlier lists.\n* /shared (read-only)\n* /shared/new (read-only)"
        );
        let text = state
            .update(list(&["/shared", "/shared/new"], &[]))
            .unwrap();
        assert!(
            text.starts_with("[workspace update] Current allowed directories; this supersedes earlier lists. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them.\n"),
            "{text}"
        );
        // Additions do not supersede it, so each one without a difference says so.
        let t = [PathBuf::from("/t")];
        let tu = [PathBuf::from("/t"), PathBuf::from("/u")];
        for (paths, declared, added) in [
            (&["/shared", "/shared/new", "/t"][..], &t[..], "/t"),
            (&["/shared", "/shared/new", "/t", "/u"][..], &tu[..], "/u"),
        ] {
            assert_eq!(
                state.update(list(paths, declared)).unwrap(),
                format!(
                    "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Delegated Workers have the same access to every added or changed directory.\n+ {added} (read-only)"
                )
            );
        }
        // A run never told a difference stays silent about Workers.
        let mut silent = ToldDirectories::started(list(&["/shared"], &shared));
        assert!(!silent.delegation_mentioned);
        assert!(
            !silent
                .update(list(&["/shared", "/shared/new"], &shared))
                .unwrap()
                .contains("Delegated Workers")
        );
        assert_eq!(
            serde_json::to_value(&silent).unwrap(),
            serde_json::json!({
                "last": [
                    {"path": "/shared", "access": "read_only"},
                    {"path": "/shared/new", "access": "read_only"},
                ],
                "rewrite_pending": false,
                "mentioned": true,
            })
        );
    }

    fn system_with(section: &str) -> crate::AgentContext {
        let mut context =
            crate::AgentContext::new(crate::RunId::new(1), agents::Role::Orchestrator);
        context.push_system(&format!("instructions\n\n{section}\n\nworkspace"));
        context.push_user("task");
        context
    }

    // Given: history replaced by compaction or a fork, with the run's initial System kept
    // When: the next boundary settles the pending restatement
    // Then: only an exact, uncontradicted System section for the current list cancels it
    #[test]
    fn a_pending_restatement_is_cancelled_only_by_an_exact_uncontradicted_system_section() {
        let list = vec![told("/shared", ReadOnly)];
        let context = system_with(&section_text(&list).unwrap());
        let mut state = ToldDirectories::started(list.clone());
        state.rewrite_pending = true;
        state.settle_redundant_rewrite(&context, &list);
        assert_eq!(state, ToldDirectories::started(list.clone()));
        assert_eq!(state.update(list.clone()), None);

        // A fork's saved told list can describe updates beyond its cut; System decides,
        // including whether a Worker's different access was ever told.
        let mut fork = ToldDirectories::started(vec![told("/later", ReadWrite)]);
        fork.rewrite_pending = true;
        fork.delegation_mentioned = true;
        fork.settle_redundant_rewrite(&context, &list);
        assert!(!fork.rewrite_pending);
        assert_eq!(fork, ToldDirectories::started(list.clone()));

        // Without a pending restatement, the told list is left for the usual difference.
        let mut idle = ToldDirectories::started(vec![told("/later", ReadWrite)]);
        idle.settle_redundant_rewrite(&context, &list);
        assert_eq!(idle.last, [told("/later", ReadWrite)]);

        // A changed or emptied list is restated.
        for current in [vec![told("/shared", ReadWrite)], Vec::new()] {
            let mut state = ToldDirectories::started(list.clone());
            state.rewrite_pending = true;
            state.settle_redundant_rewrite(&context, &current);
            assert!(state.rewrite_pending);
            assert!(
                state
                    .update(current)
                    .unwrap()
                    .contains("Current allowed directories")
            );
        }

        // A directory update in raw history counts even once a checkpoint hides it,
        // since its summary could contradict System.
        let mut updated = context.clone();
        updated.push_user("[workspace update] Allowed directories changed (+ added, - removed, ~ access changed):\n- /shared");
        updated.push_user("[workspace update] Current allowed directories; this supersedes earlier lists.\n* /shared (read-only)");
        updated.apply_checkpoint(crate::CompactionCheckpoint {
            id: "directory-updates".into(),
            summary: providers::Message {
                role: providers::Role::User,
                content: vec![providers::ContentBlock::Text {
                    text: "The shared directory was removed, then returned.".into(),
                }],
            },
            range: (2, 4),
        });
        assert!(!updated.visible_messages().iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, providers::ContentBlock::Text { text }
                    if text.starts_with(UPDATE_PREFIX))
            })
        }));
        // A missing System or one with older wording is no proof of the current section.
        let mut no_system = context.clone();
        no_system.messages.remove(0);
        let old_wording =
            system_with("Allowed directories (evorch): old wording\n* /shared (read-only)");
        for context in [updated, no_system, old_wording] {
            let mut state = ToldDirectories::started(list.clone());
            state.rewrite_pending = true;
            state.settle_redundant_rewrite(&context, &list);
            assert!(state.rewrite_pending);
        }
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

    fn worker() -> crate::ExecutionPolicy {
        crate::ExecutionPolicy::for_role(agents::Role::Worker)
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
            listed(Some(&regions), &thread, &worker()),
            [
                told("/a", ReadWrite),
                told("/b", ReadOnly),
                told("/b/c", ReadOnly),
                told("/y", ReadOnly),
                told("/z", ReadWrite),
            ]
        );
        assert_eq!(
            listed(None, &thread[..1], &worker()),
            [told("/y", ReadOnly)]
        );
        assert_eq!(listed(None, &[], &worker()), []);
        assert_eq!(listed(Some(&regions), &[], &worker()), snapshot(&regions));
    }

    // Given: a run started with allowed and thread directories, saved and restored
    // When: the same declarations are read again, then history is compacted
    // Then: no update is appended, and the restatement keeps thread directories read-only
    #[test]
    fn a_restored_run_with_thread_directories_gets_no_false_update() {
        let regions = writable(&["/allowed", "/shared"]);
        let thread = [PathBuf::from("/shared"), PathBuf::from("/drafts")];
        let started = ToldDirectories::started(listed(Some(&regions), &thread, &worker()));
        let saved = serde_json::to_value(&started).unwrap();
        let mut restored: ToldDirectories = serde_json::from_value(saved).unwrap();
        assert_eq!(restored, started);
        assert_eq!(
            restored.update(listed(Some(&regions), &thread, &worker())),
            None
        );
        restored.rewrite_pending = true;
        assert_eq!(
            restored
                .update(listed(Some(&regions), &thread, &worker()))
                .unwrap(),
            "[workspace update] Current allowed directories; this supersedes earlier lists.\n* /allowed (read-write)\n* /drafts (read-only)\n* /shared (read-only)"
        );
        // Restored again after a thread directory is withdrawn, the run is told only that.
        assert_eq!(
            restored
                .update(listed(Some(&regions), &thread[..1], &worker()))
                .unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed):\n- /drafts"
        );
    }
}
