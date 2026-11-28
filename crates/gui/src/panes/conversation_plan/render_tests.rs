//! The band and the panel drawn headless from the fixture plans: they show the
//! server's figures, open on request, select and page, and edit nothing.

use super::*;

#[test]
fn a_removed_task_is_named_by_the_title_its_history_saved() {
    let entry = |revision, change| PlanHistoryEntry {
        revision: workbench_protocol::wire::Revision(revision),
        event_id: workbench_protocol::wire::Sequence(revision),
        change: Some(
            workbench_protocol::namespaces::execution::plan::PlanHistoryChange {
                cause: PlanChangeCause::ModelUpdate,
                source_run_id: None,
                changes: vec![change],
                progress: PlanProgress::default(),
            },
        ),
        updated_at_unix_ms: None,
    };
    let renamed = |title: &str| PlanChange::Updated {
        task_id: "gone".into(),
        title: Some(title.into()),
        description: None,
        result: None,
        blocker: None,
        weight: None,
        status: None,
    };
    let added = PlanChange::Added {
        task_id: "gone".into(),
        parent_id: None,
        position: 0,
        title: "First name".into(),
        description: String::new(),
        weight: 1,
        status: PlanTaskStatus::Pending,
    };
    // Newest first, as a page is.
    let entries = vec![
        entry(
            3,
            PlanChange::SubtreeRemoved {
                task_id: "gone".into(),
                task_ids: vec!["gone".into()],
            },
        ),
        entry(2, renamed("Second name")),
        entry(1, added),
    ];

    assert_eq!(task_title(&entries, &[], "gone"), Some("Second name"));
    assert_eq!(task_title(&entries[..1], &[], "gone"), None);
    assert_eq!(task_title(&entries, &[], "other"), None);
}
