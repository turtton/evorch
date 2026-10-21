//! The draft triage button (ADR 0037): when it may open a read-only thread,
//! why it may not, and what it hands the workbench.

use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use gui::panes::self_improvement::{SelfImprovementAction, SelfImprovementPane, TRIAGE_BUTTON};
use storage::improvement::{
    ImprovementSeverity, ImprovementSource, ImprovementStatus, ImprovementWritePolicy,
    NewImprovementCandidate,
};
use storage::{Storage, StorageConfig};

type PaneState = (SelfImprovementPane, Option<SelfImprovementAction>);

fn record(store: &Storage, id: &str, draft: Option<&str>) {
    store
        .handle()
        .record_improvement_candidate(
            "p",
            NewImprovementCandidate {
                id: id.into(),
                source: ImprovementSource::Diagnostic,
                severity: ImprovementSeverity::Warning,
                code: "NoProgress".into(),
                title: format!("Candidate {id}"),
                evidence: "Repeated tool calls".into(),
                dedup_key: id.into(),
                run_id: None,
            },
            ImprovementWritePolicy {
                duplicate_cooldown: std::time::Duration::from_secs(60),
                daily_limit: 20,
                max_candidates: 200,
            },
        )
        .unwrap();
    if let Some(draft) = draft {
        store.handle().attach_improvement_draft(id, draft).unwrap();
    }
}

/// Storage under `db/`, drafts under `drafts/`, both inside one temp directory.
fn storage() -> (tempfile::TempDir, StorageConfig, Storage) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("db")).unwrap();
    std::fs::create_dir_all(dir.path().join("drafts")).unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("db/store.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    (dir, config, store)
}

fn harness(
    config: StorageConfig,
    draft_dir: Option<std::path::PathBuf>,
) -> Harness<'static, PaneState> {
    let mut pane = SelfImprovementPane::default();
    pane.enabled = true;
    pane.config = Some(config);
    pane.draft_dir = draft_dir;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(900.0, 600.0))
        .build_ui_state(
            |ui, state: &mut PaneState| {
                if let Some(action) = state.0.render(ui, Some("p")) {
                    state.1 = Some(action);
                }
            },
            (pane, None),
        );
    harness.run();
    harness
}

fn enabled(harness: &Harness<'_, PaneState>) -> bool {
    !harness
        .get_by_label(TRIAGE_BUTTON)
        .accesskit_node()
        .is_disabled()
}

fn assert_disabled_because(harness: &mut Harness<'_, PaneState>, reason: &str) {
    assert!(!enabled(harness), "button must be disabled: {reason}");
    harness.get_by_label(TRIAGE_BUTTON).hover();
    harness.run_steps(3);
    harness.get_by_label_contains(reason);
}

#[test]
fn the_button_needs_an_existing_draft_directory() {
    let (dir, config, store) = storage();
    record(&store, "first", Some("first.md"));

    let mut unset = harness(config.clone(), None);
    assert_disabled_because(&mut unset, "Draft directory is not configured");

    let missing = dir.path().join("missing");
    let mut gone = harness(config.clone(), Some(missing.clone()));
    assert_disabled_because(
        &mut gone,
        &format!("Draft directory does not exist: {}", missing.display()),
    );

    let ready = harness(config, Some(dir.path().join("drafts")));
    assert!(enabled(&ready));
}

#[test]
fn the_button_is_disabled_while_drafts_share_the_database_directory() {
    let (dir, config, store) = storage();
    record(&store, "first", Some("first.md"));
    let reason = "The draft directory also holds the candidate database";

    let mut same = harness(config.clone(), Some(dir.path().join("db")));
    assert_disabled_because(&mut same, reason);
    // An ancestor of the database directory holds it too.
    let mut above = harness(config.clone(), Some(dir.path().to_owned()));
    assert_disabled_because(&mut above, reason);
    // A link is judged by the directory it resolves to.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("db"), dir.path().join("link")).unwrap();
        let mut linked = harness(config, Some(dir.path().join("link")));
        assert_disabled_because(&mut linked, reason);
    }
}

#[test]
fn the_button_needs_candidates_under_the_status_filter() {
    let (dir, config, store) = storage();
    let mut empty = harness(config, Some(dir.path().join("drafts")));
    assert_disabled_because(&mut empty, "No candidates match the status filter");

    record(&store, "first", None);
    empty.state_mut().0.status = None;
    empty.run();
    assert!(enabled(&empty));
}

#[test]
fn pressing_the_button_hands_over_the_filtered_triage_thread() {
    let (dir, config, store) = storage();
    record(&store, "first", Some("first.md"));
    record(&store, "second", None);
    record(&store, "gone", Some("gone.md"));
    store
        .handle()
        .set_improvement_status("gone", ImprovementStatus::Dismissed)
        .unwrap();
    let mut harness = harness(config, Some(dir.path().join("drafts")));
    assert_eq!(harness.state().0.status, Some(ImprovementStatus::New));

    harness.get_by_label(TRIAGE_BUTTON).click();
    harness.run();

    let drafts = dir.path().join("drafts").canonicalize().unwrap();
    let Some(SelfImprovementAction::CreateTriageThread {
        title,
        read_dirs,
        prompt,
    }) = harness.state_mut().1.take()
    else {
        panic!("the button must hand over a triage thread");
    };
    assert_eq!(title, "Triage drafts (2 candidates, new)");
    assert_eq!(read_dirs, std::slice::from_ref(&drafts));
    assert!(prompt.contains(&format!("draft のディレクトリ: `{}`", drafts.display())));
    let draft = drafts.join("first.md");
    assert!(prompt.contains(&format!(
        "| first | NoProgress | Candidate first | 1 | new | {} |",
        draft.display()
    )));
    assert!(prompt.contains("| second | NoProgress | Candidate second | 1 | new | - |"));
    assert!(!prompt.contains("| gone |"));
    // Nothing is handed over until the next press.
    harness.run();
    assert!(harness.state().1.is_none());
}

#[test]
fn project_ancestors_root_and_descendants_disable_triage() {
    let (dir, config, store) = storage();
    record(&store, "first", Some("first.md"));
    let project = dir.path().join("drafts/project");
    let inside = project.join("drafts");
    std::fs::create_dir_all(&inside).unwrap();
    for draft in [dir.path().join("drafts"), project.clone(), inside] {
        let mut harness = harness(config.clone(), Some(draft));
        harness.state_mut().0.project_root = Some(project.clone());
        harness.run();
        assert_disabled_because(&mut harness, "overlaps the project root");
        assert!(harness.state().1.is_none());
    }
}
