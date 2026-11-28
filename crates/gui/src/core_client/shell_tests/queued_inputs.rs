//! Edit and Restore of queued follow-ups in the conversation (ADR 0045 E7): what the rows
//! offer, what reaches the composer and when, and the "Re-attach" hint.
use egui_kittest::kittest::Queryable;
use serde_json::json;
use workbench_protocol::commands::{Command, CommandReply};
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::namespaces::conversation::InputDraft;
use workbench_protocol::namespaces::{ConversationCommand, ConversationQuery, ConversationReply};
use workbench_protocol::views::InputReceipt;

use super::*;
use crate::attachments::AttachmentFile;
use crate::core_client::attachments::ClientAttachmentDraft;

fn receipt(id: &str, state: &str, images: u32, attachments: u32) -> InputReceipt {
    view(json!({
        "submission_id": id, "thread": "thr-1", "run": "run-1", "state": state,
        "revision": "1", "sent_by": {"actor": "human", "surface": "desktop", "client_id": "c-1"},
        "preview": {
            "text": format!("words of {id}"), "truncated": false,
            "image_count": images, "attachment_count": attachments,
        },
    }))
}

fn queue_screen() -> Screen {
    let mut screen = screen_from(
        egui::vec2(1440.0, 900.0),
        super::super::local::LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            None,
        ),
        |shared| {
            for (id, state, images, attachments) in [
                ("sub-a", "accepted", 2, 3),
                ("sub-b", "dropped", 0, 0),
                ("sub-c", "unknown", 1, 1),
            ] {
                shared
                    .cache
                    .merge_receipt(receipt(id, state, images, attachments));
            }
        },
    );
    screen.settle();
    screen
}

fn draft_of(id: &str, state: &str, images: u32, attachments: u32) -> InputDraft {
    view(json!({
        "thread": "thr-1", "submission_id": id, "run": "run-1", "state": state,
        "text": format!("words of {id}"), "image_count": images, "attachment_count": attachments,
    }))
}

impl Screen {
    /// What the execution feature sent since the last call, as (tag, request).
    fn execution_requests(&mut self) -> Vec<(u64, FeatureRequest)> {
        self.harness.run_steps(2);
        self.settle()
            .into_iter()
            .filter_map(|action| match action {
                Action::Feature(FeatureAction {
                    feature: FeatureId::Execution,
                    tag,
                    request,
                }) => Some((tag, request)),
                _ => None,
            })
            .collect()
    }

    fn answer(&mut self, tag: u64, reply: FeatureReply) {
        self.shared
            .lock()
            .unwrap()
            .features
            .reply(FeatureId::Execution, tag, reply);
        self.harness.run_steps(3);
    }

    fn composer_text(&self) -> String {
        self.shared
            .lock()
            .unwrap()
            .drafts
            .get("thr-1")
            .map(|draft| draft.text.clone())
            .unwrap_or_default()
    }

    /// Presses `button` and answers the read of the words with `draft`; returns what is sent
    /// next.
    fn press_and_read(
        &mut self,
        button: &str,
        nth: usize,
        draft: InputDraft,
    ) -> Vec<(u64, FeatureRequest)> {
        self.harness
            .get_all_by_label(button)
            .nth(nth)
            .unwrap()
            .click();
        let sent = self.execution_requests();
        let [
            (
                tag,
                FeatureRequest::Query(Query::Conversation(ConversationQuery::GetInputDraft(get))),
            ),
        ] = sent.as_slice()
        else {
            panic!("one read of the words, got {} requests", sent.len());
        };
        assert_eq!(get.submission_id, draft.submission_id);
        self.answer(
            *tag,
            FeatureReply::Query(Ok(QueryReply::Conversation(ConversationReply::InputDraft(
                draft,
            )))),
        );
        self.execution_requests()
    }
}

fn is_withdraw(request: &FeatureRequest) -> bool {
    matches!(
        request,
        FeatureRequest::Command(Command::Conversation(
            ConversationCommand::WithdrawFollowUp(_)
        ))
    )
}

// Given: queued, dropped and unknown follow-ups of thr-1 with their previews
// Then: the queued one offers Withdraw and Edit, the others Restore, each with its text
#[test]
fn the_queue_shows_previews_and_the_buttons_of_each_state() {
    let screen = queue_screen();

    screen.harness.get_by_label_contains("words of sub-a");
    screen.harness.get_by_label_contains("[3 attachments]");
    screen.harness.get_by_label("Withdraw");
    screen.harness.get_by_label("Edit");
    assert_eq!(screen.harness.get_all_by_label("Restore").count(), 2);
}

// Given: a queued follow-up with two images and a file
// When: Edit is pressed and the core takes the input back
// Then: the composer is untouched until the withdrawal is confirmed, then holds the text,
//       and the hint asks for the 2 images and the other file again
#[test]
fn edit_puts_the_text_in_the_composer_only_after_the_withdrawal_succeeded() {
    let mut screen = queue_screen();

    let next = screen.press_and_read("Edit", 0, draft_of("sub-a", "accepted", 2, 3));
    let [(tag, request)] = next.as_slice() else {
        panic!("one withdrawal, got {} requests", next.len());
    };
    assert!(is_withdraw(request), "{request:?}");
    assert_eq!(
        screen.composer_text(),
        "",
        "nothing changes before the core answers"
    );

    screen.answer(*tag, FeatureReply::Command(Ok(CommandReply::accepted())));

    assert_eq!(screen.composer_text(), "words of sub-a");
    screen.harness.get_by_label("Re-attach 2 images");
    screen.harness.get_by_label("Re-attach 1 file");
}

// Given: an Edit whose withdrawal is refused because the agent adopted the input
// Then: the composer and the hint are unchanged and the queue says why
#[test]
fn a_refused_withdrawal_leaves_the_composer_alone() {
    let mut screen = queue_screen();
    let next = screen.press_and_read("Edit", 0, draft_of("sub-a", "accepted", 2, 3));
    let [(tag, _)] = next.as_slice() else {
        panic!()
    };
    let refused = FeatureError {
        code: Some(ErrorCode::Conflict),
        ..FeatureError::new("adopted")
    };

    screen.answer(*tag, FeatureReply::Command(Err(refused)));

    assert_eq!(screen.composer_text(), "");
    screen
        .harness
        .get_by_label_contains("already took this input");
    assert!(
        screen
            .harness
            .query_by_label_contains("Re-attach")
            .is_none()
    );
}

// Given: a dropped and an unknown follow-up
// When: each is restored
// Then: the text is copied without any withdrawal and nothing is sent
#[test]
fn restore_copies_the_text_of_a_dropped_or_unknown_input_without_withdrawing() {
    for (nth, id, state, images, attachments) in
        [(0, "sub-b", "dropped", 0, 0), (1, "sub-c", "unknown", 1, 1)]
    {
        let mut screen = queue_screen();

        let next = screen.press_and_read("Restore", nth, draft_of(id, state, images, attachments));

        assert!(next.is_empty(), "{id}: nothing else is sent: {next:?}");
        assert_eq!(screen.composer_text(), format!("words of {id}"));
        if images > 0 {
            screen.harness.get_by_label("Re-attach 1 image");
        } else {
            assert!(
                screen
                    .harness
                    .query_by_label_contains("Re-attach")
                    .is_none()
            );
        }
    }
}

// Given: Restore pressed, then the person types before the core answers
// Then: the answer is not applied over what was typed
#[test]
fn a_late_answer_does_not_overwrite_what_the_person_typed() {
    let mut screen = queue_screen();
    screen
        .harness
        .get_all_by_label("Restore")
        .next()
        .unwrap()
        .click();
    let sent = screen.execution_requests();
    let [(tag, _)] = sent.as_slice() else {
        panic!()
    };
    {
        let mut shared = screen.shared.lock().unwrap();
        let draft = shared.drafts.entry("thr-1".into()).or_default();
        draft.text = "typed meanwhile".into();
        draft.revision += 1;
    }

    screen.answer(
        *tag,
        FeatureReply::Query(Ok(QueryReply::Conversation(ConversationReply::InputDraft(
            draft_of("sub-b", "dropped", 0, 0),
        )))),
    );

    assert_eq!(screen.composer_text(), "typed meanwhile");
    screen.harness.get_by_label_contains("composer changed");
}

// Given: a restored input that carried 2 images and a file
// When: an image, then a text file, are attached
// Then: the missing images go down by the image only; the file does not stand in for one
#[test]
fn the_reattach_hint_counts_each_kind_of_file_apart() {
    let mut screen = queue_screen();
    let next = screen.press_and_read("Edit", 0, draft_of("sub-a", "accepted", 2, 3));
    let [(tag, _)] = next.as_slice() else {
        panic!()
    };
    screen.answer(*tag, FeatureReply::Command(Ok(CommandReply::accepted())));
    screen.harness.get_by_label("Re-attach 2 images");

    let attach = |screen: &Screen, id: &str, media_type: &str| {
        let draft = ClientAttachmentDraft::new(
            ProjectId::new("proj-1").unwrap(),
            AttachmentFile {
                draft_id: id.into(),
                filename: id.into(),
                media_type: media_type.into(),
                bytes: vec![1],
            },
        );
        screen
            .shared
            .lock()
            .unwrap()
            .attachment_drafts
            .entry("thr-1".into())
            .or_default()
            .push(draft);
    };
    attach(&screen, "a.png", "image/png");
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Re-attach 1 image");
    screen.harness.get_by_label("Re-attach 1 file");

    attach(&screen, "notes.txt", "text/plain");
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Re-attach 1 image");
    assert!(screen.harness.query_by_label("Re-attach 1 file").is_none());
}

// Given: a composer with text, then one with a file
// Then: Edit and Restore are off, and nothing is sent when they are pressed
#[test]
fn edit_and_restore_wait_for_an_empty_composer() {
    let mut screen = queue_screen();
    {
        let mut shared = screen.shared.lock().unwrap();
        let draft = shared.drafts.entry("thr-1".into()).or_default();
        draft.text = "half written".into();
        draft.revision += 1;
    }
    screen.harness.run_steps(2);

    screen.harness.get_by_label("Edit").click();
    for restore in screen.harness.get_all_by_label("Restore") {
        restore.click();
    }

    assert!(screen.execution_requests().is_empty());
    assert_eq!(screen.composer_text(), "half written");
}
