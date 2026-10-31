use std::collections::{BTreeMap, HashMap, HashSet};
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
        Self::partition_with_lineage(&crate::LineageIndex::new(threads), project, phases)
    }

    /// [`Self::partition_for_project`] over a lineage index the caller already
    /// built for the same thread list, so one frame resolves version groups once.
    pub fn partition_with_lineage<'a>(
        lineage: &crate::LineageIndex<'a>,
        project: &ProjectId,
        phases: &BTreeMap<String, ThreadRunPhase>,
    ) -> (Vec<&'a Self>, Vec<&'a Self>) {
        let threads = lineage.threads();
        let (mut archived, mut main): (Vec<_>, Vec<_>) = threads
            .iter()
            .filter(|thread| &thread.project_id == project && !thread.superseded)
            .partition(|thread| thread.archived);
        let newest_first = |activity: &HashMap<&ThreadId, i64>, left: &Self, right: &Self| {
            activity[&right.id]
                .cmp(&activity[&left.id])
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.id.cmp(&right.id))
        };
        let activity = subtree_activity(&main, lineage);
        let running = subtree_running(&main, lineage, phases);
        // Only a displayed root can be pinned; a child hangs under its parent
        // whatever a stale pin says.
        let pinned = |thread: &Self| thread.pinned && lineage.display_parent(&thread.id).is_none();
        main.sort_by(|left, right| {
            pinned(right)
                .cmp(&pinned(left))
                .then_with(|| running.contains(&right.id).cmp(&running.contains(&left.id)))
                .then_with(|| newest_first(&activity, left, right))
        });
        let activity = subtree_activity(&archived, lineage);
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

/// Latest activity in each displayed subtree of `section`: a thread's own
/// activity, raised by every other section thread that reaches it by following
/// displayed parents inside the section. A walk ends at a parent outside the
/// section and never counts a thread for itself, so the members of a parent
/// cycle all see the rest of the cycle.
///
/// Parent links form a functional graph, so one pass in leaf-to-root order
/// settles every thread (O(threads)); cycles are what remains once the leaves
/// are peeled off.
fn subtree_activity<'a>(
    section: &[&'a ThreadRecord],
    lineage: &crate::LineageIndex<'_>,
) -> HashMap<&'a ThreadId, i64> {
    let mut node_of: HashMap<&ThreadId, usize> = HashMap::with_capacity(section.len());
    let mut own = Vec::new();
    let mut offered = Vec::new();
    let mut parent_id: Vec<Option<&ThreadId>> = Vec::new();
    let mut ids = Vec::new();
    for thread in section {
        let at = thread.last_activity();
        match node_of.get(&thread.id) {
            // A repeated ID keeps one entry: the last record's own activity,
            // the highest activity it offers upward, the last parent link.
            Some(&node) => {
                own[node] = at;
                offered[node] = i64::max(offered[node], at);
                parent_id[node] = lineage.display_parent(&thread.id);
            }
            None => {
                node_of.insert(&thread.id, ids.len());
                ids.push(&thread.id);
                own.push(at);
                offered.push(at);
                parent_id.push(lineage.display_parent(&thread.id));
            }
        }
    }
    let parent: Vec<Option<usize>> = parent_id
        .iter()
        .map(|parent| node_of.get(parent.as_ref()?).copied())
        .collect();

    // What strictly reaches a node, accumulated leaf to root.
    let mut reached = vec![i64::MIN; ids.len()];
    let mut children = vec![0usize; ids.len()];
    for parent in parent.iter().flatten() {
        children[*parent] += 1;
    }
    let mut ready: Vec<usize> = (0..ids.len()).filter(|&node| children[node] == 0).collect();
    let mut settled = vec![false; ids.len()];
    while let Some(node) = ready.pop() {
        settled[node] = true;
        if let Some(parent) = parent[node] {
            reached[parent] = reached[parent].max(offered[node]).max(reached[node]);
            children[parent] -= 1;
            if children[parent] == 0 {
                ready.push(parent);
            }
        }
    }
    // Unsettled nodes lie on cycles (a self-parent is a cycle of one). Every
    // member sees all others, plus what hangs off each of them.
    for start in 0..ids.len() {
        if settled[start] {
            continue;
        }
        let mut cycle = vec![start];
        settled[start] = true;
        let mut next = parent[start];
        while let Some(node) = next.filter(|&node| !settled[node]) {
            settled[node] = true;
            cycle.push(node);
            next = parent[node];
        }
        let up: Vec<i64> = cycle
            .iter()
            .map(|&node| offered[node].max(reached[node]))
            .collect();
        let (mut best, mut second) = (i64::MIN, i64::MIN);
        for &value in &up {
            if value > best {
                second = best;
                best = value;
            } else {
                second = second.max(value);
            }
        }
        for (&node, &value) in cycle.iter().zip(&up) {
            let others = if value == best { second } else { best };
            reached[node] = reached[node].max(others);
        }
    }
    ids.into_iter()
        .zip(own.into_iter().zip(reached))
        .map(|(id, (own, reached))| (id, own.max(reached)))
        .collect()
}

/// Threads of `section` that show a pending or running run of their own, of a
/// hidden rewind version of theirs, or of a displayed descendant. Every thread
/// of the lineage counts, not only the visible ones, and raw run phases are used as
/// for archive safety, so a Stopped or Error root cannot hide a running
/// subagent and a hidden version's run still lifts the family it belongs to.
fn subtree_running<'a>(
    section: &[&'a ThreadRecord],
    lineage: &crate::LineageIndex<'_>,
    phases: &BTreeMap<String, ThreadRunPhase>,
) -> HashSet<&'a ThreadId> {
    let shown: HashMap<&ThreadId, &'a ThreadRecord> =
        section.iter().map(|thread| (&thread.id, *thread)).collect();
    let mut running = HashSet::new();
    for thread in lineage.threads().iter().filter(|thread| {
        thread.run_ids.iter().any(|run_id| {
            matches!(
                phases.get(run_id),
                Some(ThreadRunPhase::Pending | ThreadRunPhase::Running)
            )
        })
    }) {
        // A hidden rewind version is displayed through its group's visible version.
        let mut current = if thread.superseded {
            lineage.visible_version(&thread.id)
        } else {
            Some(&thread.id)
        };
        // Cyclic parent links stop at the first repeated ancestor.
        while let Some(id) = current
            && let Some(record) = shown.get(id)
            && running.insert(&record.id)
        {
            current = lineage.display_parent(id);
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
