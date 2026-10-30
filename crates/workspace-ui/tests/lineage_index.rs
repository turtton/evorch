//! `LineageIndex` must answer exactly like the per-call chain walks it replaced.
//! The `oracle` module keeps those walks verbatim as the reference.

use std::collections::BTreeMap;

use workspace_ui::{
    ForkPoint, LineageIndex, LineageKind, ProjectId, ThreadId, ThreadLineage, ThreadRecord,
    ThreadRunPhase, normalize_versions, show_version,
};

mod oracle {
    use std::collections::{BTreeMap, BTreeSet};

    use workspace_ui::{ProjectId, ThreadId, ThreadRecord, ThreadRunPhase};

    fn find<'a>(threads: &'a [ThreadRecord], id: &ThreadId) -> Option<&'a ThreadRecord> {
        threads.iter().find(|thread| &thread.id == id)
    }

    pub fn version_root(threads: &[ThreadRecord], id: &ThreadId) -> ThreadId {
        let mut current = id.clone();
        let mut visited = BTreeSet::new();
        while visited.insert(current.clone()) {
            match find(threads, &current) {
                Some(thread) if thread.is_rewind() => match &thread.parent_thread_id {
                    Some(parent) if find(threads, parent).is_some() => current = parent.clone(),
                    _ => break,
                },
                _ => break,
            }
        }
        current
    }

    pub fn version_members<'a>(
        threads: &'a [ThreadRecord],
        id: &ThreadId,
    ) -> Vec<&'a ThreadRecord> {
        let root = version_root(threads, id);
        threads
            .iter()
            .filter(|thread| version_root(threads, &thread.id) == root)
            .collect()
    }

    pub fn visible_version(threads: &[ThreadRecord], id: &ThreadId) -> Option<ThreadId> {
        version_members(threads, id)
            .into_iter()
            .find(|thread| !thread.superseded)
            .map(|thread| thread.id.clone())
    }

    pub fn display_parent(threads: &[ThreadRecord], id: &ThreadId) -> Option<ThreadId> {
        let root = version_root(threads, id);
        let parent = find(threads, &root)?.parent_thread_id.as_ref()?;
        visible_version(threads, parent)
            .filter(|_| find(threads, parent).is_some())
            .or_else(|| Some(parent.clone()))
    }

    pub fn show_version(threads: &mut [ThreadRecord], target: &ThreadId) {
        let members: BTreeSet<ThreadId> = version_members(threads, target)
            .into_iter()
            .map(|thread| thread.id.clone())
            .collect();
        for thread in threads
            .iter_mut()
            .filter(|thread| members.contains(&thread.id))
        {
            thread.superseded = &thread.id != target;
        }
    }

    pub fn normalize_versions(threads: &mut [ThreadRecord]) {
        let roots: BTreeSet<ThreadId> = threads
            .iter()
            .map(|thread| version_root(threads, &thread.id))
            .collect();
        for root in roots {
            let members = version_members(threads, &root);
            let visible = members.iter().filter(|thread| !thread.superseded).count();
            if visible == 1 {
                continue;
            }
            let keep = members
                .iter()
                .rev()
                .find(|thread| !thread.superseded)
                .or_else(|| members.last())
                .map(|thread| thread.id.clone());
            if let Some(keep) = keep {
                show_version(threads, &keep);
            }
        }
    }

    /// Ordering of `ThreadRecord::partition_for_project` before the index.
    pub fn partition_for_project<'a>(
        threads: &'a [ThreadRecord],
        project: &ProjectId,
        phases: &BTreeMap<String, ThreadRunPhase>,
    ) -> (Vec<&'a ThreadRecord>, Vec<&'a ThreadRecord>) {
        let (mut archived, mut main): (Vec<_>, Vec<_>) = threads
            .iter()
            .filter(|thread| &thread.project_id == project && !thread.superseded)
            .partition(|thread| thread.archived);
        let newest_first =
            |activity: &BTreeMap<&ThreadId, i64>, left: &ThreadRecord, right: &ThreadRecord| {
                activity[&right.id]
                    .cmp(&activity[&left.id])
                    .then_with(|| left.title.cmp(&right.title))
                    .then_with(|| left.id.cmp(&right.id))
            };
        let activity = subtree_activity(&main, threads);
        let running = subtree_running(&main, threads, phases);
        let pinned =
            |thread: &ThreadRecord| thread.pinned && display_parent(threads, &thread.id).is_none();
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
            .filter_map(|thread| Some((&thread.id, display_parent(all, &thread.id)?)))
            .collect();
        for thread in section {
            let at = thread.last_activity();
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
            let mut current = if thread.superseded {
                visible_version(all, &thread.id)
            } else {
                Some(thread.id.clone())
            };
            while let Some(id) = current
                && let Some(record) = shown.get(&id)
                && running.insert(&record.id)
            {
                current = display_parent(all, &id);
            }
        }
        running
    }
}

fn thread(id: &str) -> ThreadRecord {
    ThreadRecord::new(ThreadId::new(id), ProjectId::new("project"), id)
}

fn rewind_of(id: &str, parent: &str) -> ThreadRecord {
    let mut record = thread(id);
    record.parent_thread_id = Some(ThreadId::new(parent));
    record.lineage = Some(ThreadLineage {
        kind: LineageKind::Rewind,
        point: Some(ForkPoint {
            run_id: "run-1".into(),
            context_len: 2,
        }),
    });
    record
}

fn fork_of(id: &str, parent: &str) -> ThreadRecord {
    let mut record = thread(id);
    record.parent_thread_id = Some(ThreadId::new(parent));
    record.lineage = Some(ThreadLineage {
        kind: LineageKind::Fork,
        point: None,
    });
    record
}

fn tid(id: &str) -> ThreadId {
    ThreadId::new(id)
}

fn ids(records: &[&ThreadRecord]) -> Vec<String> {
    records.iter().map(|thread| thread.id.to_string()).collect()
}

/// Every query of the index against the walks it replaced, for each thread and
/// for IDs that are not in the list.
fn assert_matches_oracle(threads: &[ThreadRecord]) {
    let index = LineageIndex::new(threads);
    let unknown = tid("not-in-list");
    let probes = threads
        .iter()
        .map(|thread| &thread.id)
        .chain(std::iter::once(&unknown));
    for id in probes {
        assert_eq!(
            index.version_root(id),
            &oracle::version_root(threads, id),
            "version_root of {id}"
        );
        assert_eq!(
            ids(&index.version_members(id).collect::<Vec<_>>()),
            ids(&oracle::version_members(threads, id)),
            "version_members of {id}"
        );
        assert_eq!(
            index.visible_version(id).cloned(),
            oracle::visible_version(threads, id),
            "visible_version of {id}"
        );
        assert_eq!(
            index.display_parent(id).cloned(),
            oracle::display_parent(threads, id),
            "display_parent of {id}"
        );
        // The free functions are thin wrappers over the same index.
        assert_eq!(
            workspace_ui::display_parent(threads, id),
            oracle::display_parent(threads, id)
        );
    }
    // Repair and restore act on the same groups.
    let mut expected = threads.to_vec();
    let mut actual = threads.to_vec();
    oracle::normalize_versions(&mut expected);
    normalize_versions(&mut actual);
    assert_eq!(actual, expected, "normalize_versions");
    for target in threads {
        let mut expected = threads.to_vec();
        let mut actual = threads.to_vec();
        oracle::show_version(&mut expected, &target.id);
        show_version(&mut actual, &target.id);
        assert_eq!(actual, expected, "show_version of {}", target.id);
    }
    // And the sidebar ordering.
    let phases = BTreeMap::from([
        ("run-a".to_string(), ThreadRunPhase::Running),
        ("run-b".to_string(), ThreadRunPhase::Pending),
        ("run-c".to_string(), ThreadRunPhase::Done),
    ]);
    for project in ["project", "other"] {
        let project = ProjectId::new(project);
        let (main, archived) = ThreadRecord::partition_for_project(threads, &project, &phases);
        let (expected_main, expected_archived) =
            oracle::partition_for_project(threads, &project, &phases);
        assert_eq!(ids(&main), ids(&expected_main), "main list of {project:?}");
        assert_eq!(
            ids(&archived),
            ids(&expected_archived),
            "archived list of {project:?}"
        );
    }
}

#[test]
fn rewind_chain_resolves_to_the_original_and_shows_the_visible_version() {
    // root <- v2 <- v3, a fork of the visible v3, and an unrelated conversation.
    let mut root = thread("root");
    root.superseded = true;
    let mut v2 = rewind_of("v2", "root");
    v2.superseded = true;
    let v3 = rewind_of("v3", "v2");
    let fork = fork_of("fork", "root");
    let other = thread("other");
    let threads = [root, v2, v3, fork, other];
    let index = LineageIndex::new(&threads);

    assert_eq!(index.version_root(&tid("v3")), &tid("root"));
    assert_eq!(index.version_root(&tid("other")), &tid("other"));
    assert_eq!(
        ids(&index.version_members(&tid("v2")).collect::<Vec<_>>()),
        ["root", "v2", "v3"]
    );
    assert_eq!(index.visible_version(&tid("root")), Some(&tid("v3")));
    assert_eq!(index.display_parent(&tid("v2")), None);
    assert_eq!(index.display_parent(&tid("fork")), Some(&tid("v3")));
    assert_eq!(
        ids(&index.children(&tid("root")).collect::<Vec<_>>()),
        ["v2", "fork"]
    );
    assert_matches_oracle(&threads);
}

#[test]
fn rewind_cycles_stop_at_the_first_repeated_thread_like_the_chain_walk() {
    // a <- b <- c <- a is a cycle; t hangs off it and s rewinds itself.
    let threads = [
        rewind_of("a", "c"),
        rewind_of("b", "a"),
        rewind_of("c", "b"),
        rewind_of("t", "b"),
        rewind_of("s", "s"),
    ];
    let index = LineageIndex::new(&threads);
    // A cycle member is its own root; a tail leads to the cycle's entry point.
    assert_eq!(index.version_root(&tid("a")), &tid("a"));
    assert_eq!(index.version_root(&tid("c")), &tid("c"));
    assert_eq!(index.version_root(&tid("t")), &tid("b"));
    assert_eq!(index.version_root(&tid("s")), &tid("s"));
    assert_matches_oracle(&threads);
}

#[test]
fn missing_parents_keep_orphans_as_roots_and_children() {
    let mut orphan_rewind = rewind_of("v2", "ghost");
    orphan_rewind.superseded = true;
    let orphan_fork = fork_of("lost-fork", "ghost");
    // The parent group's only version is hidden, so it is not shown for children.
    let mut hidden = thread("hidden");
    hidden.superseded = true;
    let under_hidden = fork_of("under-hidden", "hidden");
    let threads = [orphan_rewind, orphan_fork, hidden, under_hidden];
    let index = LineageIndex::new(&threads);
    // The rewind's parent does not exist, so the rewind itself is the root.
    assert_eq!(index.version_root(&tid("v2")), &tid("v2"));
    assert_eq!(index.display_parent(&tid("v2")), Some(&tid("ghost")));
    assert_eq!(index.display_parent(&tid("lost-fork")), Some(&tid("ghost")));
    assert_eq!(index.visible_version(&tid("hidden")), None);
    assert_eq!(
        index.display_parent(&tid("under-hidden")),
        Some(&tid("hidden"))
    );
    assert_eq!(index.get(&tid("ghost")), None);
    assert_matches_oracle(&threads);
}

#[test]
fn archive_mix_and_several_roots_partition_like_before() {
    let mut a = thread("a");
    a.archived = true;
    a.updated_at = 10;
    let mut a2 = rewind_of("a2", "a");
    a2.archived = true;
    a2.updated_at = 20;
    a.superseded = true;
    let mut a_fork = fork_of("a-fork", "a");
    a_fork.archived = true;
    a_fork.updated_at = 30;
    let mut b = thread("b");
    b.pinned = true;
    b.updated_at = 5;
    let mut b_child = fork_of("b-child", "b");
    b_child.run_ids = vec!["run-a".into()];
    let mut c = thread("c");
    c.updated_at = 50;
    // Archived parent with a still-active child, and a thread of another project.
    let mut c_child = fork_of("c-child", "c");
    c_child.archived = true;
    let mut elsewhere = thread("elsewhere");
    elsewhere.project_id = ProjectId::new("other");
    let threads = [a, a2, a_fork, b, b_child, c, c_child, elsewhere];
    assert_matches_oracle(&threads);
}

/// Deterministic xorshift, so a failure reproduces from its seed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % u64::try_from(bound).unwrap()).unwrap()
    }
}

#[test]
fn generated_lists_with_cycles_orphans_and_mixed_flags_match_the_chain_walk() {
    for seed in 1..=300u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let count = 1 + rng.below(14);
        let threads: Vec<ThreadRecord> = (0..count)
            .map(|index| {
                let mut record = thread(&format!("t{index}"));
                record.parent_thread_id = match rng.below(5) {
                    0 | 1 => None,
                    2 => Some(tid("ghost")),
                    // Any thread, including later ones and itself: cycles happen.
                    _ => Some(tid(&format!("t{}", rng.below(count)))),
                };
                if record.parent_thread_id.is_some() {
                    record.lineage = match rng.below(3) {
                        0 => None,
                        1 => fork_of("x", "x").lineage,
                        _ => rewind_of("x", "x").lineage,
                    };
                }
                record.superseded = rng.below(3) == 0;
                record.archived = rng.below(4) == 0;
                record.pinned = rng.below(4) == 0;
                record.updated_at = i64::try_from(rng.below(6)).unwrap();
                record.title = format!("title{}", rng.below(3));
                record.project_id = ProjectId::new(if rng.below(6) == 0 {
                    "other"
                } else {
                    "project"
                });
                if rng.below(3) == 0 {
                    record.run_ids = vec![["run-a", "run-b", "run-c"][rng.below(3)].to_string()];
                }
                record
            })
            .collect();
        assert_matches_oracle(&threads);
    }
}

#[test]
fn deep_rewind_chain_and_many_threads_resolve_without_repeated_walks() {
    // Walking the chain per thread is quadratic here (4M steps, each cloning
    // an ID and allocating a set); the index must stay linear, so this simply
    // has to finish. Correctness is checked structurally.
    const DEPTH: usize = 4_000;
    const FORKS: usize = 4_000;
    let mut threads = vec![thread("v0")];
    for version in 1..DEPTH {
        let mut record = rewind_of(&format!("v{version}"), &format!("v{}", version - 1));
        record.superseded = version + 1 < DEPTH;
        threads.push(record);
    }
    threads[0].superseded = true;
    let newest = format!("v{}", DEPTH - 1);
    for fork in 0..FORKS {
        threads.push(fork_of(&format!("f{fork}"), "v0"));
    }
    let index = LineageIndex::new(&threads);

    assert_eq!(index.version_root(&tid(&newest)), &tid("v0"));
    assert_eq!(index.version_members(&tid("v17")).count(), DEPTH);
    assert_eq!(index.visible_version(&tid("v0")), Some(&tid(&newest)));
    assert_eq!(index.display_parent(&tid("f0")), Some(&tid(&newest)));
    assert_eq!(index.display_parent(&tid(&newest)), None);
    assert_eq!(index.children(&tid("v0")).count(), FORKS + 1);

    let project = ProjectId::new("project");
    let (main, archived) =
        ThreadRecord::partition_for_project(&threads, &project, &BTreeMap::new());
    assert_eq!(main.len(), FORKS + 1);
    assert!(archived.is_empty());

    normalize_versions(&mut threads);
    assert_eq!(
        threads.iter().filter(|thread| !thread.superseded).count(),
        FORKS + 1
    );
}
