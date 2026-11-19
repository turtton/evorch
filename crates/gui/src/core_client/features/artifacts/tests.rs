//! The artifact feature against the shared actor's contract, without a core:
//! a rig renders the feature the way the connection does, answers its queries
//! and plays the data plane (claim, publish, cancel, notices) through the real
//! `DataInbox`, so what is checked is the feature's behaviour, not a copy of it.
//! Nothing waits on a clock: replies and data are delivered by the test, and a
//! decode is waited for on its own thread.
use super::*;
use crate::core_client::CoreClientScreen;
use crate::core_client::{
    cache::Cache,
    features::{
        FeatureFrame, FeatureRequest, FeatureView, Selection,
        data::{Blob, DataInbox, DataKey, DataRequest},
    },
};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use std::{collections::HashMap, io::Cursor};
use workbench_protocol::{
    errors::{ErrorCode, RetryAdvice},
    namespaces::browser::data::BlobSource,
};

fn project(name: &str) -> ProjectId {
    ProjectId::new(name).unwrap()
}

fn thread(name: &str) -> ThreadId {
    ThreadId::new(name).unwrap()
}

fn reference(thread_name: &str, n: u8) -> ArtifactRef {
    ArtifactRef {
        project: project("p"),
        thread: thread(thread_name),
        artifact_id: format!("artifact-{}", format!("{n:02x}").repeat(16)),
        digest: format!("{n:02x}").repeat(32),
    }
}

fn digest(n: u8) -> [u8; 32] {
    [n; 32]
}

fn meta(reference: &ArtifactRef, title: &str, media: &str, len: u64) -> ArtifactMetadata {
    ArtifactMetadata {
        reference: reference.clone(),
        title: title.to_owned(),
        media_type: media.to_owned(),
        byte_len: len,
    }
}

fn ok(reply: ArtifactsReply) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Artifacts(reply)))
}

fn err(error: FeatureError) -> FeatureReply {
    FeatureReply::Query(Err(error))
}

fn list(artifacts: Vec<ArtifactMetadata>, next: Option<&str>) -> FeatureReply {
    ok(ArtifactsReply::List {
        artifacts,
        next: next.map(str::to_owned),
    })
}

fn content(reference: &ArtifactRef, offset: u64, text: &str, eof: bool) -> FeatureReply {
    ok(ArtifactsReply::Content {
        artifact: reference.clone(),
        offset,
        text: text.to_owned(),
        eof,
    })
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    ::image::RgbaImage::new(width, height)
        .write_to(&mut Cursor::new(&mut bytes), ::image::ImageFormat::Png)
        .unwrap();
    bytes
}

/// What one frame sent to the actor.
#[derive(Default, Debug)]
struct Sent {
    queries: Vec<(u64, ArtifactsQuery)>,
    started: Vec<(u64, DataSource)>,
    cancelled: Vec<u64>,
    previews: Vec<ArtifactRef>,
}

struct Rig {
    feature: ArtifactsFeature,
    selection: Selection,
    cache: Cache,
    inbox: DataInbox,
    frame: FeatureFrame,
    keys: HashMap<u64, DataKey>,
}

impl Rig {
    fn new(thread_name: &str) -> Self {
        Self {
            feature: ArtifactsFeature::default(),
            selection: Selection {
                project: Some(project("p")),
                thread: Some(thread(thread_name)),
            },
            cache: Cache::default(),
            inbox: DataInbox::default(),
            frame: FeatureFrame::default(),
            keys: HashMap::new(),
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        let mut ctx = FeatureContext {
            feature: FeatureId::Artifacts,
            view: FeatureView {
                connected: true,
                capabilities: &[],
                cache: &self.cache,
                selection: &self.selection,
            },
            frame: &mut self.frame,
            data: &mut self.inbox,
        };
        self.feature.render(ui, &mut ctx);
    }

    /// The connection's side of what the feature queued.
    fn settle(&mut self) -> Sent {
        let mut sent = Sent::default();
        for action in self.frame.actions.drain(..) {
            let FeatureRequest::Query(Query::Artifacts(query)) = action.request else {
                panic!("the artifact feature only asks artifact queries");
            };
            sent.queries.push((action.tag, query));
        }
        for action in self.frame.data.drain(..) {
            match &action.request {
                DataRequest::Start(source) => {
                    let key = self.inbox.claim(&action, source.clone()).expect("claim");
                    self.keys.insert(action.tag, key);
                    sent.started.push((action.tag, source.clone()));
                }
                DataRequest::Cancel => {
                    self.keys.remove(&action.tag);
                    if self.inbox.remove(FeatureId::Artifacts, action.tag) {
                        sent.cancelled.push(action.tag);
                        // The connection says so too; the feature ignores it by tag.
                        self.feature.data(action.tag, DataNotice::Cancelled);
                    }
                }
            }
        }
        sent.previews = std::mem::take(&mut self.frame.previews);
        sent
    }

    fn publish_blob(&mut self, tag: u64, digest: [u8; 32], total: u64, bytes: Vec<u8>) {
        let start = BlobStart {
            core_epoch: 1,
            transfer_id: 1,
            source: BlobSource::Artifact,
            media_type: MediaType::Png,
            total_bytes: total,
            digest,
        };
        assert!(
            self.inbox
                .publish(self.keys[&tag], DataPayload::Blob(Blob { start, bytes }))
        );
        self.feature.data(tag, DataNotice::Ready);
    }

    fn finish_decode(&mut self, egui_ctx: &egui::Context) {
        let job = self.feature.job.take().expect("a decode is running");
        let serial = job.serial;
        let result = job.wait();
        self.feature.decoded(egui_ctx, serial, result);
    }

    fn body(&self) -> &Body {
        &self
            .feature
            .open
            .as_ref()
            .expect("an artifact is open")
            .body
    }

    fn text(&self) -> &TextBody {
        match self.body() {
            Body::Text(view) => &view.body,
            _ => panic!("not a text body"),
        }
    }
}

fn harness(rig: Rig) -> Harness<'static, Rig> {
    Harness::builder()
        .with_size(egui::vec2(900.0, 900.0))
        .build_ui_state(|ui, rig: &mut Rig| rig.show(ui), rig)
}

/// Render a few frames and hand what they sent to the connection.
fn frames(harness: &mut Harness<'static, Rig>) -> Sent {
    harness.run_steps(3);
    harness.state_mut().settle()
}

fn only_query(sent: &Sent) -> (u64, &ArtifactsQuery) {
    assert_eq!(sent.queries.len(), 1, "{sent:?}");
    (sent.queries[0].0, &sent.queries[0].1)
}

/// Click the list row titled `title` (the opened artifact repeats the title as a label).
fn click_row(harness: &mut Harness<'static, Rig>, title: &str) {
    harness.run_steps(2);
    harness
        .get_all_by_label_contains(title)
        .find(|node| node.accesskit_node().role() != egui::accesskit::Role::Label)
        .expect("a row")
        .click();
}

/// Open the row titled `title` and answer `Get` with `metadata`; returns what the next frames sent.
fn open_row(harness: &mut Harness<'static, Rig>, title: &str, metadata: &ArtifactMetadata) -> Sent {
    click_row(harness, title);
    let sent = frames(harness);
    let (tag, query) = only_query(&sent);
    assert!(matches!(query, ArtifactsQuery::Get { artifact } if *artifact == metadata.reference));
    harness
        .state_mut()
        .feature
        .reply(tag, ok(ArtifactsReply::Metadata(metadata.clone())));
    frames(harness)
}

#[test]
fn the_feature_needs_queries_and_commands() {
    assert_eq!(
        ArtifactsFeature::default().availability(),
        Availability::Implemented {
            required: &[Capability::Queries, Capability::Commands]
        }
    );
}

#[test]
fn the_selected_thread_is_listed_opened_and_read_in_utf8_chunks() {
    let note = reference("t1", 1);
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (tag, query) = only_query(&sent);
    assert_eq!(
        *query,
        ArtifactsQuery::List {
            project: project("p"),
            thread: thread("t1"),
            after: None,
            limit: LIST_PAGE
        }
    );
    // Asking again while nothing changed sends nothing.
    assert!(frames(&mut harness).queries.is_empty());
    harness.state_mut().feature.reply(
        tag,
        list(vec![meta(&note, "memo.txt", "text/plain", 20)], None),
    );
    let sent = open_row(
        &mut harness,
        "memo.txt",
        &meta(&note, "memo.txt", "text/plain", 20),
    );
    let (tag, query) = only_query(&sent);
    assert_eq!(
        *query,
        ArtifactsQuery::ReadContent {
            artifact: note.clone(),
            offset: 0,
            max_bytes: text::CHUNK_BYTES
        }
    );
    // The core cut the chunk before an incomplete character: the next read
    // starts at the bytes held (6), not at the size asked for.
    harness
        .state_mut()
        .feature
        .reply(tag, content(&note, 0, "あい", false));
    let sent = frames(&mut harness);
    let (tag, query) = only_query(&sent);
    assert!(
        matches!(query, ArtifactsQuery::ReadContent { offset: 6, .. }),
        "{query:?}"
    );
    harness
        .state_mut()
        .feature
        .reply(tag, content(&note, 6, "う\n二行目", true));
    assert!(frames(&mut harness).queries.is_empty());
    let body = harness.state().text();
    assert_eq!((body.len(), body.line_count()), (6 + 3 + 1 + 9, 2));
}

#[test]
fn the_next_page_continues_from_the_cursor_and_stops_where_the_core_does() {
    let thread_name = "t1";
    let mut harness = harness(Rig::new(thread_name));
    let (tag, _) = {
        let sent = frames(&mut harness);
        (sent.queries[0].0, ())
    };
    let first: Vec<_> = (1..=2)
        .map(|n| {
            meta(
                &reference(thread_name, n),
                &format!("one-{n}"),
                "text/plain",
                1,
            )
        })
        .collect();
    harness
        .state_mut()
        .feature
        .reply(tag, list(first, Some("cursor-1")));
    harness.run_steps(2);
    assert!(harness.query_by_label("さらに読み込む").is_some());
    harness.get_by_label("さらに読み込む").click();
    let sent = frames(&mut harness);
    let (tag, query) = only_query(&sent);
    assert!(
        matches!(query, ArtifactsQuery::List { after: Some(after), .. } if after == "cursor-1"),
        "{query:?}"
    );
    // Row 2 again (a page overlap) is not shown twice; the last page names no cursor.
    let second = vec![
        meta(&reference(thread_name, 2), "one-2", "text/plain", 1),
        meta(&reference(thread_name, 3), "one-3", "text/plain", 1),
    ];
    harness.state_mut().feature.reply(tag, list(second, None));
    harness.run_steps(2);
    assert_eq!(harness.state().feature.items.len(), 3);
    assert!(harness.query_by_label("さらに読み込む").is_none());
}

#[test]
fn a_cursor_that_does_not_move_on_ends_the_paging() {
    let mut rig = Rig::new("t1");
    rig.feature.scope = Some((project("p"), thread("t1")));
    rig.feature.list_after = Some("same".to_owned());
    rig.feature.list_tag = Some(9);
    rig.feature.reply(9, list(vec![], Some("same")));
    assert_eq!(rig.feature.next, None);
}

#[test]
fn text_is_cut_on_character_boundaries_and_gaps_are_refused() {
    let mut body = TextBody::default();
    assert_eq!(body.append(0, "あ", false), Ok(()));
    // 1 is inside the first character, and the chunk after it does not start at 3.
    assert_eq!(body.append(1, "い", false), Err(text::AppendError::Gap));
    assert_eq!(
        body.append(3, "", false),
        Err(text::AppendError::NoProgress)
    );
    assert_eq!(body.append(3, "い", false), Ok(()));
    assert!(body.wants_more() && !body.capped());
    // A long line is shortened by characters: never inside a multi-byte one.
    let mut long = TextBody::default();
    long.append(0, &"あ".repeat(text::MAX_LINE_CHARS + 5), true)
        .unwrap();
    let (line, cut) = long.line(0);
    assert!(cut);
    assert_eq!(line.chars().count(), text::MAX_LINE_CHARS);
    // The viewer's limit stops the read, one chunk may overshoot it.
    let mut big = TextBody::default();
    let chunk = "x".repeat(text::CHUNK_BYTES as usize);
    let mut offset = 0;
    while big.wants_more() {
        big.append(offset as u64, &chunk, false).unwrap();
        offset += chunk.len();
    }
    assert!(big.capped());
    assert!(big.len() < text::MAX_TEXT_BYTES + text::CHUNK_BYTES as usize);
}

#[test]
fn replies_of_another_thread_are_dropped() {
    let note = reference("t1", 1);
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (stale_list, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        stale_list,
        list(vec![meta(&note, "memo.txt", "text/plain", 20)], None),
    );
    let sent = open_row(
        &mut harness,
        "memo.txt",
        &meta(&note, "memo.txt", "text/plain", 20),
    );
    let (stale_read, _) = only_query(&sent);
    // The view moves to another thread while the read is in flight.
    harness.state_mut().selection.thread = Some(thread("t2"));
    let sent = frames(&mut harness);
    let (fresh_list, query) = only_query(&sent);
    assert!(matches!(query, ArtifactsQuery::List { thread: t, .. } if *t == thread("t2")));
    assert!(harness.state().feature.open.is_none());
    let rig = harness.state_mut();
    // Every answer of the first thread arrives late: list, get and read.
    rig.feature
        .reply(stale_read, content(&note, 0, "secret", true));
    rig.feature.reply(
        stale_list,
        list(vec![meta(&note, "memo.txt", "text/plain", 20)], None),
    );
    rig.feature.reply(
        stale_list + 1,
        ok(ArtifactsReply::Metadata(meta(
            &note,
            "memo.txt",
            "text/plain",
            20,
        ))),
    );
    assert!(rig.feature.items.is_empty() && rig.feature.open.is_none());
    // A row that names another thread is not shown in this one either.
    let foreign = reference("t1", 7);
    rig.feature.reply(
        fresh_list,
        list(vec![meta(&foreign, "foreign", "text/plain", 1)], None),
    );
    assert!(rig.feature.items.is_empty());
}

#[test]
fn a_late_data_notice_or_payload_of_the_old_selection_shows_nothing() {
    let picture = reference("t1", 1);
    let bytes = png(4, 3);
    let len = bytes.len() as u64;
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (tag, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        tag,
        list(vec![meta(&picture, "shot.png", "image/png", len)], None),
    );
    let sent = open_row(
        &mut harness,
        "shot.png",
        &meta(&picture, "shot.png", "image/png", len),
    );
    assert!(sent.queries.is_empty());
    let [(data_tag, DataSource::Artifact(asked))] = sent.started.as_slice() else {
        panic!("one artifact transfer is started: {sent:?}");
    };
    assert_eq!(*asked, picture);
    let data_tag = *data_tag;
    harness
        .state_mut()
        .feature
        .data(data_tag, DataNotice::Started);
    let old_key = harness.state().keys[&data_tag];
    harness.state_mut().selection.thread = Some(thread("t2"));
    let sent = frames(&mut harness);
    // The transfer of the old selection is cancelled and its slot is gone.
    assert_eq!(sent.cancelled, [data_tag]);
    let rig = harness.state_mut();
    assert!(!rig.inbox.publish(
        old_key,
        DataPayload::Blob(Blob {
            start: BlobStart {
                core_epoch: 1,
                transfer_id: 1,
                source: BlobSource::Artifact,
                media_type: MediaType::Png,
                total_bytes: len,
                digest: digest(1),
            },
            bytes,
        })
    ));
    rig.feature.data(data_tag, DataNotice::Ready);
    rig.feature.data(data_tag, DataNotice::Completed);
    assert!(rig.feature.open.is_none() && rig.feature.job.is_none());
}

#[test]
fn an_image_is_fetched_by_reference_decoded_and_its_slot_ends_with_the_selection() {
    let picture = reference("t1", 1);
    let bytes = png(4, 3);
    let len = bytes.len() as u64;
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (tag, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        tag,
        list(vec![meta(&picture, "shot.png", "image/png", len)], None),
    );
    let sent = open_row(
        &mut harness,
        "shot.png",
        &meta(&picture, "shot.png", "image/png", len),
    );
    let data_tag = sent.started[0].0;
    harness
        .state_mut()
        .publish_blob(data_tag, digest(1), len, bytes);
    // One frame takes the bytes and starts the decode; it cannot be polled before the next.
    harness.step();
    assert!(matches!(
        harness.state().body(),
        Body::Image(ImageView::Decoding)
    ));
    let egui_ctx = harness.ctx.clone();
    harness.state_mut().finish_decode(&egui_ctx);
    assert!(matches!(
        harness.state().body(),
        Body::Image(ImageView::Shown { size: [4, 3], .. })
    ));
    // The finished image keeps its slot, so a disconnect reaches it.
    assert_eq!(harness.state().inbox.usage().0, 1);
    harness
        .state_mut()
        .feature
        .data(data_tag, DataNotice::Completed);
    assert!(matches!(
        harness.state().body(),
        Body::Image(ImageView::Shown { .. })
    ));
    // Another thread: the texture is dropped and the slot released.
    harness.state_mut().selection.thread = Some(thread("t2"));
    let sent = frames(&mut harness);
    assert_eq!(sent.cancelled, [data_tag]);
    assert_eq!(harness.state().inbox.usage().0, 0);
    assert!(harness.state().feature.open.is_none());
}

#[test]
fn a_disconnect_drops_the_decoded_image_and_a_resync_drops_the_text() {
    let picture = reference("t1", 1);
    let bytes = png(4, 3);
    let len = bytes.len() as u64;
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (tag, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        tag,
        list(vec![meta(&picture, "shot.png", "image/png", len)], None),
    );
    let sent = open_row(
        &mut harness,
        "shot.png",
        &meta(&picture, "shot.png", "image/png", len),
    );
    let data_tag = sent.started[0].0;
    harness
        .state_mut()
        .publish_blob(data_tag, digest(1), len, bytes);
    // One frame takes the bytes and starts the decode; it cannot be polled before the next.
    harness.step();
    let egui_ctx = harness.ctx.clone();
    harness.state_mut().finish_decode(&egui_ctx);
    // The connection ends: every slot, a completed one too, is told `Cancelled`.
    harness
        .state_mut()
        .feature
        .data(data_tag, DataNotice::Cancelled);
    assert!(matches!(
        harness.state().body(),
        Body::Failed(Problem::Disconnected)
    ));

    // A text read cut by a new snapshot: what was read is gone and the late chunk is dropped.
    let note = reference("t1", 2);
    let mut rig = Rig::new("t1");
    rig.feature.scope = Some((project("p"), thread("t1")));
    rig.feature.serial = 1;
    rig.feature.open = Some(Open {
        serial: 1,
        reference: note.clone(),
        title: "memo".to_owned(),
        meta: Some(meta(&note, "memo", "text/plain", 99)),
        meta_tag: None,
        data_tag: None,
        body: Body::Text(TextView {
            body: TextBody::default(),
            read_tag: Some(5),
            problem: None,
        }),
    });
    rig.feature.reply(5, content(&note, 0, "half", false));
    assert_eq!(rig.text().len(), 4);
    rig.feature.reset();
    assert!(rig.feature.open.is_none() && rig.feature.items.is_empty());
    rig.feature.reply(5, content(&note, 4, "late", true));
    assert!(rig.feature.open.is_none());
}

#[test]
fn images_over_the_limits_are_refused_before_decoding() {
    // Over the compressed limit: no transfer is even started.
    let picture = reference("t1", 1);
    let huge = MAX_IMAGE_BLOB_BYTES + 1;
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (tag, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        tag,
        list(vec![meta(&picture, "big.png", "image/png", huge)], None),
    );
    let sent = open_row(
        &mut harness,
        "big.png",
        &meta(&picture, "big.png", "image/png", huge),
    );
    assert!(sent.started.is_empty() && sent.queries.is_empty());
    assert!(matches!(
        harness.state().body(),
        Body::Failed(Problem::TooLarge(_))
    ));

    // Small files that declare more than the decoded limits: the header decides.
    let too_wide = png(image::MAX_SIDE + 1, 1);
    assert!(matches!(
        image::decode(&too_wide, MediaType::Png),
        Err(image::ImageProblem::TooLarge(_))
    ));
    let too_many_pixels = png(4000, 3000);
    assert!(too_many_pixels.len() as u64 <= MAX_IMAGE_BLOB_BYTES);
    assert!(matches!(
        image::decode(&too_many_pixels, MediaType::Png),
        Err(image::ImageProblem::TooLarge(_))
    ));
    assert!(matches!(
        image::decode(b"not a png", MediaType::Png),
        Err(image::ImageProblem::Decode(_))
    ));
    assert_eq!(
        image::decode(&png(4, 3), MediaType::Png).unwrap().size,
        [4, 3]
    );
}

#[test]
fn bytes_that_do_not_match_the_reference_are_never_decoded_and_free_their_slot() {
    let picture = reference("t1", 1);
    let bytes = png(4, 3);
    let len = bytes.len() as u64;
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (tag, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        tag,
        list(vec![meta(&picture, "shot.png", "image/png", len)], None),
    );
    let sent = open_row(
        &mut harness,
        "shot.png",
        &meta(&picture, "shot.png", "image/png", len),
    );
    let data_tag = sent.started[0].0;
    // Another artifact's digest.
    harness
        .state_mut()
        .publish_blob(data_tag, digest(2), len, bytes);
    harness.run_steps(2);
    assert!(matches!(
        harness.state().body(),
        Body::Failed(Problem::DigestMismatch)
    ));
    assert!(harness.state().feature.job.is_none());
    let sent = frames(&mut harness);
    assert_eq!(sent.cancelled, [data_tag]);
    assert_eq!(harness.state().inbox.usage().0, 0);
}

#[test]
fn what_went_wrong_is_told_apart() {
    let busy = FeatureError::new("busy")
        .with_code(ErrorCode::Busy)
        .with_retry(RetryAdvice::SameRequest);
    let cases = [
        (Problem::from_error(&busy), true),
        (
            Problem::from_error(&FeatureError::new("no").with_code(ErrorCode::PermissionDenied)),
            false,
        ),
        (
            Problem::from_error(&FeatureError::new("gone").with_code(ErrorCode::NotFound)),
            false,
        ),
        (
            Problem::from_error(&FeatureError::new(
                "data protocol violation: record out of order: data source or control epoch mismatch",
            )),
            false,
        ),
        (
            Problem::from_error(&FeatureError::outcome_unknown("lost")),
            true,
        ),
    ];
    assert!(matches!(
        cases[0].0,
        Problem::Busy(Some(RetryAdvice::SameRequest))
    ));
    assert_eq!(cases[1].0, Problem::Denied);
    assert_eq!(cases[2].0, Problem::Gone);
    assert_eq!(cases[3].0, Problem::DigestMismatch);
    assert_eq!(cases[4].0, Problem::Disconnected);
    let mut lines: Vec<_> = cases.iter().map(|(p, _)| p.line()).collect();
    lines.sort();
    lines.dedup();
    assert_eq!(lines.len(), cases.len(), "each cause is worded on its own");
    for (problem, retry) in &cases {
        assert_eq!(problem.can_retry(), *retry, "{problem:?}");
    }
    assert!(
        !Problem::from_error(&busy.clone().with_retry(RetryAdvice::Never)).can_retry(),
        "the core said never"
    );
}
#[test]
fn a_refused_get_and_a_reply_for_another_digest_are_not_shown_as_content() {
    let note = reference("t1", 1);
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (tag, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        tag,
        list(vec![meta(&note, "memo.txt", "text/plain", 20)], None),
    );
    harness.run_steps(2);
    click_row(&mut harness, "memo.txt");
    let sent = frames(&mut harness);
    let (get, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        get,
        err(FeatureError::new("not found").with_code(ErrorCode::NotFound)),
    );
    assert!(matches!(
        harness.state().body(),
        Body::Failed(Problem::Gone)
    ));

    // The core answers `Get` with the metadata of another digest.
    let mut other = note.clone();
    other.digest = format!("{:02x}", 9).repeat(32);
    click_row(&mut harness, "memo.txt");
    let sent = frames(&mut harness);
    let (get, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        get,
        ok(ArtifactsReply::Metadata(meta(
            &other,
            "memo.txt",
            "text/plain",
            20,
        ))),
    );
    assert!(matches!(
        harness.state().body(),
        Body::Failed(Problem::DigestMismatch)
    ));

    // A chunk that names another artifact is dropped with the text read so far.
    let sent = open_row(
        &mut harness,
        "memo.txt",
        &meta(&note, "memo.txt", "text/plain", 20),
    );
    let (read, _) = only_query(&sent);
    harness
        .state_mut()
        .feature
        .reply(read, content(&other, 0, "not yours", true));
    assert!(matches!(
        harness.state().body(),
        Body::Failed(Problem::DigestMismatch)
    ));
}

#[test]
fn html_is_shown_as_source_and_its_preview_is_a_reference_for_the_browser_feature() {
    let page = reference("t1", 1);
    let note = reference("t1", 2);
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (tag, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        tag,
        list(
            vec![
                meta(&page, "page.html", "text/html", 40),
                meta(&note, "note.txt", "text/plain", 5),
            ],
            None,
        ),
    );
    harness.run_steps(2);
    let sent = open_row(
        &mut harness,
        "page.html",
        &meta(&page, "page.html", "text/html", 40),
    );
    let (read, _) = only_query(&sent);
    let source = "<script>alert(1)</script>";
    harness
        .state_mut()
        .feature
        .reply(read, content(&page, 0, source, true));
    let sent = frames(&mut harness);
    assert!(sent.queries.is_empty() && sent.started.is_empty() && sent.previews.is_empty());
    assert_eq!(harness.state().text().line(0).0, source);
    harness.get_by_label("プレビューを開く").click();
    let sent = frames(&mut harness);
    assert_eq!(sent.previews, [page]);
    assert!(sent.queries.is_empty() && sent.started.is_empty());

    // Plain text has no preview.
    click_row(&mut harness, "note.txt");
    let sent = open_row_after_click(&mut harness, &meta(&note, "note.txt", "text/plain", 5));
    assert!(sent.previews.is_empty());
    harness.run_steps(2);
    assert!(harness.query_by_label("プレビューを開く").is_none());
}

/// The `Get` of a row that was just clicked, answered.
fn open_row_after_click(harness: &mut Harness<'static, Rig>, metadata: &ArtifactMetadata) -> Sent {
    let sent = frames(harness);
    let _ = sent;
    let get = harness
        .state()
        .feature
        .open
        .as_ref()
        .and_then(|o| o.meta_tag)
        .expect("a Get is in flight");
    harness
        .state_mut()
        .feature
        .reply(get, ok(ArtifactsReply::Metadata(metadata.clone())));
    frames(harness)
}

#[test]
fn a_connection_lost_while_an_image_is_received_leaves_nothing_to_show() {
    let picture = reference("t1", 1);
    let bytes = png(4, 3);
    let len = bytes.len() as u64;
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (tag, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        tag,
        list(vec![meta(&picture, "shot.png", "image/png", len)], None),
    );
    let sent = open_row(
        &mut harness,
        "shot.png",
        &meta(&picture, "shot.png", "image/png", len),
    );
    let data_tag = sent.started[0].0;
    harness
        .state_mut()
        .feature
        .data(data_tag, DataNotice::Started);
    // What the actor does when the connection ends: every slot is dropped and told so.
    let rig = harness.state_mut();
    rig.inbox.reset(FeatureId::Artifacts);
    rig.feature.data(data_tag, DataNotice::Cancelled);
    assert!(matches!(rig.body(), Body::Failed(Problem::Disconnected)));
    assert!(rig.feature.job.is_none() && rig.feature.open.as_ref().unwrap().data_tag.is_none());
    // The slot is gone, so a frame after it finds no payload and starts nothing.
    let sent = frames(&mut harness);
    assert!(sent.started.is_empty() && sent.cancelled.is_empty());
    // A refusal of the bind (the core is busy) is a different message.
    let rig = harness.state_mut();
    rig.feature.open.as_mut().unwrap().data_tag = Some(77);
    rig.feature.data(
        77,
        DataNotice::Failed(FeatureError::new("busy").with_code(ErrorCode::Busy)),
    );
    assert!(matches!(rig.body(), Body::Failed(Problem::Busy(_))));
}

/// A 1×1 PNG whose compressed ICC profile expands to `icc_bytes`: the decoder
/// expands it while it is created, long before any pixel.
fn png_with_icc(icc_bytes: usize) -> Vec<u8> {
    use ::image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
    let mut bytes = Vec::new();
    let mut encoder = PngEncoder::new(&mut bytes);
    encoder.set_icc_profile(vec![0; icc_bytes]).unwrap();
    encoder
        .write_image(&[0, 0, 0, 0], 1, 1, ExtendedColorType::Rgba8)
        .unwrap();
    bytes
}

#[test]
fn compressed_metadata_never_reaches_the_decoder_so_it_allocates_nothing_large() {
    // The profile is 48 MiB once expanded, a few KiB on the wire. Decoding it as
    // is allocates it in one piece while the decoder is created.
    let bomb = png_with_icc(48 * 1024 * 1024);
    assert!(bomb.len() < 256 * 1024, "{}", bomb.len());
    let stripped = image::strip_png(&bomb).unwrap();
    assert!(
        !stripped.windows(4).any(|w| w == b"iCCP"),
        "the profile is dropped before a decoder exists"
    );
    let (decoded, largest) =
        alloc_probe::largest_allocation(|| image::decode(&bomb, MediaType::Png));
    assert_eq!(decoded.unwrap().size, [1, 1]);
    assert!(
        largest < 1024 * 1024,
        "the largest single allocation while decoding was {largest} bytes"
    );
    // A broken chunk stream is refused, not guessed at.
    assert!(image::strip_png(b"not a png at all").is_err());
    assert!(image::strip_png(&bomb[..bomb.len() / 2]).is_err());
}

/// Counts the largest single allocation of the calling thread while armed: the
/// decode under test runs on it.
mod alloc_probe {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static LARGEST: Cell<usize> = const { Cell::new(0) };
    }

    struct Probe;

    fn note(size: usize) {
        let _ = ARMED.try_with(|armed| {
            if armed.get() {
                let _ = LARGEST.try_with(|largest| largest.set(largest.get().max(size)));
            }
        });
    }

    // SAFETY: every call is forwarded to `System` unchanged; only a size is noted.
    unsafe impl GlobalAlloc for Probe {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note(layout.size());
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note(new_size);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static PROBE: Probe = Probe;

    pub(super) fn largest_allocation<T>(run: impl FnOnce() -> T) -> (T, usize) {
        LARGEST.with(|largest| largest.set(0));
        ARMED.with(|armed| armed.set(true));
        let result = run();
        ARMED.with(|armed| armed.set(false));
        (result, LARGEST.with(Cell::get))
    }
}

#[test]
fn a_decode_that_is_abandoned_leaves_no_image_behind_and_blocks_no_new_one() {
    let picture = reference("t1", 1);
    let bytes = png(4, 3);
    let len = bytes.len() as u64;
    let mut harness = harness(Rig::new("t1"));
    let sent = frames(&mut harness);
    let (tag, _) = only_query(&sent);
    harness.state_mut().feature.reply(
        tag,
        list(vec![meta(&picture, "shot.png", "image/png", len)], None),
    );
    let sent = open_row(
        &mut harness,
        "shot.png",
        &meta(&picture, "shot.png", "image/png", len),
    );
    let data_tag = sent.started[0].0;
    harness
        .state_mut()
        .publish_blob(data_tag, digest(1), len, bytes);
    harness.step();
    assert!(matches!(
        harness.state().body(),
        Body::Image(ImageView::Decoding)
    ));
    // The connection is lost while the decode runs.
    harness.state_mut().inbox.reset(FeatureId::Artifacts);
    harness
        .state_mut()
        .feature
        .data(data_tag, DataNotice::Cancelled);
    let job = harness
        .state_mut()
        .feature
        .job
        .as_mut()
        .expect("still running or done");
    assert!(job.abandoned(), "nobody waits for its result");
    // The thread ends by itself; its result is dropped, not left in a channel.
    job.join();
    harness.step();
    assert!(
        harness.state().feature.job.is_none(),
        "the next decode may start"
    );
    assert!(matches!(
        harness.state().body(),
        Body::Failed(Problem::Disconnected)
    ));
}

/// The feature as the production composition holds it, with a handle on it so a
/// test can look at what it keeps. Every hook is forwarded unchanged.
struct Held(std::sync::Arc<std::sync::Mutex<ArtifactsFeature>>);

impl Held {
    fn feature(&self) -> std::sync::MutexGuard<'_, ArtifactsFeature> {
        self.0.lock().unwrap()
    }
}

impl Feature for Held {
    fn id(&self) -> FeatureId {
        FeatureId::Artifacts
    }
    fn title(&self) -> &'static str {
        "Artifacts"
    }
    fn availability(&self) -> Availability {
        self.feature().availability()
    }
    fn reset(&mut self) {
        self.feature().reset();
    }
    fn disconnected(&mut self) {
        self.feature().disconnected();
    }
    fn selection_changed(&mut self, ctx: &mut FeatureContext<'_>) {
        self.feature().selection_changed(ctx);
    }
    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        self.feature().maintain(ctx, visibility);
    }
    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        self.feature().reply(tag, reply);
    }
    fn data(&mut self, tag: u64, notice: DataNotice) {
        self.feature().data(tag, notice);
    }
    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        self.feature().render(ui, ctx);
    }
}

type Wire = tokio::sync::mpsc::Receiver<crate::core_client::connection::Action>;

/// The screen with the real `Features` around the artifact feature, selecting
/// `t1`, connected with the capabilities the feature needs.
fn composition() -> (
    CoreClientScreen,
    std::sync::Arc<std::sync::Mutex<crate::core_client::Shared>>,
    std::sync::Arc<std::sync::Mutex<ArtifactsFeature>>,
    Wire,
) {
    use crate::core_client::features::Features;
    let feature = std::sync::Arc::new(std::sync::Mutex::new(ArtifactsFeature::default()));
    let shared = std::sync::Arc::new(std::sync::Mutex::new(crate::core_client::Shared {
        connected: true,
        capabilities: vec![Capability::Queries, Capability::Commands],
        features: Features {
            features: vec![Box::new(Held(feature.clone()))],
            data: DataInbox::default(),
            selection: None,
            expanded: Default::default(),
        },
        ..crate::core_client::Shared::default()
    }));
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let mut screen = CoreClientScreen::new(shared.clone(), tx, None, None);
    screen.project = Some(project("p"));
    screen.selected = Some(thread("t1"));
    (screen, shared, feature, rx)
}

fn compose(
    ctx: &egui::Context,
    screen: &mut CoreClientScreen,
    events: Vec<egui::Event>,
) -> (Vec<egui::epaint::ClippedShape>, Vec<egui::TextureId>) {
    let input = egui::RawInput {
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| {
        screen.logic(ui.ctx());
        screen.ui(ui);
    });
    let freed = output.textures_delta.free.iter().copied().collect();
    output.textures_delta.clear();
    (output.shapes, freed)
}

/// Click the title of the feature's header, which opens or closes it.
fn toggle_header(ctx: &egui::Context, screen: &mut CoreClientScreen) {
    fn find(shape: &egui::Shape) -> Option<egui::Pos2> {
        match shape {
            egui::Shape::Text(text) if text.galley.text() == "Artifacts" => {
                Some(text.pos + text.galley.rect.center().to_vec2())
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(find),
            _ => None,
        }
    }
    let pos = compose(ctx, screen, vec![])
        .0
        .iter()
        .find_map(|s| find(&s.shape))
        .expect("the header title was drawn");
    for pressed in [true, false] {
        compose(
            ctx,
            screen,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        );
    }
}

fn data_cancels(wire: &mut Wire) -> Vec<u64> {
    let mut tags = Vec::new();
    while let Ok(action) = wire.try_recv() {
        if let crate::core_client::connection::Action::Data(action) = action {
            assert!(matches!(action.request, DataRequest::Cancel));
            tags.push(action.tag);
        }
    }
    tags
}

/// An opened artifact as a frame leaves it: `body` shown, `data_tag` its data slot.
fn opened(note: &ArtifactRef, body: Body, data_tag: Option<u64>) -> Open {
    Open {
        serial: 1,
        reference: note.clone(),
        title: "memo".to_owned(),
        meta: Some(meta(note, "memo", "text/plain", 99)),
        meta_tag: None,
        data_tag,
        body,
    }
}

fn text_view(read_tag: Option<u64>) -> Body {
    Body::Text(TextView {
        body: TextBody::default(),
        read_tag,
        problem: None,
    })
}

#[test]
fn a_selection_change_drops_the_body_texture_and_slot_while_the_panel_is_collapsed() {
    let (mut screen, _shared, feature, mut wire) = composition();
    let ctx = egui::Context::default();
    compose(&ctx, &mut screen, vec![]);
    assert_eq!(
        feature.lock().unwrap().scope,
        Some((project("p"), thread("t1"))),
        "the first pass follows the selection"
    );
    toggle_header(&ctx, &mut screen);
    while wire.try_recv().is_ok() {}
    // Something is open for t1: a picture on screen, its slot held.
    let picture = reference("t1", 1);
    let texture = ctx.load_texture("shot", egui::ColorImage::example(), Default::default());
    let texture_id = texture.id();
    feature.lock().unwrap().open = Some(opened(
        &picture,
        Body::Image(ImageView::Shown {
            texture,
            size: [1, 1],
        }),
        Some(7),
    ));
    compose(&ctx, &mut screen, vec![]);
    assert!(
        feature.lock().unwrap().open.is_some(),
        "the same selection keeps what is open"
    );
    assert!(data_cancels(&mut wire).is_empty());
    // Another thread is selected while the body is collapsed (never drawn).
    screen.selected = Some(thread("t2"));
    let (_, freed) = compose(&ctx, &mut screen, vec![]);
    let held = feature.lock().unwrap();
    assert!(held.open.is_none(), "the old body is gone");
    assert_eq!(held.scope, Some((project("p"), thread("t2"))));
    drop(held);
    assert!(
        freed.contains(&texture_id),
        "the texture is freed in the same pass"
    );
    assert_eq!(
        data_cancels(&mut wire),
        [7],
        "the slot is released at the actor"
    );
}

#[test]
fn a_disconnect_drops_the_text_at_once_and_a_late_chunk_is_not_shown() {
    let (mut screen, shared, feature, mut wire) = composition();
    let ctx = egui::Context::default();
    compose(&ctx, &mut screen, vec![]);
    let note = reference("t1", 2);
    feature.lock().unwrap().open = Some(opened(&note, text_view(Some(5)), None));
    feature.lock().unwrap().serial = 1;
    feature
        .lock()
        .unwrap()
        .reply(5, content(&note, 0, "half", false));
    assert!(feature.lock().unwrap().open.is_some());
    while wire.try_recv().is_ok() {}
    // No composition runs between the loss and the check.
    shared.lock().unwrap().disconnect();
    let mut held = feature.lock().unwrap();
    assert!(
        held.open.is_none(),
        "the text is dropped when the connection is lost"
    );
    assert!(held.items.is_empty() && held.list.is_none());
    held.reply(5, content(&note, 4, "late", true));
    assert!(
        held.open.is_none(),
        "a reply that arrives later finds nothing to show"
    );
    drop(held);
    // While disconnected the feature is still maintained, and nothing is sent for the dropped slot.
    compose(&ctx, &mut screen, vec![]);
    assert!(feature.lock().unwrap().open.is_none());
    assert!(data_cancels(&mut wire).is_empty());
}
