// Captured by the unchanged main writer; see fixtures/waits-main-2eec1163/README.md.
use crate::quiesce::RunRecoveryRecord;
use crate::run_store::RunStore;

#[test]
fn verify_saved_wait_fixtures() {
    let out = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/waits-main-2eec1163");
    let dir = tempfile::tempdir_in(std::env::var("CARGO_MANIFEST_DIR").unwrap()).unwrap();
    let copy = dir.path().join("fixture-copy.db");
    std::fs::copy(out.join("mixed-manifest.sqlite"), &copy).unwrap();
    let mut cfg = storage::StorageConfig {
        db_path: copy,
        ..Default::default()
    };
    cfg.thread_index.enabled = false;
    let storage = storage::Storage::open(cfg.clone()).unwrap();
    let store = RunStore::open(&cfg, storage.handle()).unwrap();
    let redactor = secret_guard::SecretRedactor::from_env();
    let mut scanned = 0;
    for entry in std::fs::read_dir(&out).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                redactor.detect(&text).is_none(),
                "credential shape in {}",
                path.display()
            );
            serde_json::from_str::<serde_json::Value>(&text).unwrap();
            scanned += 1;
        }
    }
    assert_eq!(scanned, 26);
    let db = storage::Database::open(&cfg).unwrap();
    let loads = db.recovery_manifests().unwrap();
    assert_eq!(loads.len(), 4);
    let mut members = 0;
    for load in &loads {
        let manifest = load.record.as_ref().unwrap();
        assert!(manifest.committed && !manifest.superseded);
        assert_eq!(manifest.schema_version, 1);
        members += manifest.members.len();
        for member in &manifest.members {
            assert!(redactor.detect(&member.record_json).is_none());
            let record: RunRecoveryRecord = serde_json::from_str(&member.record_json).unwrap();
            let snapshot = db.run_context(&member.run_id).unwrap().unwrap();
            assert!(redactor.detect(&snapshot.config_json).is_none());
            assert!(redactor.detect(&snapshot.messages_json).is_none());
            let intent = db.resume_intent(&member.run_id).unwrap().unwrap();
            assert!(redactor.detect(&intent.payload).is_none());
            crate::restore::validate_recovery_waits(&snapshot, &record.waits).unwrap();
        }
    }
    assert_eq!(members, 12);
    for name in [
        "runs-legacy-true",
        "runs-legacy-false",
        "delegates",
        "reply",
        "mixed",
    ] {
        let waits_text = std::fs::read_to_string(out.join(format!("{name}.waits.json"))).unwrap();
        let waits: Vec<crate::quiesce::DurableWait> = serde_json::from_str(&waits_text).unwrap();
        let bundle = store.restore_bundle(waits[0].requester).unwrap().unwrap();
        assert_eq!(
            std::fs::read(out.join(format!("{name}.config.json"))).unwrap(),
            bundle.record.config_json.as_bytes()
        );
        assert_eq!(
            std::fs::read(out.join(format!("{name}.messages.json"))).unwrap(),
            bundle.record.messages_json.as_bytes()
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&waits_text).unwrap(),
            serde_json::from_str::<serde_json::Value>(&bundle.record.config_json).unwrap()["recovery_waits"]
        );
        crate::restore::validate_recovery_waits(&bundle.record, &waits).unwrap();
        let restored =
            crate::restore::RestoredState::from_recovery_bundle(&bundle, &waits).unwrap();
        assert_eq!(restored.recovery_waits, waits);
        let member_text = std::fs::read_to_string(out.join(format!("{name}.member.json"))).unwrap();
        let member: RunRecoveryRecord = serde_json::from_str(&member_text).unwrap();
        assert_eq!(member.waits, waits);
        let intent_text = std::fs::read_to_string(out.join(format!("{name}.intent.json"))).unwrap();
        let intent: crate::quiesce::ResumeIntent = serde_json::from_str(&intent_text).unwrap();
        assert_eq!(intent.run_id, waits[0].requester);
        assert_eq!(
            intent.checkpoint.unwrap().boundary,
            crate::checkpoint::CheckpointBoundary::ToolsWaiting
        );
    }
    eprintln!(
        "independent reload: 26 JSON files, 12 members, 4 committed manifests; no credential shapes detected"
    );
}
