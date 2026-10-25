use std::sync::Arc;

use event_bus::{Event, EventBus, EventKind, ProviderEvent, RequestPurpose};

#[test]
fn ac4_image_generation_purpose_roundtrips_and_obeys_run_fencing() {
    // Existing schema fixtures cover all purpose-carrying attempt event variants.
    for fixture in [
        include_str!("otel_golden/non_mapped_request_started.json"),
        include_str!("otel_golden/request_completed_profile_some.json"),
        include_str!("otel_golden/request_failed_rate_limited.json"),
    ] {
        let mut value: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let event = &mut value["event"];
        event["kind"]["payload"]["payload"]["purpose"] = "image_generation".into();
        event["kind"]["payload"]["payload"]["run_id"] = "image-run".into();
        let parsed: Event = serde_json::from_value(event.clone()).unwrap();
        let purpose = match &parsed.kind {
            EventKind::Provider(
                ProviderEvent::RequestStarted { purpose, .. }
                | ProviderEvent::RequestCompleted { purpose, .. }
                | ProviderEvent::RequestFailed { purpose, .. },
            ) => *purpose,
            other => panic!("unexpected fixture {other:?}"),
        };
        assert_eq!(purpose, Some(RequestPurpose::ImageGeneration));
        let restored: Event =
            serde_json::from_slice(&serde_json::to_vec(&parsed).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(restored).unwrap()["kind"]["payload"]["payload"]["purpose"],
            "image_generation"
        );
        let bus = EventBus::new(8);
        let _receiver = bus.subscribe();
        assert_eq!(bus.emit(parsed.clone()), 1);
        assert!(bus.register_mutation_fence("image-run".into(), Arc::new(|| false)));
        assert_eq!(bus.emit(parsed), 0);
    }
}
