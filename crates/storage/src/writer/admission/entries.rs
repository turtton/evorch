//! Closed storage admission inventory, independent of deletion LIVE_ONLY exemptions.
use std::sync::{Arc, Mutex, mpsc};

#[derive(Default)]
struct Probe {
    pending: usize,
    failed: usize,
}
struct AdmissionProbe {
    state: Arc<Mutex<Probe>>,
    retain: bool,
}
impl crate::MutationAdmission for AdmissionProbe {
    fn persistence_failed(&mut self) {
        self.retain = true;
        self.state.lock().unwrap().failed += 1;
    }
}
impl Drop for AdmissionProbe {
    fn drop(&mut self) {
        if !self.retain {
            self.state.lock().unwrap().pending -= 1;
        }
    }
}

#[test]
fn actual_writer_ack_releases_admission_but_failed_persistence_retains_it() {
    let dir = tempfile::tempdir().unwrap();
    let storage = crate::Storage::open(crate::StorageConfig {
        db_path: dir.path().join("ack.db"),
        ..Default::default()
    })
    .unwrap();
    let handle = storage.handle();
    let state = Arc::new(Mutex::new(Probe::default()));
    let captured = state.clone();
    let (admitted, registered) = mpsc::channel();
    assert!(handle.install_request_admission(Arc::new(move |scope| {
        captured.lock().unwrap().pending += 1;
        admitted.send(scope).unwrap();
        Box::new(AdmissionProbe {
            state: captured.clone(),
            retain: false,
        })
    })));
    let (entered, paused) = mpsc::channel();
    let (release, proceed) = mpsc::channel();
    handle.pause_writer_for_test(entered, proceed).unwrap();
    paused.recv().unwrap();
    let writer = handle.clone();
    let event = event_bus::Event::new(event_bus::MessageEvent::MessageCompleted {
        run_id: "run-1".into(),
        text: "fixture".into(),
    });
    let sent = event.clone();
    let append = std::thread::spawn(move || writer.append_event(None, &sent));
    assert_eq!(registered.recv().unwrap(), Some("r:run-1".into()));
    assert_eq!(state.lock().unwrap().pending, 1);
    release.send(()).unwrap();
    append.join().unwrap().unwrap();
    assert_eq!(state.lock().unwrap().pending, 0);

    handle
        .activity_job(true, |conn| {
            conn.execute_batch(
                "CREATE TEMP TRIGGER reject_ack_fixture BEFORE INSERT ON events \
                 BEGIN SELECT RAISE(ABORT,'fixture persistence failure'); END",
            )?;
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        handle.append_event(None, &event),
        Err(crate::StorageError::Sqlite(_))
    ));
    let held = state.lock().unwrap();
    assert_eq!(held.pending, 1);
    assert_eq!(held.failed, 1);
}

#[test]
fn uncommitted_context_binding_is_global_without_reading_or_decoding_context() {
    let (reply, _receive) = mpsc::channel();
    let record = crate::RunContextRecord {
        run_id: "run-1".into(),
        role: "worker".into(),
        name: "chat:project:not-yet-bound".into(),
        parent_run_id: None,
        config_json: "not decoded during admission".into(),
        messages_json: "not decoded during admission".into(),
        checkpoints_json: "not decoded during admission".into(),
        terminal_phase: "waiting".into(),
        restorable: true,
        updated_at_ns: 0,
        host_inputs_json: "not decoded during admission".into(),
    };
    assert_eq!(
        super::scope(&super::Command::UpsertRunContext(record, reply)),
        Some(None)
    );
}

fn methods(text: &str) -> Vec<String> {
    let mut names = text
        .lines()
        .filter_map(|line| {
            let public = line.trim_start().strip_prefix("pub ")?;
            let (_, tail) = public.split_once("fn ")?;
            Some(
                tail.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}
fn check(
    rows: &[(&str, &str, &str)],
    actual: std::collections::BTreeMap<String, Vec<String>>,
    root: &std::path::Path,
) {
    let mut expected = std::collections::BTreeMap::<String, Vec<String>>::new();
    for (file, names, route) in rows {
        assert!(
            !route.trim().is_empty(),
            "{file}: classify guarded/delegated/exempt with reason"
        );
        assert!(root.join(file).is_file(), "removed entry source: {file}");
        expected
            .entry((*file).into())
            .or_default()
            .extend(names.split_whitespace().map(str::to_owned));
    }
    for names in expected.values_mut() {
        names.sort();
    }
    assert_eq!(
        actual, expected,
        "classify every new/removed public entry; do not reuse deletion LIVE_ONLY exemptions"
    );
}

#[test]
fn every_public_writer_entry_has_an_archive_admission_route_or_reason() {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("src");
    fn visit(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, Vec<String>>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                visit(root, &p, out)
            } else if p.extension().is_some_and(|e| e == "rs") {
                let m = methods(&std::fs::read_to_string(&p).unwrap());
                if !m.is_empty() {
                    out.insert(
                        p.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                        m,
                    );
                }
            }
        }
    }
    let mut actual = std::collections::BTreeMap::new();
    visit(&root, &root.join("writer"), &mut actual);
    actual.insert(
        "writer.rs".into(),
        methods(&std::fs::read_to_string(root.join("writer.rs")).unwrap()),
    );
    // Pure entry metadata, not calls to the narrowly-owned recovery seams.
    // Keep the production boundary scanner unchanged. Read at runtime so the
    // catalog is not an include_str dependency of the library.
    let catalog = std::fs::read_to_string(
        root.parent()
            .unwrap()
            .join("shared-test-support/archive-admission-entries.tsv"),
    )
    .unwrap();
    let rows = catalog
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            let fields = line.splitn(3, '\t').collect::<Vec<_>>();
            assert_eq!(fields.len(), 3, "entry catalog row");
            (fields[0], fields[1], fields[2])
        })
        .collect::<Vec<_>>();
    check(&rows, actual, &root);
}
