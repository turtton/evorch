//! The wire contract of queued-input previews and drafts (ADR 0045 E7): when a receipt
//! may carry a preview, and what a draft reply may hold.
mod common;

#[allow(unused_imports)]
use common::*;

use workbench_protocol::{
    ids::{RunId, SubmissionId, ThreadId},
    namespaces::conversation::{InputDraft, MAX_INPUT_DRAFT_BYTES},
    views::{InputPreview, InputReceipt, InputReceiptState},
};

fn preview(text: &str, images: u32, attachments: u32) -> InputPreview {
    InputPreview {
        text: text.to_owned(),
        truncated: false,
        image_count: images,
        attachment_count: attachments,
    }
}

fn receipt_in(state: InputReceiptState, preview: Option<InputPreview>) -> InputReceipt {
    InputReceipt {
        state,
        preview,
        ..common::receipt()
    }
}

fn draft(state: InputReceiptState, text: &str) -> InputDraft {
    InputDraft {
        thread: ThreadId::new("thread-1").unwrap(),
        submission_id: SubmissionId::new("sub-a").unwrap(),
        run: RunId::new("run-1").unwrap(),
        state,
        text: text.to_owned(),
        image_count: 1,
        attachment_count: 1,
    }
}

#[test]
fn a_receipt_without_a_preview_keeps_its_wire_form() {
    let wire = serde_json::to_value(receipt_in(InputReceiptState::Accepted, None)).unwrap();
    assert!(wire.get("preview").is_none());
    // An old peer's receipt, without the field, still reads.
    let back: InputReceipt = serde_json::from_value(wire).unwrap();
    assert_eq!(back.preview, None);
}

#[test]
fn a_preview_round_trips_and_only_an_editable_state_may_carry_it() {
    for state in [
        InputReceiptState::Accepted,
        InputReceiptState::Dropped,
        InputReceiptState::Unknown,
    ] {
        let with = receipt_in(state, Some(preview("hello", 1, 2)));
        assert!(with.validate().is_ok());
        let wire = serde_json::to_value(&with).unwrap();
        assert_eq!(wire["preview"]["image_count"], 1);
        assert_eq!(serde_json::from_value::<InputReceipt>(wire).unwrap(), with);
    }
    for state in [InputReceiptState::Delivered, InputReceiptState::Withdrawn] {
        assert!(
            receipt_in(state, Some(preview("hello", 0, 0)))
                .validate()
                .is_err()
        );
        assert!(receipt_in(state, None).validate().is_ok());
    }
}

#[test]
fn a_preview_stays_short_and_its_counts_agree() {
    let accepted = |p| receipt_in(InputReceiptState::Accepted, Some(p));
    assert!(accepted(preview("", 0, 0)).validate().is_ok());
    assert!(
        accepted(preview(&"a".repeat(1024), 0, 0))
            .validate()
            .is_ok()
    );
    assert!(
        accepted(preview(&"a".repeat(1025), 0, 0))
            .validate()
            .is_err()
    );
    assert!(accepted(preview("bell\u{7}", 0, 0)).validate().is_err());
    assert!(accepted(preview("x", 3, 2)).validate().is_err());
}

#[test]
fn a_draft_is_one_bounded_body_of_an_editable_input() {
    assert!(draft(InputReceiptState::Accepted, "").validate().is_ok());
    assert!(
        draft(
            InputReceiptState::Unknown,
            &"é".repeat(MAX_INPUT_DRAFT_BYTES / 2)
        )
        .validate()
        .is_ok()
    );
    assert!(
        draft(
            InputReceiptState::Dropped,
            &"a".repeat(MAX_INPUT_DRAFT_BYTES + 1)
        )
        .validate()
        .is_err()
    );
    assert!(draft(InputReceiptState::Delivered, "x").validate().is_err());
    assert!(draft(InputReceiptState::Withdrawn, "x").validate().is_err());
}
