//! Version groups: a rewind replaces its parent in place, so the sidebar shows
//! one visible version per group while every version stays restorable.

use std::collections::{BTreeSet, HashMap};

use crate::thread::{ForkPoint, ThreadId, ThreadRecord};

fn find<'a>(threads: &'a [ThreadRecord], id: &ThreadId) -> Option<&'a ThreadRecord> {
    threads.iter().find(|thread| &thread.id == id)
}

/// Version groups and parent links of one thread list, resolved once.
///
/// Walking a rewind chain per lookup costs O(depth × threads), and the sidebar
/// asks for every thread every frame. Building the index is O(threads) (hash
/// lookups, one pass over the rewind chains) and every query afterwards is
/// O(1), so a frame builds one index and shares it. The index borrows the
/// list and must be rebuilt after the list, a lineage or a `superseded` flag
/// changes.
///
/// Thread IDs are unique in a valid list; if one repeats, the first record
/// with that ID answers, as a linear search would.
pub struct LineageIndex<'a> {
    threads: &'a [ThreadRecord],
    /// Record index of each thread ID.
    position: HashMap<&'a ThreadId, usize>,
    /// Record index of the version root of each record.
    root: Vec<usize>,
    /// Record indices of each version group in creation order, indexed by the
    /// group's root record; empty for a record that is not a root.
    members: Vec<Vec<usize>>,
    /// The first record of each group that is not superseded, indexed by root.
    visible: Vec<Option<usize>>,
    /// Record indices by `parent_thread_id`, in list order.
    children: HashMap<&'a ThreadId, Vec<usize>>,
}

impl<'a> LineageIndex<'a> {
    pub fn new(threads: &'a [ThreadRecord]) -> Self {
        let mut position = HashMap::with_capacity(threads.len());
        for (index, thread) in threads.iter().enumerate() {
            position.entry(&thread.id).or_insert(index);
        }
        let root = Self::resolve_roots(threads, &position);
        let mut members = vec![Vec::new(); threads.len()];
        let mut children: HashMap<&ThreadId, Vec<usize>> = HashMap::new();
        for (index, thread) in threads.iter().enumerate() {
            members[root[index]].push(index);
            if let Some(parent) = &thread.parent_thread_id {
                children.entry(parent).or_default().push(index);
            }
        }
        let visible = members
            .iter()
            .map(|group| {
                group
                    .iter()
                    .copied()
                    .find(|&index| !threads[index].superseded)
            })
            .collect();
        Self {
            threads,
            position,
            root,
            members,
            visible,
            children,
        }
    }

    /// A rewind continues its parent when that parent exists; the walk to the
    /// root follows these links. A chain ending in a cycle stops at the first
    /// repeated record, so the root of a cycle member is the member itself and
    /// the root of a record leading into a cycle is the cycle's entry point.
    fn resolve_roots(threads: &[ThreadRecord], position: &HashMap<&ThreadId, usize>) -> Vec<usize> {
        const UNVISITED: u8 = 0;
        const ON_PATH: u8 = 1;
        const RESOLVED: u8 = 2;
        let continues = |index: usize| {
            let thread = &threads[index];
            if !thread.is_rewind() {
                return None;
            }
            position.get(thread.parent_thread_id.as_ref()?).copied()
        };
        let mut root = vec![usize::MAX; threads.len()];
        let mut state = vec![UNVISITED; threads.len()];
        let mut path = Vec::new();
        for start in 0..threads.len() {
            if state[start] != UNVISITED || position[&threads[start].id] != start {
                continue;
            }
            let mut current = start;
            let resolved = loop {
                match state[current] {
                    UNVISITED => {
                        state[current] = ON_PATH;
                        path.push(current);
                        match continues(current) {
                            Some(next) => current = next,
                            None => break current,
                        }
                    }
                    ON_PATH => {
                        let entry = path
                            .iter()
                            .position(|&visited| visited == current)
                            .expect("an ON_PATH record is on the current path");
                        for member in path.drain(entry..) {
                            root[member] = member;
                            state[member] = RESOLVED;
                        }
                        break current;
                    }
                    _ => break root[current],
                }
            };
            for visited in path.drain(..) {
                root[visited] = resolved;
                state[visited] = RESOLVED;
            }
        }
        // A repeated ID shares the first record's group.
        for index in 0..threads.len() {
            root[index] = root[position[&threads[index].id]];
        }
        root
    }

    /// The indexed list.
    pub fn threads(&self) -> &'a [ThreadRecord] {
        self.threads
    }

    pub fn get(&self, id: &ThreadId) -> Option<&'a ThreadRecord> {
        self.position.get(id).map(|&index| &self.threads[index])
    }

    /// The original conversation a chain of rewinds replaced; an unknown ID is
    /// its own root.
    pub fn version_root<'b>(&self, id: &'b ThreadId) -> &'b ThreadId
    where
        'a: 'b,
    {
        match self.position.get(id) {
            Some(&index) => &self.threads[self.root[index]].id,
            None => id,
        }
    }

    /// Every version sharing `id`'s group, in creation order.
    pub fn version_members(&self, id: &ThreadId) -> impl Iterator<Item = &'a ThreadRecord> + '_ {
        self.group(id).iter().map(|&index| &self.threads[index])
    }

    fn group(&self, id: &ThreadId) -> &[usize] {
        self.position
            .get(id)
            .map_or(&[][..], |&index| &self.members[self.root[index]])
    }

    /// The version currently shown for `id`'s group.
    pub fn visible_version(&self, id: &ThreadId) -> Option<&'a ThreadId> {
        let &index = self.position.get(id)?;
        let visible = self.visible[self.root[index]]?;
        Some(&self.threads[visible].id)
    }

    /// Sidebar parent: rewinds are hidden behind their group, and a parent
    /// group is represented by its visible version. A missing parent is still
    /// reported, so an orphan keeps child semantics.
    pub fn display_parent(&self, id: &ThreadId) -> Option<&'a ThreadId> {
        let &index = self.position.get(id)?;
        let parent = self.threads[self.root[index]].parent_thread_id.as_ref()?;
        Some(self.visible_version(parent).unwrap_or(parent))
    }

    /// Threads whose `parent_thread_id` is `id`, in list order.
    pub fn children(&self, id: &ThreadId) -> impl Iterator<Item = &'a ThreadRecord> + '_ {
        self.children
            .get(id)
            .into_iter()
            .flatten()
            .map(|&index| &self.threads[index])
    }

    /// Record indices of every version group, one slice per group.
    fn groups(&self) -> impl Iterator<Item = &[usize]> {
        self.members
            .iter()
            .filter(|group| !group.is_empty())
            .map(Vec::as_slice)
    }
}

/// The original conversation a chain of rewinds replaced.
///
/// Builds an index per call: for many lookups share one [`LineageIndex`].
pub fn version_root(threads: &[ThreadRecord], id: &ThreadId) -> ThreadId {
    LineageIndex::new(threads).version_root(id).clone()
}

/// The version currently shown for `id`'s group.
///
/// Builds an index per call: for many lookups share one [`LineageIndex`].
pub fn visible_version(threads: &[ThreadRecord], id: &ThreadId) -> Option<ThreadId> {
    LineageIndex::new(threads).visible_version(id).cloned()
}

/// Sidebar parent; see [`LineageIndex::display_parent`].
///
/// Builds an index per call: for many lookups share one [`LineageIndex`].
pub fn display_parent(threads: &[ThreadRecord], id: &ThreadId) -> Option<ThreadId> {
    LineageIndex::new(threads).display_parent(id).cloned()
}

fn rewound_to(thread: &ThreadRecord, point: Option<&ForkPoint>) -> bool {
    thread.is_rewind()
        && thread
            .lineage
            .as_ref()
            .and_then(|lineage| lineage.point.as_ref())
            == point
}

/// The versions of what follows `point` in `id`'s conversation: the conversation
/// that was first rewound there, then every rewind to `point` reached from it
/// (rewinds of rewinds included) in creation order. `id` is always included.
pub fn point_versions(
    threads: &[ThreadRecord],
    id: &ThreadId,
    point: Option<&ForkPoint>,
) -> Vec<ThreadId> {
    let mut anchor = id.clone();
    let mut visited = BTreeSet::new();
    while visited.insert(anchor.clone()) {
        match find(threads, &anchor) {
            Some(thread) if rewound_to(thread, point) => match &thread.parent_thread_id {
                Some(parent) if find(threads, parent).is_some() => anchor = parent.clone(),
                _ => break,
            },
            _ => break,
        }
    }
    let mut members = BTreeSet::from([anchor.clone()]);
    loop {
        let before = members.len();
        for thread in threads {
            if rewound_to(thread, point)
                && thread
                    .parent_thread_id
                    .as_ref()
                    .is_some_and(|parent| members.contains(parent))
            {
                members.insert(thread.id.clone());
            }
        }
        if members.len() == before {
            break;
        }
    }
    let mut versions = vec![anchor.clone()];
    versions.extend(
        threads
            .iter()
            .filter(|thread| thread.id != anchor && members.contains(&thread.id))
            .map(|thread| thread.id.clone()),
    );
    versions
}

/// Make `target` the only visible version of its group.
pub fn show_version(threads: &mut [ThreadRecord], target: &ThreadId) {
    let members = LineageIndex::new(threads).group(target).to_vec();
    set_visible(threads, &members, target);
}

fn set_visible(threads: &mut [ThreadRecord], members: &[usize], target: &ThreadId) {
    for &index in members {
        threads[index].superseded = &threads[index].id != target;
    }
}

/// Repair groups left with no or several visible versions; the newest wins.
pub fn normalize_versions(threads: &mut [ThreadRecord]) {
    // Groups depend only on lineage, never on the flags repaired here.
    let repairs: Vec<(ThreadId, Vec<usize>)> = {
        let index = LineageIndex::new(threads);
        index
            .groups()
            .filter_map(|group| {
                let visible = group
                    .iter()
                    .filter(|&&member| !threads[member].superseded)
                    .count();
                if visible == 1 {
                    return None;
                }
                let keep = group
                    .iter()
                    .rev()
                    .find(|&&member| !threads[member].superseded)
                    .or_else(|| group.last())?;
                Some((threads[*keep].id.clone(), group.to_vec()))
            })
            .collect()
    };
    for (keep, members) in repairs {
        set_visible(threads, &members, &keep);
    }
}
