//! The batch rules on the pure tree: progress arithmetic, structure edits, limits.

use super::*;

const PLAN: &str = "plan-test";

fn tree() -> PlanTree {
    PlanTree::empty(PLAN.to_owned())
}

fn id(number: u64) -> String {
    format!("{PLAN}:{number}")
}

fn add(client_ref: &str, parent: Option<TaskRef>, title: &str) -> PlanOperation {
    PlanOperation::Add {
        client_ref: client_ref.to_owned(),
        parent,
        position: None,
        title: title.to_owned(),
        description: None,
        weight: None,
        status: None,
    }
}

fn add_with(
    client_ref: &str,
    parent: Option<TaskRef>,
    weight: u64,
    status: PlanTaskStatus,
) -> PlanOperation {
    PlanOperation::Add {
        client_ref: client_ref.to_owned(),
        parent,
        position: None,
        title: client_ref.to_owned(),
        description: None,
        weight: Some(PlanWeight::new(weight).unwrap()),
        status: Some(status),
    }
}

fn by_ref(name: &str) -> Option<TaskRef> {
    Some(TaskRef::Ref(name.to_owned()))
}

fn by_id(number: u64) -> TaskRef {
    TaskRef::Id(id(number))
}

fn set_status(task: TaskRef, status: PlanTaskStatus) -> PlanOperation {
    PlanOperation::Update {
        task,
        title: None,
        description: None,
        result: None,
        blocker: None,
        weight: None,
        status: Some(status),
    }
}

fn run(tree: &PlanTree, operations: &[PlanOperation]) -> PlanTree {
    let (next, _) = tree.apply(operations, false).unwrap();
    next.finalize().unwrap();
    next
}

fn refusal(tree: &PlanTree, operations: &[PlanOperation]) -> PlanRejection {
    match tree.apply(operations, false) {
        Err(rejection) => rejection,
        Ok((next, _)) => next.finalize().expect_err("the batch must be refused"),
    }
}

fn progress(tree: &PlanTree) -> PlanProgress {
    tree.finalize().unwrap().progress
}

/// The order the tree lists its tasks in, by task number.
fn order(tree: &PlanTree) -> Vec<(u64, u32)> {
    tree.flatten()
        .0
        .iter()
        .map(|entry| {
            let number = entry.id.rsplit_once(':').unwrap().1.parse().unwrap();
            (number, entry.depth)
        })
        .collect()
}

#[test]
fn an_empty_plan_is_unplanned_and_not_complete() {
    let empty = tree();
    let done = empty.finalize().unwrap();
    assert_eq!(done.progress, PlanProgress::default());
    assert_eq!(done.progress.completion_ppm(), None);
    assert!(done.flat.is_empty());
}

#[test]
fn sibling_weights_decide_the_share_and_the_top_level_is_one_sibling_group() {
    // Given: a completed task of weight 3 and a pending one of weight 1.
    let plan = run(
        &tree(),
        &[
            add_with("a", None, 3, PlanTaskStatus::Completed),
            add_with("b", None, 1, PlanTaskStatus::Pending),
        ],
    );
    // Then: the aggregate is 75 % complete and 25 % pending, in parts per million.
    let progress = progress(&plan);
    assert_eq!(progress.completion_ppm(), Some(750_000));
    assert_eq!(progress.weighted_ppm.pending, 250_000);
    assert_eq!(progress.leaf_count, 2);
    assert_eq!(progress.leaf_counts.completed, 1);
}

#[test]
fn a_parents_progress_is_the_weighted_aggregate_of_its_children() {
    // Given: P (weight 1) with a completed child (weight 1) and a pending child (weight 3),
    // and a completed sibling Q (weight 1).
    let plan = run(
        &tree(),
        &[
            add_with("p", None, 1, PlanTaskStatus::Pending),
            add_with("c1", by_ref("p"), 1, PlanTaskStatus::Completed),
            add_with("c2", by_ref("p"), 3, PlanTaskStatus::Pending),
            add_with("q", None, 1, PlanTaskStatus::Completed),
        ],
    );
    // Then: P is 25 % complete, so the plan is (0.25 + 1) / 2.
    let flat = plan.flatten().0;
    assert_eq!(flat[0].share.completed, 250_000);
    assert_eq!(flat[0].share.pending, 750_000);
    assert_eq!(progress(&plan).completion_ppm(), Some(625_000));
    // And the parent holds no state of its own.
    assert_eq!(plan.nodes[&id(1)].status, None);
}

#[test]
fn interrupted_and_blocked_leaves_count_as_incomplete() {
    let plan = run(
        &tree(),
        &[
            add_with("a", None, 1, PlanTaskStatus::Completed),
            add_with("b", None, 1, PlanTaskStatus::Blocked),
        ],
    );
    let (interrupted, _) = plan
        .apply(&[set_status(by_id(2), PlanTaskStatus::Interrupted)], true)
        .unwrap();
    let progress = progress(&interrupted);
    assert_eq!(progress.completion_ppm(), Some(500_000));
    assert_eq!(progress.weighted_ppm.interrupted, 500_000);
    assert_eq!(progress.leaf_counts.interrupted, 1);
}

#[test]
fn the_model_cannot_set_interrupted_but_the_runtime_can() {
    let plan = run(&tree(), &[add("a", None, "task")]);
    let interrupting = [set_status(by_id(1), PlanTaskStatus::Interrupted)];
    assert_eq!(
        refusal(&plan, &interrupting),
        PlanRejection::InterruptedIsRuntimeOnly
    );
    assert_eq!(
        refusal(
            &tree(),
            &[add_with("a", None, 1, PlanTaskStatus::Interrupted)]
        ),
        PlanRejection::InterruptedIsRuntimeOnly
    );
    let (runtime, _) = plan.apply(&interrupting, true).unwrap();
    assert_eq!(
        runtime.nodes[&id(1)].status,
        Some(PlanTaskStatus::Interrupted)
    );
}

#[test]
fn the_runtimes_reconciliation_interrupts_only_leaves_in_progress() {
    let plan = run(
        &tree(),
        &[
            add_with("a", None, 1, PlanTaskStatus::InProgress),
            add_with("b", None, 1, PlanTaskStatus::Pending),
            add_with("c", by_ref("b"), 1, PlanTaskStatus::InProgress),
        ],
    );
    let operations = plan.interrupt_operations();
    assert_eq!(operations.len(), 2);
    let (next, applied) = plan.apply(&operations, true).unwrap();
    assert_eq!(applied.changes.len(), 2);
    assert_eq!(next.nodes[&id(1)].status, Some(PlanTaskStatus::Interrupted));
    assert_eq!(next.nodes[&id(3)].status, Some(PlanTaskStatus::Interrupted));
    assert!(next.interrupt_operations().is_empty());
}

#[test]
fn a_deep_tree_is_listed_in_pre_order_with_depths() {
    // Given: a(b(c), d), e.
    let plan = run(
        &tree(),
        &[
            add("a", None, "a"),
            add("b", by_ref("a"), "b"),
            add("c", by_ref("b"), "c"),
            add("d", by_ref("a"), "d"),
            add("e", None, "e"),
        ],
    );
    assert_eq!(order(&plan), [(1, 1), (2, 2), (3, 3), (4, 2), (5, 1)]);
    assert_eq!(progress(&plan).node_count, 5);
    assert_eq!(progress(&plan).leaf_count, 3);
}

#[test]
fn depth_eight_is_accepted_and_nine_refused_whole() {
    let chain = |depth: usize| -> Vec<PlanOperation> {
        (1..=depth)
            .map(|level| {
                let parent = (level > 1).then(|| TaskRef::Ref(format!("t{}", level - 1)));
                add(&format!("t{level}"), parent, "step")
            })
            .collect()
    };
    let plan = run(&tree(), &chain(PLAN_MAX_DEPTH));
    assert_eq!(order(&plan).last(), Some(&(8, 8)));
    assert_eq!(
        refusal(&tree(), &chain(PLAN_MAX_DEPTH + 1)),
        PlanRejection::TooDeep {
            actual: 9,
            max: PLAN_MAX_DEPTH
        }
    );
    // A move that would nest the plan too deeply is refused as well.
    let two = run(&plan, &[add("x", None, "x")]);
    assert_eq!(
        refusal(
            &two,
            &[PlanOperation::Move {
                task: by_id(1),
                parent: Some(by_id(9)),
                position: None
            }]
        ),
        PlanRejection::TooDeep {
            actual: 9,
            max: PLAN_MAX_DEPTH
        }
    );
}

#[test]
fn the_node_limit_is_exact() {
    let mut plan = tree();
    for batch in 0..PLAN_MAX_NODES / PLAN_MAX_OPERATIONS {
        let operations: Vec<_> = (0..PLAN_MAX_OPERATIONS)
            .map(|n| add(&format!("n{batch}-{n}"), None, "task"))
            .collect();
        plan = run(&plan, &operations);
    }
    assert_eq!(progress(&plan).node_count as usize, PLAN_MAX_NODES);
    assert_eq!(
        refusal(&plan, &[add("one-more", None, "task")]),
        PlanRejection::TooManyNodes {
            actual: PLAN_MAX_NODES + 1,
            max: PLAN_MAX_NODES
        }
    );
    // The refused batch changed nothing.
    assert_eq!(plan.nodes.len(), PLAN_MAX_NODES);
}

#[test]
fn the_operation_limit_is_exact() {
    let batch = |count: usize| -> Vec<PlanOperation> {
        (0..count)
            .map(|n| add(&format!("n{n}"), None, "task"))
            .collect()
    };
    assert_eq!(progress(&run(&tree(), &batch(32))).node_count, 32);
    assert_eq!(
        refusal(&tree(), &batch(33)),
        PlanRejection::TooManyOperations {
            actual: 33,
            max: 32
        }
    );
    assert_eq!(refusal(&tree(), &[]), PlanRejection::EmptyBatch);
}

#[test]
fn a_task_moves_among_siblings_and_between_parents() {
    // Given: a, b, c at the top and d below a.
    let plan = run(
        &tree(),
        &[
            add("a", None, "a"),
            add("b", None, "b"),
            add("c", None, "c"),
            add("d", by_ref("a"), "d"),
        ],
    );
    assert_eq!(order(&plan), [(1, 1), (4, 2), (2, 1), (3, 1)]);
    // When: c moves to the front, then b below c at position 0.
    let moved = run(
        &plan,
        &[
            PlanOperation::Move {
                task: by_id(3),
                parent: None,
                position: Some(0),
            },
            PlanOperation::Move {
                task: by_id(2),
                parent: Some(by_id(3)),
                position: Some(0),
            },
        ],
    );
    // Then: c is first and now a parent; b is its child.
    assert_eq!(order(&moved), [(3, 1), (2, 2), (1, 1), (4, 2)]);
    assert_eq!(moved.nodes[&id(3)].status, None);
    // And a move to the place the task already holds is no change.
    let (same, applied) = moved
        .apply(
            &[PlanOperation::Move {
                task: by_id(3),
                parent: None,
                position: Some(0),
            }],
            false,
        )
        .unwrap();
    assert!(applied.changes.is_empty());
    assert!(same.same_tasks(&moved));
}

#[test]
fn a_task_cannot_move_below_itself_or_beyond_its_siblings() {
    let plan = run(
        &tree(),
        &[
            add("a", None, "a"),
            add("b", by_ref("a"), "b"),
            add("c", by_ref("b"), "c"),
        ],
    );
    for target in [1, 2, 3] {
        assert_eq!(
            refusal(
                &plan,
                &[PlanOperation::Move {
                    task: by_id(1),
                    parent: Some(by_id(target)),
                    position: None
                }]
            ),
            PlanRejection::Cycle(id(1))
        );
    }
    assert_eq!(
        refusal(
            &plan,
            &[PlanOperation::Move {
                task: by_id(3),
                parent: None,
                position: Some(2)
            }]
        ),
        PlanRejection::PositionOutOfRange {
            position: 2,
            len: 1
        }
    );
}

#[test]
fn a_leaf_becomes_a_parent_and_a_parent_becomes_a_leaf_without_a_stale_state() {
    // Given: a completed leaf.
    let leaf = run(
        &tree(),
        &[add_with("a", None, 1, PlanTaskStatus::Completed)],
    );
    // When: it gets a child, its own state is gone and the child decides.
    let parent = run(&leaf, &[add("b", Some(TaskRef::Id(id(1))), "child")]);
    assert_eq!(parent.nodes[&id(1)].status, None);
    assert_eq!(progress(&parent).completion_ppm(), Some(0));
    // And a state cannot be set on the parent directly.
    assert_eq!(
        refusal(&parent, &[set_status(by_id(1), PlanTaskStatus::Completed)]),
        PlanRejection::ParentStatusNotSettable(id(1))
    );
    // When: the last child goes, the old `completed` does not come back: the task is a
    // leaf without a state, which the batch must settle.
    let remove = PlanOperation::Remove { task: by_id(2) };
    assert_eq!(
        refusal(&parent, std::slice::from_ref(&remove)),
        PlanRejection::LeafStatusMissing(id(1))
    );
    let settled = run(
        &parent,
        &[remove, set_status(by_id(1), PlanTaskStatus::Pending)],
    );
    assert_eq!(settled.nodes[&id(1)].status, Some(PlanTaskStatus::Pending));
    // The same holds when the last child moves away.
    let moved_away = [PlanOperation::Move {
        task: by_id(2),
        parent: None,
        position: None,
    }];
    assert_eq!(
        refusal(&parent, &moved_away),
        PlanRejection::LeafStatusMissing(id(1))
    );
}

#[test]
fn removing_a_subtree_takes_its_tasks_and_reports_them() {
    let plan = run(
        &tree(),
        &[
            add("a", None, "a"),
            add("b", by_ref("a"), "b"),
            add("c", by_ref("b"), "c"),
            add("d", None, "d"),
        ],
    );
    let (next, applied) = plan
        .apply(&[PlanOperation::Remove { task: by_id(1) }], false)
        .unwrap();
    assert_eq!(order(&next), [(4, 1)]);
    assert_eq!(
        applied.changes,
        [PlanChange::SubtreeRemoved {
            task_id: id(1),
            task_ids: vec![id(1), id(2), id(3)]
        }]
    );
}

#[test]
fn clear_works_alone_and_task_numbers_are_never_reused() {
    let plan = run(&tree(), &[add("a", None, "a"), add("b", None, "b")]);
    assert_eq!(
        refusal(
            &plan,
            &[
                PlanOperation::Clear,
                PlanOperation::Remove { task: by_id(1) }
            ]
        ),
        PlanRejection::ClearNotAlone
    );
    let cleared = run(&plan, &[PlanOperation::Clear]);
    assert!(cleared.nodes.is_empty());
    assert_eq!(progress(&cleared).completion_ppm(), None);
    // The next task takes a new number.
    let again = run(&cleared, &[add("c", None, "c")]);
    assert_eq!(order(&again), [(3, 1)]);
    // Clearing an empty plan changes nothing.
    let (nothing, applied) = cleared.apply(&[PlanOperation::Clear], false).unwrap();
    assert!(applied.changes.is_empty() && nothing.same_tasks(&cleared));
}

#[test]
fn client_refs_resolve_backwards_only_and_task_ids_must_belong_to_the_plan() {
    let plan = run(&tree(), &[add("a", None, "a")]);
    // A reference to a task a later operation adds.
    assert_eq!(
        refusal(
            &tree(),
            &[
                add("child", by_ref("parent"), "c"),
                add("parent", None, "p")
            ]
        ),
        PlanRejection::UnknownClientRef("parent".into())
    );
    assert_eq!(
        refusal(&tree(), &[add("x", None, "a"), add("x", None, "b")]),
        PlanRejection::DuplicateClientRef("x".into())
    );
    // A client_ref of an earlier batch is not known any more: only task IDs are.
    assert_eq!(
        refusal(&plan, &[add("b", by_ref("a"), "b")]),
        PlanRejection::UnknownClientRef("a".into())
    );
    assert_eq!(
        refusal(&plan, &[PlanOperation::Remove { task: by_id(99) }]),
        PlanRejection::UnknownTask(id(99))
    );
    assert_eq!(
        refusal(
            &plan,
            &[PlanOperation::Remove {
                task: TaskRef::Id("plan-other:1".into())
            }]
        ),
        PlanRejection::ForeignTask("plan-other:1".into())
    );
    assert_eq!(
        refusal(&tree(), &[add("", None, "a")]),
        PlanRejection::InvalidClientRef
    );
    assert_eq!(
        refusal(&tree(), &[add(&"r".repeat(65), None, "a")]),
        PlanRejection::InvalidClientRef
    );
}

#[test]
fn a_batch_is_applied_in_order_and_created_ids_are_reported_for_the_kept_tasks() {
    // Given: a batch that adds a task and removes it again.
    let (plan, applied) = tree()
        .apply(
            &[
                add("kept", None, "kept"),
                add("dropped", None, "dropped"),
                PlanOperation::Remove {
                    task: TaskRef::Ref("dropped".into()),
                },
            ],
            false,
        )
        .unwrap();
    // Then: only the kept task is reported, and the plan holds only it.
    assert_eq!(applied.created, [("kept".to_owned(), id(1))]);
    assert_eq!(order(&plan), [(1, 1)]);
}

#[test]
fn an_update_changes_named_fields_and_an_identical_value_is_no_change() {
    let plan = run(&tree(), &[add("a", None, "title")]);
    assert_eq!(
        refusal(
            &plan,
            &[PlanOperation::Update {
                task: by_id(1),
                title: None,
                description: None,
                result: None,
                blocker: None,
                weight: None,
                status: None
            }]
        ),
        PlanRejection::EmptyUpdate
    );
    let same = PlanOperation::Update {
        task: by_id(1),
        title: Some("title".into()),
        description: Some(String::new()),
        result: None,
        blocker: None,
        weight: Some(PlanWeight::DEFAULT),
        status: Some(PlanTaskStatus::Pending),
    };
    let (next, applied) = plan.apply(&[same], false).unwrap();
    assert!(applied.changes.is_empty());
    assert!(next.same_tasks(&plan));
    // A real change is reported with the changed fields only.
    let (changed, applied) = plan
        .apply(
            &[PlanOperation::Update {
                task: by_id(1),
                title: None,
                description: None,
                result: Some("done well".into()),
                blocker: None,
                weight: Some(PlanWeight::new(7).unwrap()),
                status: None,
            }],
            false,
        )
        .unwrap();
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(changed.nodes[&id(1)].result, "done well");
    assert_eq!(changed.nodes[&id(1)].weight.get(), 7);
}

#[test]
fn text_fields_are_bounded_and_titles_cannot_be_blank() {
    assert!(matches!(
        refusal(&tree(), &[add("a", None, "   ")]),
        PlanRejection::InvalidText { field: "title", .. }
    ));
    assert!(matches!(
        refusal(
            &tree(),
            &[add("a", None, &"t".repeat(PLAN_TITLE_MAX_BYTES + 1))]
        ),
        PlanRejection::InvalidText { field: "title", .. }
    ));
    run(
        &tree(),
        &[add("a", None, &"t".repeat(PLAN_TITLE_MAX_BYTES))],
    );
    let described = |bytes: usize| PlanOperation::Add {
        client_ref: "a".into(),
        parent: None,
        position: None,
        title: "t".into(),
        description: Some("d".repeat(bytes)),
        weight: None,
        status: None,
    };
    run(&tree(), &[described(PLAN_DESCRIPTION_MAX_BYTES)]);
    assert!(matches!(
        refusal(&tree(), &[described(PLAN_DESCRIPTION_MAX_BYTES + 1)]),
        PlanRejection::InvalidText {
            field: "description",
            ..
        }
    ));
}

#[test]
fn a_node_that_escapes_past_the_result_limit_is_refused_by_its_serialized_size() {
    // Given: a description within its text limit whose JSON form (each control byte takes
    // six bytes) is over the per-task limit of a tool result.
    let operation = |text: String| PlanOperation::Add {
        client_ref: "a".into(),
        parent: None,
        position: None,
        title: "t".into(),
        description: Some(text),
        weight: None,
        status: None,
    };
    // A quote takes two bytes: 4096 of them fit.
    run(
        &tree(),
        &[operation("\"".repeat(PLAN_DESCRIPTION_MAX_BYTES))],
    );
    // A control character takes six: 4096 of them do not.
    match refusal(
        &tree(),
        &[operation("\u{1}".repeat(PLAN_DESCRIPTION_MAX_BYTES))],
    ) {
        PlanRejection::NodeDetailTooLarge { actual, max, .. } => {
            assert!(actual > max);
            assert_eq!(max, PLAN_NODE_DETAIL_MAX_BYTES);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_whole_plan_is_bounded_in_serialized_bytes() {
    // Given: tasks of about 3 KiB each, added in batches until the plan is refused.
    let task = |n: usize| PlanOperation::Add {
        client_ref: format!("n{n}"),
        parent: None,
        position: None,
        title: "t".into(),
        description: Some("d".repeat(3000)),
        weight: None,
        status: None,
    };
    let mut plan = tree();
    let mut last_bytes = 0;
    let mut refused = None;
    for n in 0..PLAN_MAX_NODES {
        let candidate = [task(n)];
        match plan.apply(&candidate, false) {
            Ok((next, _)) => match next.finalize() {
                Ok(done) => {
                    last_bytes = done.plan_bytes;
                    plan = next;
                }
                Err(rejection) => {
                    refused = Some(rejection);
                    break;
                }
            },
            Err(rejection) => panic!("{rejection:?}"),
        }
    }
    // Then: the refusal is the plan size, and the plan before it was within the limit.
    match refused.expect("the plan limit is reached before the node limit") {
        PlanRejection::PlanTooLarge { actual, max } => {
            assert_eq!(max, PLAN_MAX_BYTES);
            assert!(actual > max);
        }
        other => panic!("{other:?}"),
    }
    assert!(last_bytes <= PLAN_MAX_BYTES);
}

#[test]
fn plan_ids_are_stable_per_thread_and_differ_between_threads() {
    let first = plan_id_for_thread("thread-a");
    assert_eq!(first, plan_id_for_thread("thread-a"));
    assert_ne!(first, plan_id_for_thread("thread-b"));
    assert!(first.starts_with("plan-") && first.len() == 5 + 16);
}

#[test]
fn the_largest_accepted_task_still_fits_a_tool_result_with_its_envelope() {
    // The reserve is what is left of a tool result once the task is at its limit.
    assert_eq!(
        PLAN_NODE_DETAIL_MAX_BYTES + PLAN_TASK_RESULT_RESERVE_BYTES,
        PLAN_TOOL_RESULT_MAX_BYTES
    );
    // Given: a task grown (a quote takes two bytes) until it is the largest one accepted.
    let task = |result_bytes: usize| {
        vec![
            PlanOperation::Add {
                client_ref: "a".into(),
                parent: None,
                position: None,
                title: "t".into(),
                description: Some("\"".repeat(PLAN_DESCRIPTION_MAX_BYTES)),
                weight: None,
                status: Some(PlanTaskStatus::Pending),
            },
            PlanOperation::Update {
                task: TaskRef::Ref("a".into()),
                title: None,
                description: None,
                result: Some("\"".repeat(result_bytes)),
                blocker: None,
                weight: None,
                status: None,
            },
        ]
    };
    let accepted = (0..=PLAN_RESULT_MAX_BYTES)
        .rev()
        .find_map(|bytes| {
            let (next, _) = tree().apply(&task(bytes), false).ok()?;
            next.finalize().ok().map(|_| next)
        })
        .expect("a task within the limit exists");
    let flat = accepted.flatten().0;
    let detail = accepted.detail(&flat[0]);
    let size = serde_json::to_string(&detail).unwrap().len();
    assert!(size <= PLAN_NODE_DETAIL_MAX_BYTES);
    assert!(
        size + 2 > PLAN_NODE_DETAIL_MAX_BYTES,
        "not the largest: {size}"
    );
    // When: the task is wrapped as a `plan_read` task result, with the widest identifiers.
    let wrapped = serde_json::to_string(&serde_json::json!({
        "plan_id": plan_id_for_thread("thread"),
        "revision": u64::MAX,
        "node": detail,
    }))
    .unwrap();
    // Then: the wrapper takes little, and a reply envelope of 1 KiB still fits.
    assert!(wrapped.len() <= PLAN_NODE_DETAIL_MAX_BYTES + 128);
    assert!(wrapped.len() + 1024 <= PLAN_TOOL_RESULT_MAX_BYTES);
}

#[test]
fn a_reconciliation_may_name_every_task_while_a_model_batch_stops_at_its_own_limit() {
    // Given: a plan of the most leaves, all in progress (added in batches of the model's size).
    let mut plan = tree();
    for batch in 0..PLAN_MAX_NODES / PLAN_MAX_OPERATIONS {
        let operations: Vec<_> = (0..PLAN_MAX_OPERATIONS)
            .map(|n| {
                add_with(
                    &format!("t{batch}-{n}"),
                    None,
                    1,
                    PlanTaskStatus::InProgress,
                )
            })
            .collect();
        plan = run(&plan, &operations);
    }
    assert_eq!(plan.nodes.len(), PLAN_MAX_NODES);
    let operations = plan.interrupt_operations();
    assert_eq!(operations.len(), PLAN_MAX_RECONCILE_OPERATIONS);
    // The runtime applies them all at once.
    let (next, applied) = plan.apply(&operations, true).unwrap();
    assert_eq!(applied.changes.len(), PLAN_MAX_NODES);
    assert!(next.interrupt_operations().is_empty());
    // A model batch of the same operations is over its limit; so is one more than the plan
    // can hold for the runtime.
    assert!(matches!(
        plan.apply(&operations, false),
        Err(PlanRejection::TooManyOperations {
            actual: PLAN_MAX_NODES,
            max: PLAN_MAX_OPERATIONS
        })
    ));
    let mut too_many = operations.clone();
    too_many.push(operations[0].clone());
    assert!(matches!(
        plan.apply(&too_many, true),
        Err(PlanRejection::TooManyOperations {
            max: PLAN_MAX_RECONCILE_OPERATIONS,
            ..
        })
    ));
}
