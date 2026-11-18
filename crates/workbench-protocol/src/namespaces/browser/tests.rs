use super::*;
use serde_json::json;

fn id() -> RendererId {
    RendererId::new("1234567890abcdef1234567890abcdef").unwrap()
}

#[test]
fn renderer_receipts_and_replay_events_contain_only_opaque_ids() {
    let operation_id = id();
    assert_eq!(
        serde_json::to_value(BrowserResult::Accepted {
            operation_id: operation_id.clone()
        })
        .unwrap(),
        json!({"type":"accepted","payload":{"operation_id":operation_id}})
    );
    assert_eq!(
        serde_json::to_value(BrowserEvent::OperationChanged { id: id() }).unwrap(),
        json!({"type":"operation_changed","payload":{"id":id()}})
    );
}

#[test]
fn pixel_results_are_references_not_json_base64_transfers() {
    let tab = RendererTabRef {
        core_epoch: 1,
        generation: 1,
        id: id(),
    };
    assert!(
        BrowserCommand::RendererCdp {
            tab: tab.clone(),
            method: "Page.captureScreenshot".into(),
            params: json!({}),
        }
        .validate()
        .is_err()
    );
    assert!(
        BrowserCommand::CaptureRendererTab { tab }
            .validate()
            .is_ok()
    );
    assert_eq!(
        serde_json::to_value(RendererOutput::Capture {
            capture_id: id(),
            byte_len: 128,
        })
        .unwrap(),
        json!({"type":"capture","payload":{"capture_id":id(),"byte_len":128}})
    );
    assert!(
        serde_json::from_value::<BrowserQuery>(json!({
            "type":"read_renderer_capture",
            "payload":{"capture_id":id(),"offset":0,"max_bytes":1024}
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<BrowserReply>(json!({
            "type":"capture_content",
            "payload":{"capture_id":id(),"offset":0,"base64":"iVBORw==","eof":true}
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<super::super::artifacts::ArtifactsReply>(json!({
            "type":"content",
            "payload":{
                "artifact":{
                    "project":"project", "thread":"thread",
                    "artifact_id":"artifact-1234567890abcdef1234567890abcdef",
                    "digest":"a".repeat(64)
                },
                "offset":0, "base64":"iVBORw==", "eof":true
            }
        }))
        .is_err()
    );
}

#[test]
fn artifact_text_chunks_are_bounded_by_utf8_bytes_not_character_count() {
    use super::super::artifacts::{ArtifactRef, ArtifactsReply, MAX_CONTENT_CHUNK_BYTES};
    use crate::ids::{ProjectId, ThreadId};

    let reply = |text| ArtifactsReply::Content {
        artifact: ArtifactRef {
            project: ProjectId::new("project").unwrap(),
            thread: ThreadId::new("thread").unwrap(),
            artifact_id: "artifact-1234567890abcdef1234567890abcdef".into(),
            digest: "a".repeat(64),
        },
        offset: 0,
        text,
        eof: false,
    };
    let limits = WireLimits::default();
    let max_bytes = MAX_CONTENT_CHUNK_BYTES.min(limits.max_body_chunk_bytes as usize);
    assert!(reply("x".repeat(max_bytes)).validate(&limits).is_ok());
    assert!(reply("x".repeat(max_bytes + 1)).validate(&limits).is_err());
    // Each character needs three UTF-8 bytes; a character-count check is unsafe.
    let multibyte = "あ".repeat(max_bytes / 3 + 1);
    assert!(multibyte.chars().count() < max_bytes);
    assert!(reply(multibyte).validate(&limits).is_err());
}

#[test]
fn request_body_cannot_claim_transport_identity_or_smuggle_a_result() {
    for payload in [
        json!({"owner":"another-client"}),
        json!({"surface":"desktop"}),
        json!({"session":"guessed","client_id":"another-client"}),
        json!({"result":{"data":"private"}}),
    ] {
        assert!(
            serde_json::from_value::<BrowserCommand>(json!({
                "type":"create_renderer_session","payload":payload
            }))
            .is_err()
        );
    }
    assert!(
        serde_json::from_value::<BrowserCommand>(json!({
            "type":"create_renderer_session","payload":{}
        }))
        .is_ok()
    );
    assert!(
        serde_json::from_value::<BrowserQuery>(json!({
            "type":"get_renderer_operation","payload":{
                "operation_id":id(),"owner":"another-client"
            }
        }))
        .is_err()
    );
}

#[test]
fn cdp_result_bounds_use_serialized_bytes_and_keep_envelope_headroom() {
    let reply = |bytes| {
        BrowserReply::Operation(RendererOperation {
            operation_id: id(),
            state: RendererOperationState::Completed {
                output: RendererOutput::Cdp {
                    result: Value::String("x".repeat(bytes)),
                },
            },
        })
    };
    let limits = WireLimits::default();
    // JSON quotes count toward 256 KiB, not just the contained string.
    assert!(reply(MAX_CDP_RESULT_BYTES - 2).validate(&limits).is_ok());
    assert!(reply(MAX_CDP_RESULT_BYTES - 1).validate(&limits).is_err());
    let small = WireLimits {
        max_frame_bytes: 2048,
        ..limits
    };
    assert!(reply(1024).validate(&small).is_err());
    assert!(
        BrowserReply::Operation(RendererOperation {
            operation_id: id(),
            state: RendererOperationState::Failed {
                reason: RendererFailure::ResultTooLarge
            },
        })
        .validate(&small)
        .is_ok()
    );
}

#[test]
fn viewport_matches_uhd_budget_and_ids_are_not_paths_or_free_text() {
    assert!(
        RendererViewport {
            width: 3840,
            height: 2160
        }
        .validate()
        .is_ok()
    );
    for viewport in [
        RendererViewport {
            width: 3841,
            height: 2160,
        },
        RendererViewport {
            width: 3840,
            height: 2161,
        },
        RendererViewport {
            width: 0,
            height: 768,
        },
    ] {
        assert!(viewport.validate().is_err());
    }
    for invalid in [
        "../artifact",
        "/tmp/page",
        "",
        "1234567890ABCDEF1234567890abcdef",
    ] {
        assert!(RendererId::new(invalid).is_err());
    }
}
