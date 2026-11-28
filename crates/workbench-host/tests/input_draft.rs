//! Queued human input can be edited or restored (ADR 0045 E7): receipts carry a bounded
//! preview while a person can still act on the input, and its words are read by id.

mod common;

use base64::{Engine, engine::general_purpose::STANDARD};
use common::{host_with_project, runtime_with_store, send, web};
use workbench_host::CommandHost;
use workbench_protocol::{
    commands::Command,
    conversation::{ConversationCommand, ConversationResult},
    errors::ErrorCode,
    ids::{AttachmentId, SessionId, SubmissionId, ThreadId},
    namespaces::conversation::{GetInputDraft, InputDraft},
    namespaces::{ConversationQuery, ConversationReply},
    queries::{Query, QueryReply, ThreadQuery},
    views::{InputReceipt, InputReceiptState},
};

struct Fixture {
    _rt: tokio::runtime::Runtime,
    dir: tempfile::TempDir,
    store: storage::Storage,
    host: CommandHost,
}

impl Fixture {
    fn new() -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (runtime, store) = runtime_with_store(dir.path());
        let host = host_with_project(&rt, runtime, dir.path());
        let f = Self {
            _rt: rt,
            dir,
            store,
            host,
        };
        f.thread("thread-1", "run-1");
        f
    }

    fn thread(&self, thread: &str, run: &str) {
        self.store
            .handle()
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: thread.into(),
                project_id: Some(common::project().to_string()),
                project_root: Some(self.dir.path().to_string_lossy().into_owned()),
                title: Some("First".into()),
                root_run_id: Some(run.into()),
                ..Default::default()
            }])
            .unwrap();
        self.store
            .handle()
            .upsert_run_context(&storage::RunContextRecord {
                run_id: run.into(),
                role: "worker".into(),
                name: format!("chat:Worker:{thread}"),
                parent_run_id: None,
                config_json: "{}".into(),
                messages_json: "[]".into(),
                checkpoints_json: "[]".into(),
                terminal_phase: "done".into(),
                restorable: false,
                updated_at_ns: 1,
                host_inputs_json: "[]".into(),
            })
            .unwrap();
    }

    /// An accepted input on `run-1` with the edit data of `words`, as the host stores it.
    fn accept(&self, id: &str, receipt_text: &str, edit: Option<(&str, u32, u32)>) {
        let handle = self.store.handle();
        handle
            .append_host_input_accepted(
                &[],
                &[],
                None,
                &storage::HostInputReceipt {
                    provenance: storage::HostInputProvenance::User,
                    prompt_id: id.into(),
                    run_id: "run-1".into(),
                    root_run_id: "run-1".into(),
                    text: receipt_text.into(),
                    status: storage::HostInputStatus::Accepted,
                    reason: None,
                    accepted_at_ns: 1,
                    status_at_ns: 1,
                    evidence_id: None,
                },
            )
            .unwrap();
        let edit = edit.and_then(|(words, images, files)| {
            storage::HostChatSubmissionEdit::new(words, images, files)
        });
        handle
            .record_host_chat_submission(
                &storage::HostChatSubmission {
                    submission_id: id.into(),
                    fingerprint: format!("digest-{id}"),
                    run_id: "run-1".into(),
                    provenance_json: serde_json::to_string(&web()).unwrap(),
                    accepted_at_ns: 1,
                },
                edit.as_ref(),
            )
            .unwrap();
    }

    fn query(
        &self,
        query: Query,
    ) -> Result<QueryReply, workbench_protocol::errors::WorkbenchError> {
        self.host.handle_query(
            &SessionId::new("session").unwrap(),
            &query,
            &workbench_host::provenance::local_desktop(),
        )
    }

    fn receipts(&self) -> Vec<InputReceipt> {
        let reply = self
            .query(Query::Conversation(ConversationQuery::GetInputReceipts(
                ThreadQuery {
                    thread: ThreadId::new("thread-1").unwrap(),
                    page: Default::default(),
                },
            )))
            .unwrap();
        let QueryReply::Conversation(ConversationReply::InputReceipts(page)) = reply else {
            panic!()
        };
        page.items
    }

    fn receipt(&self, id: &str) -> InputReceipt {
        self.receipts()
            .into_iter()
            .find(|r| r.submission_id.as_str() == id)
            .unwrap_or_else(|| panic!("no receipt {id}"))
    }

    fn draft_in(
        &self,
        thread: &str,
        id: &str,
    ) -> Result<InputDraft, workbench_protocol::errors::WorkbenchError> {
        let reply = self.query(Query::Conversation(ConversationQuery::GetInputDraft(
            GetInputDraft {
                thread: ThreadId::new(thread).unwrap(),
                submission_id: SubmissionId::new(id).unwrap(),
            },
        )))?;
        let QueryReply::Conversation(ConversationReply::InputDraft(draft)) = reply else {
            panic!()
        };
        Ok(draft)
    }

    fn draft(&self, id: &str) -> Result<InputDraft, workbench_protocol::errors::WorkbenchError> {
        self.draft_in("thread-1", id)
    }

    fn move_to(&self, id: &str, status: storage::HostInputStatus) {
        self.store
            .handle()
            .transition_host_inputs(&[storage::HostInputTransition::new(id, status, Some("test"))])
            .unwrap();
    }

    fn sql(&self, statement: &str) {
        rusqlite::Connection::open(self.dir.path().join("host.db"))
            .unwrap()
            .execute(statement, [])
            .unwrap();
    }
}

#[test]
fn a_queued_input_has_a_preview_cut_at_a_character_and_its_words_are_read_whole() {
    let f = Fixture::new();
    let words = "あ".repeat(600);
    f.accept("sub-a", &words, Some((&words, 0, 0)));

    let receipt = f.receipt("sub-a");
    let preview = receipt.preview.expect("an accepted input shows a preview");
    assert!(preview.truncated);
    assert!(preview.text.len() <= 1024 && words.starts_with(&preview.text));
    assert_eq!(preview.text.chars().count(), 341);
    let draft = f.draft("sub-a").unwrap();
    assert_eq!(draft.text, words);
    assert_eq!(draft.state, InputReceiptState::Accepted);
    assert_eq!(draft.run.as_str(), "run-1");
}

#[test]
fn only_an_input_that_can_still_be_acted_on_shows_a_preview_or_gives_its_words() {
    let f = Fixture::new();
    for (id, status, state) in [
        ("accepted", None, InputReceiptState::Accepted),
        (
            "dropped",
            Some(storage::HostInputStatus::Dropped),
            InputReceiptState::Dropped,
        ),
        (
            "unknown",
            Some(storage::HostInputStatus::Unknown),
            InputReceiptState::Unknown,
        ),
    ] {
        f.accept(id, "hello", Some(("hello", 0, 0)));
        if let Some(status) = status {
            f.move_to(id, status);
        }
        assert_eq!(f.receipt(id).preview.unwrap().text, "hello", "{id}");
        let draft = f.draft(id).unwrap();
        assert_eq!((draft.state, draft.text.as_str()), (state, "hello"), "{id}");
    }
    for status in [
        storage::HostInputStatus::Delivered,
        storage::HostInputStatus::Withdrawn,
    ] {
        let id = status.as_str();
        f.accept(id, "hello", Some(("hello", 0, 0)));
        f.move_to(id, status);
        assert_eq!(f.receipt(id).preview, None, "{id}");
        assert_eq!(f.draft(id).unwrap_err().code, ErrorCode::Conflict, "{id}");
    }
}

#[test]
fn an_input_without_edit_data_cannot_be_edited_and_other_threads_learn_nothing() {
    let f = Fixture::new();
    f.thread("thread-2", "run-2");
    f.accept("old", "old words", None);
    f.accept("sub-a", "hello", Some(("hello", 0, 0)));

    assert_eq!(f.receipt("old").preview, None);
    assert_eq!(f.draft("old").unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(f.draft("missing").unwrap_err().code, ErrorCode::NotFound);
    // A real input asked for under another thread answers as a missing one does.
    let foreign = f.draft_in("thread-2", "sub-a").unwrap_err();
    let missing = f.draft_in("thread-2", "missing").unwrap_err();
    assert_eq!(foreign.code, ErrorCode::NotFound);
    assert_eq!(foreign.message, missing.message);
}

#[test]
fn words_beyond_the_bound_are_refused_never_cut() {
    let f = Fixture::new();
    f.accept("sub-a", "hello\n\n/files/a.png", Some(("hello", 0, 1)));
    f.sql(&format!(
        "UPDATE host_chat_submissions SET edit_original_text = '{}' WHERE submission_id = 'sub-a'",
        "x".repeat(256 * 1024 + 1)
    ));

    assert_eq!(f.draft("sub-a").unwrap_err().code, ErrorCode::LimitExceeded);
}

#[test]
fn a_deleted_thread_gives_no_preview_and_no_words() {
    let f = Fixture::new();
    f.accept("sub-a", "hello", Some(("hello", 0, 0)));
    f.sql("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES ('thread-1','family',1)");
    f.sql("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES ('run-1','thread-1','family')");

    assert_eq!(f.draft("sub-a").unwrap_err().code, ErrorCode::NotFound);
    assert!(f.receipts().is_empty());
}

fn conversation_result(reply: workbench_protocol::commands::CommandReply) -> ConversationResult {
    match reply.result.map(|result| *result) {
        Some(workbench_protocol::commands::CommandResult::Conversation(result)) => result,
        other => panic!("expected a conversation result, got {other:?}"),
    }
}

fn upload(f: &mut Fixture, name: &str, media_type: &str, bytes: &[u8]) -> AttachmentId {
    let begin = Command::Conversation(ConversationCommand::BeginAttachment {
        project: common::project(),
        thread: ThreadId::new("thread-1").unwrap(),
        client_upload_id: uuid::Uuid::new_v4().to_string(),
        filename: name.into(),
        media_type_hint: media_type.into(),
        byte_length: bytes.len() as u64,
    });
    let ConversationResult::AttachmentBegun { upload_id, .. } =
        conversation_result(f.host.handle_command(begin, web()).unwrap())
    else {
        panic!()
    };
    f.host
        .handle_command(
            Command::Conversation(ConversationCommand::AppendAttachment {
                upload_id: upload_id.clone(),
                offset: 0,
                data_base64: STANDARD.encode(bytes),
            }),
            web(),
        )
        .unwrap();
    let ConversationResult::AttachmentFinished(reference) = conversation_result(
        f.host
            .handle_command(
                Command::Conversation(ConversationCommand::FinishAttachment { upload_id }),
                web(),
            )
            .unwrap(),
    ) else {
        panic!()
    };
    reference.id
}

#[test]
fn what_a_chat_sent_with_attachments_keeps_is_its_words_and_counts_without_the_paths() {
    // (typed words, attachments as (name, media type))
    let cases: [(&str, &[(&str, &str)]); 4] = [
        ("just words", &[]),
        ("", &[("shot.png", "image/png")]),
        (
            "compare",
            &[("a.png", "image/png"), ("b.jpg", "image/jpeg")],
        ),
        (
            "see all",
            &[
                ("a.png", "image/png"),
                ("notes.txt", "text/plain"),
                ("b.webp", "image/webp"),
            ],
        ),
    ];
    let mut f = Fixture::new();
    for (index, (words, files)) in cases.iter().enumerate() {
        let id = format!("sub-{index}");
        let mut chat = send("thread-1", &id, words);
        for (name, media_type) in *files {
            chat.attachments
                .push(upload(&mut f, name, media_type, b"data"));
        }
        f.host
            .handle_command(
                Command::Conversation(ConversationCommand::SendChat(chat)),
                web(),
            )
            .unwrap();

        // The first message of a chat is delivered at once, so read the stored data
        // directly: it does not depend on the receipt's state.
        let db = storage::Database::open(&storage::StorageConfig {
            db_path: f.dir.path().join("host.db"),
            ..Default::default()
        })
        .unwrap();
        let draft = db
            .host_input_draft(&id)
            .unwrap()
            .expect("edit data was stored");
        let images = files
            .iter()
            .filter(|(_, m)| m.starts_with("image/"))
            .count();
        assert_eq!(draft.text.as_deref(), Some(*words), "{id}");
        assert_eq!(
            (draft.image_count as usize, draft.attachment_count as usize),
            (images, files.len()),
            "{id}"
        );
        // The agent's own copy has the stored paths added; the editable one never does.
        let sent = f
            .store
            .handle()
            .host_input_receipts(std::slice::from_ref(&id))
            .unwrap();
        assert_eq!(sent[0].text.contains('/'), !files.is_empty(), "{id}");
        let preview = db.host_input_previews(std::slice::from_ref(&id)).unwrap();
        assert_eq!(preview[0].preview, *words, "{id}");
        assert!(!preview[0].preview_truncated);
    }
}
