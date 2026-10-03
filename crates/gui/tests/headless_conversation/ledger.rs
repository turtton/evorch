use event_bus::{Event, LedgerEvent};
use gui::{app::WorkbenchState, fixture::DemoSource};
use storage::{Database, Storage, StorageConfig};
use workspace_ui::UiSettings;

#[test]
fn selected_run_ledger_is_visible_without_transcript_messages() {
    // Given: two runs with ledger-only content in the real workbench.
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).unwrap();
    state.apply_events(
        [("selected", "Selected entry"), ("other", "Other entry")].map(|(run_id, body)| {
            Event::new(LedgerEvent::RunLedgerAppended {
                run_id: run_id.into(),
                seq: 1,
                body: body.into(),
            })
        }),
    );
    state.drill_down("selected");
    let mut gui = gui::headless::HeadlessWorkbench::new(state, [1200.0, 900.0]);
    gui.run();
    // When: expanding the selected run's ledger through the GUI.
    gui.click_label("> Ledger");
    gui.run();
    // Then: only the selected run's entry is visible.
    assert!(gui.has_label("- seq 1: Selected entry"));
    assert!(!gui.has_label("- seq 1: Other entry"));
    if let Some(directory) = std::env::var_os("LEDGER_EVIDENCE_DIR") {
        gui.capture()
            .unwrap()
            .save_png(&std::path::PathBuf::from(directory).join("ledger-workbench.png"))
            .unwrap();
    }
}

#[test]
fn ledger_event_populates_registry_for_run() {
    // Given: an empty workbench and out-of-order ledger notifications.
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).unwrap();
    // When: the same event fold used by the event pump receives entries.
    state.apply_events([(3, "last"), (1, "first"), (3, "last")].map(|(seq, body)| {
        Event::new(LedgerEvent::RunLedgerAppended {
            run_id: "run-1".into(),
            seq,
            body: body.into(),
        })
    }));
    // Then: entries are ordered and replay-safe, without transcript content.
    let entries = state.ledger().entries("run-1");
    assert_eq!(
        entries
            .iter()
            .map(|entry| (entry.seq, entry.body.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "first"), (3, "last")]
    );
    assert!(state.ledger().entries("other").is_empty());
    assert!(
        state
            .transcripts()
            .run("run-1")
            .unwrap()
            .entries()
            .is_empty()
    );
    assert!(state.transcript().entries().is_empty());
}

#[test]
fn restore_history_loads_persisted_ledger() {
    // Given: a real database with ledger rows and a duplicate event-log notification.
    let directory = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: directory.path().join("ledger.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let first = storage
        .handle()
        .append_run_ledger("run-1", "persisted first")
        .unwrap();
    storage
        .handle()
        .append_run_ledger("other", "other run")
        .unwrap();
    storage
        .handle()
        .append_run_ledger("run-1", "persisted last")
        .unwrap();
    storage
        .handle()
        .append_event(
            Some("gui"),
            &Event::new(LedgerEvent::RunLedgerAppended {
                run_id: "run-1".into(),
                seq: first,
                body: "persisted first".into(),
            }),
        )
        .unwrap();
    drop(storage);
    let database = Database::open(&config).unwrap();
    let expected = database.run_ledger("run-1").unwrap();
    let other = database.run_ledger("other").unwrap();
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).unwrap();
    // When: a fresh GUI restores persisted history.
    state.restore_history(&database).unwrap();
    // Then: table rows (including their original timestamps) are restored exactly once.
    assert_eq!(state.ledger().entries("run-1"), expected);
    assert_eq!(state.ledger().entries("other"), other);
    assert!(
        state
            .transcripts()
            .run("run-1")
            .unwrap()
            .entries()
            .is_empty()
    );
}
