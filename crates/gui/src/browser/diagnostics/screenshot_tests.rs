use super::*;
use storage::{Database, Storage, StorageConfig};

#[tokio::test]
async fn oversized_screenshot_survives_bus_and_storage() {
    // Given: a valid JPEG larger than the event size limit.
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut bytes)
        .encode(&[0, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    bytes.resize(300_000, 0);
    let bus = EventBus::new(4);
    let mut receiver = bus.subscribe();
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("screenshots.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();

    // When: the real screenshot emission path reaches the persistent writer.
    emit_screenshot(&bus, &bytes, ("action-1", "before")).unwrap();
    let event = receiver.recv().await.unwrap();
    storage.handle().append_event(None, &event).unwrap();

    // Then: evidence is persisted rather than silently dropped.
    let events = Database::open(&config)
        .unwrap()
        .events_all_ordered()
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event);
    assert!(serde_json::to_vec(&event.kind).unwrap().len() < 1024);
    let event_bus::EventKind::Diagnostic(diagnostic) = event.kind else {
        panic!("expected diagnostic");
    };
    let detail: serde_json::Value = serde_json::from_str(&diagnostic.detail).unwrap();
    assert_eq!(detail["representation"], "omitted");
    assert_eq!(detail["omission_reason"], "inline_size_limit");
    assert!(detail["base64"].is_null());
    assert_eq!(detail["byte_length"], 300_000);
    assert_eq!(detail["width"], 1);
    assert_eq!(detail["height"], 1);
    assert_eq!(detail["sha256"], format!("{:x}", Sha256::digest(&bytes)));
}

#[tokio::test]
async fn screenshot_inline_boundary_preserves_original_or_explicitly_omits() {
    // Given: JPEGs at and immediately above the inline boundary.
    for size in [MAX_INLINE_SCREENSHOT_BYTES, MAX_INLINE_SCREENSHOT_BYTES + 1] {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode(&[255, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        bytes.resize(size, 0);
        let bus = EventBus::new(4);
        let mut receiver = bus.subscribe();

        // When: screenshot evidence is emitted for the after phase.
        emit_screenshot(&bus, &bytes, ("action-2", "after")).unwrap();
        let event = receiver.recv().await.unwrap();

        // Then: the boundary keeps complete bytes; exceeding it records omission.
        let event_bus::EventKind::Diagnostic(diagnostic) = event.kind else {
            panic!("expected diagnostic");
        };
        let detail: serde_json::Value = serde_json::from_str(&diagnostic.detail).unwrap();
        assert_eq!(detail["id"], "action-2");
        assert_eq!(detail["phase"], "after");
        if size == MAX_INLINE_SCREENSHOT_BYTES {
            assert_eq!(detail["representation"], "inline");
            assert!(detail["omission_reason"].is_null());
            assert_eq!(
                STANDARD.decode(detail["base64"].as_str().unwrap()).unwrap(),
                bytes
            );
        } else {
            assert_eq!(detail["representation"], "omitted");
            assert!(detail["base64"].is_null());
        }
    }
}
