use egui_kittest::{Harness, kittest::Queryable};
use gui::panes::self_improvement::SelfImprovementPane;
use storage::improvement::{
    ImprovementSeverity, ImprovementSource, ImprovementStatus, ImprovementWritePolicy,
    NewImprovementCandidate,
};
use storage::{Database, Storage, StorageConfig};

fn candidate(id: &str) -> NewImprovementCandidate {
    NewImprovementCandidate {
        id: id.into(),
        source: ImprovementSource::Diagnostic,
        severity: ImprovementSeverity::Warning,
        code: "NoProgress".into(),
        title: format!("Candidate {id}"),
        evidence: "Repeated tool calls".into(),
        dedup_key: id.into(),
        run_id: Some("run-42".into()),
    }
}

fn policy() -> ImprovementWritePolicy {
    ImprovementWritePolicy {
        duplicate_cooldown: std::time::Duration::from_secs(60),
        daily_limit: 20,
        max_candidates: 200,
    }
}

#[test]
fn disabled_pane_does_not_open_storage_or_expose_actions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("must-not-exist.db");
    let mut pane = SelfImprovementPane::default();
    pane.config = Some(StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    });
    let mut harness = Harness::builder().build_ui_state(
        |ui, pane: &mut SelfImprovementPane| pane.render_for_repo_root(ui, Some(dir.path())),
        pane,
    );
    harness.run();
    harness.get_by_label("Self-improvement drafts are disabled ([self_improvement] enabled=false). Candidates are not collected.");
    assert!(harness.query_by_label("Refresh").is_none());
    assert!(harness.query_by_label("Mark reviewed").is_none());
    assert!(!path.exists());
}

#[test]
fn persisted_candidates_filter_review_dismiss_and_refresh_on_project_change() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("store.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let handle = store.handle();
    handle
        .record_improvement_candidate("p", candidate("first"), policy())
        .unwrap();
    handle
        .record_improvement_candidate("other", candidate("second"), policy())
        .unwrap();
    let mut pane = SelfImprovementPane::default();
    pane.enabled = true;
    pane.config = Some(config.clone());
    pane.handle = Some(handle);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 600.0))
        .build_ui_state(
            |ui, state: &mut (SelfImprovementPane, String)| state.0.render(ui, Some(&state.1)),
            (pane, "p".to_owned()),
        );
    harness.run();
    harness.get_by_label("Candidate first");
    harness.get_by_label("Evidence").click();
    harness.run();
    harness.get_by_label("Repeated tool calls");
    harness.get_by_label("Run: run-42");
    harness.get_by_label("Mark reviewed").click();
    harness.run();
    let db = Database::open(&config).unwrap();
    assert_eq!(
        db.improvement_candidates("p", None, 200).unwrap()[0].status,
        ImprovementStatus::Reviewed
    );
    harness.get_by_label("Dismiss").click();
    harness.run();
    assert_eq!(
        db.improvement_candidates("p", None, 200).unwrap()[0].status,
        ImprovementStatus::Dismissed
    );
    harness.get_by_label("Candidate status").click();
    harness.run();
    harness.get_by_label("new").click();
    harness.run();
    harness.get_by_label("No improvement candidates yet.");
    harness.state_mut().1 = "other".into();
    harness.run();
    harness.get_by_label("Candidate second");
    assert!(harness.query_by_label("Candidate first").is_none());
    // Retention can remove a candidate between its rendering and an action.
    // A missing writer target must reload automatically, before the next poll.
    rusqlite::Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "DELETE FROM improvement_candidates WHERE candidate_id = ?1",
            ["second"],
        )
        .unwrap();
    harness.get_by_label("Mark reviewed").click();
    harness.run();
    harness.get_by_label("No improvement candidates yet.");
}

#[test]
fn copy_draft_emits_clipboard_command_and_read_failure_is_visible() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("store.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    store
        .handle()
        .record_improvement_candidate("p", candidate("first"), policy())
        .unwrap();
    store
        .handle()
        .attach_improvement_draft("first", "first.md")
        .unwrap();
    std::fs::write(
        dir.path().join("first.md"),
        "# Local issue draft\nNo publication",
    )
    .unwrap();
    let mut pane = SelfImprovementPane::default();
    pane.enabled = true;
    pane.config = Some(config.clone());
    pane.draft_dir = Some(dir.path().to_owned());
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 600.0))
        .build_ui_state(
            |ui, pane: &mut SelfImprovementPane| pane.render(ui, Some("p")),
            pane,
        );
    harness.run();
    assert!(harness.query_by_label("Mark reviewed").is_none());
    harness.get_by_label("Draft: first.md");
    harness.get_by_label("Copy issue draft").click();
    harness.step();
    assert!(harness.output().platform_output.commands.iter().any(|command| {
        matches!(command, egui::OutputCommand::CopyText(text) if text == "# Local issue draft\nNo publication")
    }));
    std::fs::remove_file(dir.path().join("first.md")).unwrap();
    harness.get_by_label("Copy issue draft").click();
    harness.run();
    harness.get_by_label_contains("Could not read draft:");
    // A background candidate update must not erase a still-visible draft error.
    store
        .handle()
        .set_improvement_status("first", ImprovementStatus::Reviewed)
        .unwrap();
    advance_poll(&mut harness);
    harness.get_by_label_contains("Could not read draft:");
}

#[test]
fn review_writer_errors_are_visible() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("store.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    store
        .handle()
        .record_improvement_candidate("p", candidate("first"), policy())
        .unwrap();
    let mut pane = SelfImprovementPane::default();
    pane.enabled = true;
    pane.config = Some(config);
    pane.handle = Some(store.handle());
    drop(store);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 600.0))
        .build_ui_state(
            |ui, pane: &mut SelfImprovementPane| pane.render(ui, Some("p")),
            pane,
        );
    harness.run();
    harness.get_by_label("Dismiss").click();
    harness.run();
    harness.get_by_label(&storage::StorageError::WriterClosed.to_string());
}

fn advance_poll(harness: &mut Harness<'_, SelfImprovementPane>) {
    let now = harness.ctx.input(|input| input.time);
    harness.input_mut().time = Some(now + 1.1);
    harness.step();
}

#[test]
fn empty_pane_observes_new_candidates_status_and_drafts_without_clicks() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("store.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let other_writer = Storage::open(config.clone()).unwrap();
    let mut pane = SelfImprovementPane::default();
    pane.enabled = true;
    pane.config = Some(config);
    pane.status = Some(ImprovementStatus::New);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 600.0))
        .with_step_dt(0.01)
        .build_ui_state(
            |ui, pane: &mut SelfImprovementPane| pane.render(ui, Some("p")),
            pane,
        );
    harness.run();
    harness.get_by_label("No improvement candidates yet.");
    assert!(harness.query_by_label("Refresh").is_none());
    store
        .handle()
        .record_improvement_candidate("p", candidate("first"), policy())
        .unwrap();
    // Before the polling deadline, unrelated UI frames keep the cached result.
    harness.step();
    assert!(harness.query_by_label("Candidate first").is_none());
    advance_poll(&mut harness);
    harness.get_by_label("Candidate first");
    other_writer
        .handle()
        .attach_improvement_draft("first", "first.md")
        .unwrap();
    advance_poll(&mut harness);
    harness.get_by_label("Draft: first.md");
    other_writer
        .handle()
        .set_improvement_status("first", ImprovementStatus::Reviewed)
        .unwrap();
    advance_poll(&mut harness);
    harness.get_by_label("No improvement candidates yet.");
    // Filter changes reload immediately rather than waiting another second.
    harness.state_mut().status = Some(ImprovementStatus::Reviewed);
    harness.step();
    harness.get_by_label("Candidate first");
    assert!(harness.query_all_by_label("reviewed").next().is_some());
}

#[test]
fn storage_replacement_and_failed_open_recover_automatically() {
    let dir = tempfile::tempdir().unwrap();
    let first = StorageConfig {
        db_path: dir.path().join("missing-parent/store.db"),
        ..Default::default()
    };
    let mut pane = SelfImprovementPane::default();
    pane.enabled = true;
    pane.config = Some(first.clone());
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 600.0))
        .build_ui_state(
            |ui, pane: &mut SelfImprovementPane| pane.render(ui, Some("p")),
            pane,
        );
    harness.run();
    harness.get_by_label_contains("unable to open database file");
    std::fs::create_dir(first.db_path.parent().unwrap()).unwrap();
    let first_store = Storage::open(first).unwrap();
    first_store
        .handle()
        .record_improvement_candidate("p", candidate("first"), policy())
        .unwrap();
    advance_poll(&mut harness);
    harness.get_by_label("Candidate first");
    let second = StorageConfig {
        db_path: dir.path().join("second.db"),
        ..Default::default()
    };
    let second_store = Storage::open(second.clone()).unwrap();
    second_store
        .handle()
        .record_improvement_candidate("p", candidate("second"), policy())
        .unwrap();
    harness.state_mut().config = Some(second);
    harness.step();
    harness.get_by_label("Candidate second");
    assert!(harness.query_by_label("Candidate first").is_none());
}

#[test]
fn unselected_project_does_not_open_storage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("must-not-exist.db");
    let mut pane = SelfImprovementPane::default();
    pane.enabled = true;
    pane.config = Some(StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    });
    let mut harness = Harness::builder().build_ui_state(
        |ui, pane: &mut SelfImprovementPane| pane.render_for_repo_root(ui, None),
        pane,
    );
    harness.run();
    harness.get_by_label("Select a project to browse improvement candidates.");
    assert!(!path.exists());
}
