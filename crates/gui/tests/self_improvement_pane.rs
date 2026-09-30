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
        |ui, pane: &mut SelfImprovementPane| pane.render(ui, Some("p")),
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
    pane.config = Some(config);
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
