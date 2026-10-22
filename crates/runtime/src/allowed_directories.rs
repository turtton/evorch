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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToldDirectory {
    pub path: PathBuf,
    pub access: DirectoryAccess,
    /// A delegable Worker's access, only when it differs from this run's access.
    /// Older snapshots did not describe delegation and default to no known difference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegated_access: Option<DirectoryAccess>,
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
        } else if current.iter().any(|entry| {
            self.last.iter().any(|before| {
                before.path == entry.path
                    && before.access == entry.access
                    && before.delegated_access != entry.delegated_access
            })
        }) {
            // A delegation-only change must not produce "read-only, was read-only".
            Some(restatement(&current))
        } else {
            changes(&self.last, &current)
        };
        self.mentioned |= !current.is_empty();
        self.last = current;
        text
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
            self.last = current.to_vec();
        }
    }
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

/// What a run is told: the project's allowed directories and the thread's read-only
/// directories the run mounted (ADR 0037), sorted by path. Thread directories are
/// mounted after allowed ones, so an allowed directory equal to or inside one of
/// them is listed read-only, which is the access the boundary gives it.
pub(crate) fn told_list(
    regions: Option<&sandbox::AllowedRegions>,
    thread_read_dirs: &[PathBuf],
    policy: &crate::ExecutionPolicy,
) -> Vec<ToldDirectory> {
    let mut list = regions.map_or_else(Vec::new, snapshot);
    let worker_access = DirectoryAccess::from(
        crate::ExecutionPolicy::for_role(agents::Role::Worker).allowed_access(),
    );
    let can_delegate = policy.capabilities.can_delegate && policy.authorize("delegate").is_ok();
    for entry in &mut list {
        if thread_read_dirs
            .iter()
            .any(|dir| entry.path.starts_with(dir))
        {
            entry.access = DirectoryAccess::ReadOnly;
        } else if can_delegate && entry.access != worker_access {
            entry.delegated_access = Some(worker_access);
        }
    }
    list.extend(thread_read_dirs.iter().map(|path| ToldDirectory {
        path: path.clone(),
        access: DirectoryAccess::ReadOnly,
        delegated_access: None,
    }));
    list.sort_by(|a, b| a.path.cmp(&b.path));
    list.dedup_by(|a, b| a.path == b.path);
    list
}

/// The system message section for a run starting with `list`; none when empty.
pub(crate) fn section_text(list: &[ToldDirectory]) -> Option<String> {
    (!list.is_empty()).then(|| {
        format!(
            "{SECTION_PREFIX}besides the workspace, you may use these directories. {OWN_ACCESS}{} Changes arrive later as `{UPDATE_PREFIX}` messages.\n{}",
            delegation_note(&list.iter().collect::<Vec<_>>(), "all of them"),
            entries(list)
        )
    })
}

/// At most one sentence per message about the labelled entries, never one per entry,
/// and only when delegated Workers' access differs from this run's. Entries that keep
/// this run's access for Workers (thread read-only ones, ADR 0037) are named as
/// exceptions, nested ones folded into their parent, so none is promised writable.
fn delegation_note(labelled: &[&ToldDirectory], scope: &str) -> String {
    let Some(delegated) = labelled.iter().find_map(|entry| entry.delegated_access) else {
        return String::new();
    };
    let mut kept: Vec<&ToldDirectory> = Vec::new();
    for entry in labelled
        .iter()
        .copied()
        .filter(|entry| entry.delegated_access.is_none())
    {
        if !kept
            .iter()
            .any(|parent| entry.path.starts_with(&parent.path))
        {
            kept.push(entry);
        }
    }
    let mut note = format!(
        " Delegated Workers have {} access to {scope}",
        delegated.label()
    );
    // A Worker keeps this run's access on these, and in a delegating run every such
    // entry is a thread read-only one, so the first label describes them all.
    if let Some(first) = kept.first() {
        let paths = kept
            .iter()
            .map(|entry| format!("`{}`", entry.path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        let verb = if kept.len() == 1 { "stays" } else { "stay" };
        note.push_str(&format!(
            " except {paths}, which {verb} {} for them too",
            first.access.label()
        ));
    }
    note.push('.');
    note
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
            "{UPDATE_PREFIX} Current allowed directories; this supersedes earlier lists. {OWN_ACCESS}{}\n{}",
            delegation_note(&list.iter().collect::<Vec<_>>(), "all of them"),
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
    let explanation = if labelled.is_empty() {
        String::new()
    } else {
        format!(
            " {OWN_ACCESS}{}",
            delegation_note(&labelled, "every added or changed directory")
        )
    };
    (!lines.is_empty()).then(|| {
        format!(
            "{UPDATE_PREFIX} Allowed directories changed (+ added, - removed, ~ access changed).{explanation}\n{}",
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
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Access labels describe this run's own permissions.\n+ /b (read-only)\n~ /c (read-write, was read-only)\n- /a"
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
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Access labels describe this run's own permissions.\n+ /b (read-only)"
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
            "[workspace update] Current allowed directories; this supersedes earlier lists. Access labels describe this run's own permissions.\n* /a (read-only)"
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
            "Allowed directories (evorch): besides the workspace, you may use these directories. Access labels describe this run's own permissions. Changes arrive later as `[workspace update]` messages.\n* /a (read-write)\n* /b (read-only)"
        );
    }

    // Given a self read-only list, its explanation must name whose access it is.
    #[test]
    fn read_only_section_identifies_the_runs_own_permissions() {
        let text = section_text(&[told("/a", ReadOnly)]).unwrap();
        assert!(text.contains("Access labels describe this run's own permissions."));
        assert!(!text.contains("Delegated Workers"));
    }

    fn orchestrator() -> crate::ExecutionPolicy {
        crate::ExecutionPolicy::for_role(agents::Role::Orchestrator)
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
        let list = told_list(Some(&regions), &thread, &orchestrator());
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
            restatement(&list),
            format!(
                "[workspace update] Current allowed directories; this supersedes earlier lists. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them except `/allowed/ro`, `/thread`, which stay read-only for them too.\n{entries}"
            )
        );
        let added = changes(&[], &list).unwrap();
        assert!(added.starts_with("[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Access labels describe this run's own permissions. Delegated Workers have read-write access to every added or changed directory except `/allowed/ro`, `/thread`, which stay read-only for them too.\n+ /allowed (read-only)\n"), "{added}");
        for text in [section_text(&list).unwrap(), restatement(&list), added] {
            assert_eq!(text.matches("Delegated Workers").count(), 1);
            assert!(!text.contains("read-only for you"));
        }
        // A single exception reads in the singular.
        let single = told_list(Some(&regions), &thread[1..], &orchestrator());
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
        let list = told_list(Some(&regions), &[], &orchestrator());
        let section = section_text(&list).unwrap();
        assert_eq!(
            section,
            "Allowed directories (evorch): besides the workspace, you may use these directories. Access labels describe this run's own permissions. Delegated Workers have read-write access to all of them. Changes arrive later as `[workspace update]` messages.\n* /a (read-only)\n* /b (read-only)"
        );
        for text in [section, restatement(&list)] {
            assert_eq!(text.matches("/a").count(), 1, "{text}");
            assert_eq!(text.matches("/b").count(), 1, "{text}");
        }
        // Changes explain only the labelled lines: additions, not removals.
        let wider = told_list(
            Some(&sandbox::AllowedRegions {
                access: sandbox::PathAccess::ReadOnly,
                ..writable(&["/b", "/c"])
            }),
            &[PathBuf::from("/t")],
            &orchestrator(),
        );
        assert_eq!(
            changes(&list, &wider).unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Access labels describe this run's own permissions. Delegated Workers have read-write access to every added or changed directory except `/t`, which stays read-only for them too.\n+ /c (read-only)\n+ /t (read-only)\n- /a"
        );
        let thread_only = told_list(Some(&regions), &[PathBuf::from("/t")], &orchestrator());
        assert_eq!(
            changes(&list, &thread_only).unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed). Access labels describe this run's own permissions.\n+ /t (read-only)"
        );
        assert_eq!(
            changes(&list, &list[1..]).unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed).\n- /a"
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
            told_list(Some(&regions), &[PathBuf::from("/")], &orchestrator()),
            told_list(None, &thread, &orchestrator()),
            told_list(
                Some(&regions),
                &[],
                &crate::ExecutionPolicy::for_role(agents::Role::Explorer),
            ),
            told_list(Some(&writable(&["/allowed"])), &[], &worker()),
        ] {
            assert!(!section_text(&list).unwrap().contains("Delegated Workers"));
        }
        let mut no_delegate = orchestrator();
        no_delegate.capabilities.can_delegate = false;
        assert!(
            told_list(Some(&regions), &[], &no_delegate)
                .iter()
                .all(|entry| entry.delegated_access.is_none())
        );
        no_delegate.capabilities.can_delegate = true;
        no_delegate.capabilities.allowed_tools.remove("delegate");
        assert!(
            told_list(Some(&regions), &[], &no_delegate)
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
        let current = told_list(Some(&regions), &[], &orchestrator());
        let text = state.update(current.clone()).unwrap();
        assert!(text.contains("Current allowed directories"));
        assert!(text.contains("Delegated Workers have read-write access to all of them."));
        assert!(!text.contains("~ /shared"));
        let saved = serde_json::to_string(&state).unwrap();
        let mut restored: ToldDirectories = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored.update(current), None);
        // A restored thread can newly restrict the very same path for the Worker.
        let covered = told_list(Some(&regions), &[PathBuf::from("/shared")], &orchestrator());
        let text = restored.update(covered.clone()).unwrap();
        assert!(text.contains("Current allowed directories"));
        assert!(!text.contains("Delegated Workers"));
        assert!(!text.contains("~ /shared"));
        assert_eq!(restored.update(covered), None);
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

        // A fork's saved told list can describe updates beyond its cut; System decides.
        let mut fork = ToldDirectories::started(vec![told("/later", ReadWrite)]);
        fork.rewrite_pending = true;
        fork.settle_redundant_rewrite(&context, &list);
        assert!(!fork.rewrite_pending);
        assert_eq!(fork.last, list);

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
        updated.push_user("[workspace update] Allowed directories changed (+ added, - removed, ~ access changed).\n- /shared");
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
            told_list(Some(&regions), &thread, &worker()),
            [
                told("/a", ReadWrite),
                told("/b", ReadOnly),
                told("/b/c", ReadOnly),
                told("/y", ReadOnly),
                told("/z", ReadWrite),
            ]
        );
        assert_eq!(
            told_list(None, &thread[..1], &worker()),
            [told("/y", ReadOnly)]
        );
        assert_eq!(told_list(None, &[], &worker()), []);
        assert_eq!(
            told_list(Some(&regions), &[], &worker()),
            snapshot(&regions)
        );
    }

    // Given: a run started with allowed and thread directories, saved and restored
    // When: the same declarations are read again, then history is compacted
    // Then: no update is appended, and the restatement keeps thread directories read-only
    #[test]
    fn a_restored_run_with_thread_directories_gets_no_false_update() {
        let regions = writable(&["/allowed", "/shared"]);
        let thread = [PathBuf::from("/shared"), PathBuf::from("/drafts")];
        let started = ToldDirectories::started(told_list(Some(&regions), &thread, &worker()));
        let saved = serde_json::to_value(&started).unwrap();
        let mut restored: ToldDirectories = serde_json::from_value(saved).unwrap();
        assert_eq!(restored, started);
        assert_eq!(
            restored.update(told_list(Some(&regions), &thread, &worker())),
            None
        );
        restored.rewrite_pending = true;
        assert_eq!(
            restored
                .update(told_list(Some(&regions), &thread, &worker()))
                .unwrap(),
            "[workspace update] Current allowed directories; this supersedes earlier lists. Access labels describe this run's own permissions.\n* /allowed (read-write)\n* /drafts (read-only)\n* /shared (read-only)"
        );
        // Restored again after a thread directory is withdrawn, the run is told only that.
        assert_eq!(
            restored
                .update(told_list(Some(&regions), &thread[..1], &worker()))
                .unwrap(),
            "[workspace update] Allowed directories changed (+ added, - removed, ~ access changed).\n- /drafts"
        );
    }
}
