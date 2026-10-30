use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ProjectId;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(String);

impl ThreadId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// `thread-` followed by 8 random lowercase Crockford base32 characters (40 bits),
    /// regenerated while `is_taken` reports a collision, so processes sharing one
    /// sidebar can create threads without coordinating.
    pub fn generate(is_taken: impl Fn(&Self) -> bool) -> Self {
        loop {
            let candidate = Self(format!("thread-{}", random_suffix()));
            if !is_taken(&candidate) {
                return candidate;
            }
        }
    }

    /// `{base}-{suffix}` with a fresh 8 character random suffix, for branches
    /// derived from `base`, regenerated while `is_taken` reports a collision.
    pub fn derived(base: &Self, tag: &str, is_taken: impl Fn(&Self) -> bool) -> Self {
        loop {
            let candidate = Self(format!("{}-{tag}-{}", base.0, random_suffix()));
            if !is_taken(&candidate) {
                return candidate;
            }
        }
    }
}

const CROCKFORD_LOWER: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const SUFFIX_LEN: usize = 8;

fn random_suffix() -> String {
    let mut bytes = [0_u8; 5];
    if getrandom::fill(&mut bytes).is_err() {
        // Without OS entropy fall back to clock and pid bits rather than a
        // repeating identifier; collisions are still checked by the caller.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let mixed = nanos ^ (u128::from(std::process::id()) << 40);
        bytes.copy_from_slice(&mixed.to_le_bytes()[..5]);
    }
    let bits = bytes
        .iter()
        .fold(0_u64, |acc, byte| (acc << 8) | u64::from(*byte));
    (0..SUFFIX_LEN)
        .rev()
        .map(|index| char::from(CROCKFORD_LOWER[((bits >> (index * 5)) & 0x1f) as usize]))
        .collect()
}

impl fmt::Display for ThreadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    Active,
    Stopped,
    Running,
    Waiting,
    Done,
    Error,
}

/// Runtime event phases mirrored without importing runtime-owned types.
/// `Stopped` is operator-stopped and resumable; `Waiting` awaits input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadRunPhase {
    /// Operator-stopped, with history and workspace retained for resumption.
    Stopped,
    Pending,
    Running,
    Waiting,
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreference {
    pub profile: String,
    pub model: Option<String>,
    /// Reasoning effort chosen with the explicit model; `None` uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadChatRole {
    Worker,
    Orchestrator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRecord {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    /// `title` is a placeholder the app generated, not one the user chose, so the
    /// first message may replace it. Cleared by any rename or applied auto-title.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub title_is_placeholder: bool,
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub created_at: i64,
    /// Latest conversation activity in UNIX seconds; older records fall back to `created_at`.
    #[serde(default)]
    pub updated_at: i64,
    pub run_ids: Vec<String>,
    /// Current conversation root; historical roots and children remain in `run_ids`.
    #[serde(default)]
    pub root_run_id: Option<String>,
    /// Inspected active root of the thread's runs.
    #[serde(default)]
    pub active_root: Option<PathBuf>,
    /// Extra directories this thread's runs (including subagents) may read, never write (ADR 0037).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_read_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub model_preference: Option<ModelPreference>,
    #[serde(default)]
    pub chat_role: Option<ThreadChatRole>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub draft_input: String,
    #[serde(default)]
    pub parent_thread_id: Option<ThreadId>,
    /// Worker run that requested this independent orchestrator conversation.
    #[serde(default)]
    pub escalation_source_run_id: Option<String>,
    /// How this conversation branched from `parent_thread_id`, if it did.
    #[serde(default)]
    pub lineage: Option<ThreadLineage>,
    /// A rewound version kept for restoration but hidden behind its visible version.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub superseded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineageKind {
    /// A separate conversation shown as a child thread.
    Fork,
    /// A replacement version of the parent conversation.
    Rewind,
}

/// A completed-turn boundary in the run that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForkPoint {
    pub run_id: String,
    /// Non-system message count published with the turn completion.
    pub context_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadLineage {
    pub kind: LineageKind,
    /// `None` branches before the first turn.
    #[serde(default)]
    pub point: Option<ForkPoint>,
}

impl ThreadRecord {
    pub fn new(id: ThreadId, project_id: ProjectId, title: impl Into<String>) -> Self {
        Self {
            id,
            project_id,
            title: title.into(),
            title_is_placeholder: false,
            pinned: false,
            archived: false,
            created_at: unix_seconds(std::time::SystemTime::now()),
            updated_at: 0,
            run_ids: Vec::new(),
            root_run_id: None,
            active_root: None,
            extra_read_dirs: Vec::new(),
            model_preference: None,
            chat_role: None,
            draft_input: String::new(),
            parent_thread_id: None,
            escalation_source_run_id: None,
            lineage: None,
            superseded: false,
        }
    }

    /// When the conversation last had activity, never earlier than its creation.
    pub fn last_activity(&self) -> i64 {
        self.updated_at.max(self.created_at)
    }

    /// Records activity at `at`; returns whether the stored time moved forward.
    pub fn touch(&mut self, at: std::time::SystemTime) -> bool {
        let at = unix_seconds(at);
        let changed = at > self.updated_at;
        self.updated_at = self.updated_at.max(at);
        changed
    }

    /// Both lists run newest first by the latest activity in each thread's
    /// displayed subtree, so a reply in a child thread lifts its whole family.
    /// The main list keeps pinned threads first, then families with a pending
    /// or running run anywhere in the displayed subtree, then the rest.
    pub fn partition_for_project<'a>(
        threads: &'a [Self],
        project: &ProjectId,
        phases: &BTreeMap<String, ThreadRunPhase>,
    ) -> (Vec<&'a Self>, Vec<&'a Self>) {
        let (mut archived, mut main): (Vec<_>, Vec<_>) = threads
            .iter()
            .filter(|thread| &thread.project_id == project && !thread.superseded)
            .partition(|thread| thread.archived);
        let newest_first = |activity: &BTreeMap<&ThreadId, i64>, left: &Self, right: &Self| {
            activity[&right.id]
                .cmp(&activity[&left.id])
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.id.cmp(&right.id))
        };
        let activity = subtree_activity(&main, threads);
        let running = subtree_running(&main, threads, phases);
        // Only a displayed root can be pinned; a child hangs under its parent
        // whatever a stale pin says.
        let pinned =
            |thread: &Self| thread.pinned && crate::display_parent(threads, &thread.id).is_none();
        main.sort_by(|left, right| {
            pinned(right)
                .cmp(&pinned(left))
                .then_with(|| running.contains(&right.id).cmp(&running.contains(&left.id)))
                .then_with(|| newest_first(&activity, left, right))
        });
        let activity = subtree_activity(&archived, threads);
        archived.sort_by(|left, right| newest_first(&activity, left, right));
        (main, archived)
    }

    pub fn is_rewind(&self) -> bool {
        self.lineage
            .as_ref()
            .is_some_and(|lineage| lineage.kind == LineageKind::Rewind)
    }

    pub fn state(&self, phases: &BTreeMap<String, ThreadRunPhase>) -> ThreadState {
        // A conversation's status belongs to its current root, not every run in
        // its history. Failed children and superseded roots remain browsable.
        if let Some(root) = &self.root_run_id {
            let state = match phases.get(root) {
                Some(ThreadRunPhase::Stopped) => ThreadState::Stopped,
                Some(ThreadRunPhase::Pending | ThreadRunPhase::Running) => ThreadState::Running,
                Some(ThreadRunPhase::Waiting) => ThreadState::Waiting,
                Some(ThreadRunPhase::Done) => ThreadState::Done,
                Some(ThreadRunPhase::Error) => ThreadState::Error,
                None => ThreadState::Active,
            };
            // Subagents keep working while the root waits on them or between turns.
            let subagent_running = self.run_ids.iter().any(|run_id| {
                run_id != root
                    && matches!(
                        phases.get(run_id),
                        Some(ThreadRunPhase::Pending | ThreadRunPhase::Running)
                    )
            });
            return match state {
                ThreadState::Waiting | ThreadState::Done | ThreadState::Active
                    if subagent_running =>
                {
                    ThreadState::Running
                }
                state => state,
            };
        }
        let phases = self.run_ids.iter().filter_map(|run_id| phases.get(run_id));
        let collected: Vec<&ThreadRunPhase> = phases.collect();
        if collected
            .iter()
            .any(|phase| matches!(phase, ThreadRunPhase::Stopped))
        {
            ThreadState::Stopped
        } else if collected
            .iter()
            .any(|phase| matches!(phase, ThreadRunPhase::Error))
        {
            ThreadState::Error
        } else if collected
            .iter()
            .any(|phase| matches!(phase, ThreadRunPhase::Pending | ThreadRunPhase::Running))
        {
            ThreadState::Running
        } else if collected
            .iter()
            .any(|phase| matches!(phase, ThreadRunPhase::Waiting))
        {
            ThreadState::Waiting
        } else if !self.run_ids.is_empty()
            && collected.len() == self.run_ids.len()
            && collected
                .iter()
                .all(|phase| matches!(phase, ThreadRunPhase::Done))
        {
            ThreadState::Done
        } else {
            ThreadState::Active
        }
    }
}

fn subtree_activity<'a>(
    section: &[&'a ThreadRecord],
    all: &[ThreadRecord],
) -> BTreeMap<&'a ThreadId, i64> {
    let mut activity: BTreeMap<&ThreadId, i64> = section
        .iter()
        .map(|thread| (&thread.id, thread.last_activity()))
        .collect();
    let parents: BTreeMap<&ThreadId, ThreadId> = section
        .iter()
        .filter_map(|thread| Some((&thread.id, crate::display_parent(all, &thread.id)?)))
        .collect();
    for thread in section {
        let at = thread.last_activity();
        // Cyclic parent links stop at the first repeated ancestor.
        let mut seen = BTreeSet::from([&thread.id]);
        let mut current = &thread.id;
        while let Some(parent) = parents.get(current)
            && let Some((&ancestor, &latest)) = activity.get_key_value(parent)
            && seen.insert(ancestor)
        {
            activity.insert(ancestor, latest.max(at));
            current = ancestor;
        }
    }
    activity
}

/// Threads of `section` that show a pending or running run of their own, of a
/// hidden rewind version of theirs, or of a displayed descendant. Every thread
/// of `all` counts, not only the visible ones, and raw run phases are used as
/// for archive safety, so a Stopped or Error root cannot hide a running
/// subagent and a hidden version's run still lifts the family it belongs to.
fn subtree_running<'a>(
    section: &[&'a ThreadRecord],
    all: &[ThreadRecord],
    phases: &BTreeMap<String, ThreadRunPhase>,
) -> BTreeSet<&'a ThreadId> {
    let shown: BTreeMap<&ThreadId, &'a ThreadRecord> =
        section.iter().map(|thread| (&thread.id, *thread)).collect();
    let mut running = BTreeSet::new();
    for thread in all.iter().filter(|thread| {
        thread.run_ids.iter().any(|run_id| {
            matches!(
                phases.get(run_id),
                Some(ThreadRunPhase::Pending | ThreadRunPhase::Running)
            )
        })
    }) {
        // A hidden rewind version is displayed through its group's visible version.
        let mut current = if thread.superseded {
            crate::visible_version(all, &thread.id)
        } else {
            Some(thread.id.clone())
        };
        // Cyclic parent links stop at the first repeated ancestor.
        while let Some(id) = current
            && let Some(record) = shown.get(&id)
            && running.insert(&record.id)
        {
            current = crate::display_parent(all, &id);
        }
    }
    running
}

fn unix_seconds(at: std::time::SystemTime) -> i64 {
    at.duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod stop_tests {
    use super::*;

    #[test]
    fn stopped_is_distinct_resumable_state_even_while_children_run() {
        let mut thread =
            ThreadRecord::new(ThreadId::new("thread"), ProjectId::new("project"), "Title");
        thread.run_ids = vec!["root".into(), "child".into()];
        let phases = BTreeMap::from([
            ("root".into(), ThreadRunPhase::Stopped),
            ("child".into(), ThreadRunPhase::Running),
        ]);
        assert_eq!(thread.state(&phases), ThreadState::Stopped);
        assert_eq!(
            serde_json::to_string(&ThreadRunPhase::Stopped).unwrap(),
            "\"stopped\""
        );
    }
}
