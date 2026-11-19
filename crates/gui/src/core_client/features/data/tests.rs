use super::super::{Feature, FeatureContext, FeatureFrame, FeatureView, Features, Selection};
use super::*;
use crate::core_client::cache::Cache;
use workbench_protocol::ids::{ProjectId, ThreadId};

fn id() -> RendererId {
    RendererId::new("a".repeat(32)).unwrap()
}
fn source() -> DataSource {
    DataSource::Frames {
        tab: RendererTabRef {
            core_epoch: 7,
            generation: 2,
            id: id(),
        },
        subscription: id(),
        token: id(),
    }
}
fn action(inbox: &DataInbox, feature: FeatureId, tag: u64) -> DataAction {
    DataAction {
        feature,
        tag,
        generation: inbox.generation(feature),
        request: DataRequest::Cancel,
    }
}
fn frame(sequence: u64) -> DataPayload {
    DataPayload::Frame(Frame {
        start: FrameStart {
            core_epoch: 7,
            generation: 2,
            tab: id(),
            sequence,
            width: 1,
            height: 1,
            stride: 4,
            total_bytes: 4,
        },
        bgra: vec![sequence as u8; 4],
    })
}
#[test]
fn latest_slot_replaces_instead_of_queuing_and_reset_discards_payloads_and_old_requests() {
    let mut inbox = DataInbox::default();
    let old = action(&inbox, FeatureId::Browser, 3);
    let key = inbox.claim(&old, source()).unwrap();
    for sequence in 1..=100 {
        assert!(inbox.publish(key, frame(sequence)));
    }
    assert_eq!(inbox.usage().0, 1);
    assert_eq!(inbox.usage().1, 4);
    assert!(
        matches!(inbox.take(FeatureId::Browser, 3), Some(DataPayload::Frame(f)) if f.start.sequence == 100)
    );
    assert!(inbox.take(FeatureId::Browser, 3).is_none());
    assert!(inbox.publish(key, frame(101)));
    inbox.reset(FeatureId::Browser);
    assert_eq!(inbox.usage(), (0, 0, 0));
    assert!(!inbox.publish(key, frame(102)));
    assert!(inbox.claim(&old, source()).is_err());
    let new = inbox
        .claim(&action(&inbox, FeatureId::Browser, 3), source())
        .unwrap();
    assert_ne!(new, key);
    assert!(!inbox.publish(key, frame(103)));
    assert!(inbox.contains(new));
}
#[test]
fn the_counters_tell_replaced_frames_from_taken_ones_and_forget_nothing_on_reset() {
    let mut inbox = DataInbox::default();
    let key = inbox
        .claim(&action(&inbox, FeatureId::Browser, 3), source())
        .unwrap();
    for sequence in 1..=5 {
        assert!(inbox.publish(key, frame(sequence)));
    }
    assert_eq!(inbox.stats().bytes_held, 4, "one latest frame is held");
    assert!(inbox.take(FeatureId::Browser, 3).is_some());
    assert!(inbox.take(FeatureId::Browser, 3).is_none());
    assert_eq!(
        inbox.stats(),
        DataStats {
            frames_received: 5,
            frames_replaced: 4,
            frames_taken: 1,
            bytes_held: 0,
        }
    );
    inbox.reset(FeatureId::Browser);
    assert_eq!(inbox.stats().frames_received, 5, "counters outlive a reset");
}
#[test]
fn slot_count_and_total_receive_budget_include_incoming_buffers_and_completed_blobs() {
    let mut inbox = DataInbox::default();
    for tag in 0..MAX_FRAME_SUBSCRIPTIONS as u64 {
        let key = inbox
            .claim(&action(&inbox, FeatureId::Browser, tag), source())
            .unwrap();
        assert!(inbox.publish(key, frame(tag)));
    }
    assert!(
        inbox
            .claim(&action(&inbox, FeatureId::Browser, 99), source())
            .is_err()
    );
    for tag in 20..20 + MAX_BLOB_TRANSFERS as u64 {
        let source = DataSource::Capture {
            operation_id: id(),
            capture_id: id(),
        };
        inbox
            .claim(&action(&inbox, FeatureId::Browser, tag), source)
            .unwrap();
    }
    assert_eq!(
        inbox.usage().0,
        MAX_FRAME_SUBSCRIPTIONS + MAX_BLOB_TRANSFERS
    );
    assert_eq!(inbox.usage().2, MAX_DATA_BYTES);
    assert!(
        inbox
            .claim(
                &action(&inbox, FeatureId::Browser, 100),
                DataSource::Capture {
                    operation_id: id(),
                    capture_id: id()
                }
            )
            .is_err()
    );
    inbox.reset(FeatureId::Browser);
    assert_eq!(inbox.usage(), (0, 0, 0));
}
#[test]
fn stale_blob_completion_is_dropped_and_artifact_bytes_are_bounded() {
    let mut inbox = DataInbox::default();
    let artifact = ArtifactRef {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        artifact_id: "artifact-".to_owned() + &"a".repeat(32),
        digest: "a".repeat(64),
    };
    let request = action(&inbox, FeatureId::Artifacts, 1);
    let key = inbox
        .claim(&request, DataSource::Artifact(artifact.clone()))
        .unwrap();
    let blob = |len| {
        DataPayload::Blob(Blob {
            start: BlobStart {
                core_epoch: 7,
                transfer_id: 1,
                source: BlobSource::Artifact,
                media_type: MediaType::Png,
                total_bytes: len as u64,
                digest: [0; 32],
            },
            bytes: vec![0; len],
        })
    };
    assert!(!inbox.publish(key, blob(MAX_IMAGE_BLOB_BYTES as usize + 1)));
    assert!(inbox.publish(key, blob(1)));
    inbox.reset(FeatureId::Artifacts);
    let fresh = inbox
        .claim(
            &action(&inbox, FeatureId::Artifacts, 1),
            DataSource::Artifact(artifact),
        )
        .unwrap();
    assert!(!inbox.publish(key, blob(3)));
    assert!(inbox.contains(fresh));
    assert_eq!(inbox.usage().1, 0);
}
#[test]
fn artifact_preview_route_only_passes_a_reference_to_the_browser_queue() {
    use std::sync::{Arc, Mutex};
    struct Browser(Arc<Mutex<Vec<ArtifactRef>>>);
    impl Feature for Browser {
        fn id(&self) -> FeatureId {
            FeatureId::Browser
        }
        fn title(&self) -> &'static str {
            "probe"
        }
        fn open_preview(&mut self, artifact: ArtifactRef) {
            self.0.lock().unwrap().push(artifact);
        }
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut features = Features::default();
    features.install(Box::new(Browser(log.clone())));
    let artifact = ArtifactRef {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        artifact_id: "artifact-".to_owned() + &"a".repeat(32),
        digest: "a".repeat(64),
    };
    let (cache, selection) = (Cache::default(), Selection::default());
    let mut frame = FeatureFrame::default();
    let mut ctx = FeatureContext {
        feature: FeatureId::Artifacts,
        view: FeatureView {
            connected: true,
            capabilities: &[],
            cache: &cache,
            selection: &selection,
        },
        frame: &mut frame,
        data: &mut features.data,
    };
    ctx.open_preview(artifact.clone());
    assert!(frame.actions.is_empty());
    assert!(frame.data.is_empty());
    for reference in frame.previews {
        features.open_preview(reference);
    }
    assert_eq!(*log.lock().unwrap(), [artifact]);
}

#[test]
fn agent_and_renderer_share_eight_slots_and_agent_identity_and_payload_kind_are_fenced() {
    use workbench_protocol::namespaces::browser::observation::{
        ObservationId, ObservationScope, data::AgentFrameStart,
    };
    let scope = ObservationScope {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        run: workbench_protocol::ids::RunId::new("r").unwrap(),
        incarnation: workbench_protocol::ids::RunId::new("i").unwrap(),
        core_epoch: 7,
        browser_generation: 1,
        page_generation: 1,
    };
    let source_agent = DataSource::AgentFrames {
        scope: scope.clone(),
        subscription: ObservationId::new("a".repeat(32)).unwrap(),
        token: ObservationId::new("b".repeat(32)).unwrap(),
    };
    let mut inbox = DataInbox::default();
    for tag in 0..7 {
        inbox
            .claim(&action(&inbox, FeatureId::Browser, tag), source())
            .unwrap();
    }
    let key = inbox
        .claim(
            &action(&inbox, FeatureId::Browser, 1 << 63),
            source_agent.clone(),
        )
        .unwrap();
    assert!(
        inbox
            .claim(&action(&inbox, FeatureId::Browser, 99), source_agent)
            .is_err()
    );
    assert!(
        !inbox.publish(key, frame(1)),
        "renderer BGRA cannot enter agent RGBA slot"
    );
    let make = |scope| {
        DataPayload::AgentFrame(workbench_client::renderer::observation::AgentFrame {
            start: AgentFrameStart {
                scope,
                tab: ObservationId::new("a".repeat(32)).unwrap(),
                sequence: 1,
                width: 1,
                height: 1,
                stride: 4,
                total_bytes: 4,
            },
            rgba: vec![1; 4],
        })
    };
    let mut stale = scope.clone();
    stale.incarnation = workbench_protocol::ids::RunId::new("old").unwrap();
    assert!(!inbox.publish(key, make(stale)));
    assert!(inbox.publish(key, make(scope.clone())));
    inbox.reset(FeatureId::Browser);
    assert!(!inbox.publish(key, make(scope)));
}
