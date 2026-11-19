//! Desktop feature of the `artifacts` area (Stage 16b, PR A): the artifacts of
//! the thread the conversation view shows, one opened for its text, its HTML
//! source or its image.
//!
//! * **List / Get.** The feature lists the selected project's and thread's
//!   artifacts page by page (a page is one query, the next page is asked for
//!   with the cursor of the last, up to [`MAX_LISTED`] rows) and asks `Get` for
//!   the artifact that is opened, so what is shown is what the core confirms for
//!   the reference it was given.
//! * **Text and HTML.** Read through `ReadContent` in the core's UTF-8 chunks
//!   (`text`), at most [`text::MAX_TEXT_BYTES`] (+ one chunk) are kept. HTML is
//!   shown as source only; nothing here runs it. Its preview is a reference
//!   handed to the browser feature ([`FeatureContext::open_preview`]).
//! * **Images.** Fetched with `start_data(DataSource::Artifact)` / `take_data`:
//!   the GUI never opens a stored path. The compressed size is bounded by the
//!   data plane; the decoded size by [`image`] (header check, allocation limit,
//!   one decode at a time, on a thread of its own). One finished image keeps its
//!   data slot until another artifact is opened or the selection changes, so a
//!   disconnect reaches it as `Cancelled` and drops decoded state and texture.
//! * **What is dropped.** Every reply is matched by the tag of the request the
//!   feature currently waits for, and every request is forgotten when the
//!   selection changes ([`ArtifactsFeature::leave_scope`]) or a new snapshot
//!   replaces the projection ([`Feature::reset`]): a late reply for another
//!   thread, another artifact or an earlier read finds no tag and is dropped.
//! * **Why it failed** is told apart ([`problem::Problem`]): busy, denied,
//!   gone (the core answers deleted and wrong digest alike), a digest mismatch
//!   detected here, too large, unsupported, disconnected.
//!
//! Nothing is retried or reconnected by the feature: a button asks again.
mod image;
mod problem;
mod text;

use super::data::{DataNotice, DataPayload, DataSource};
use super::layout::paged_list;
use super::{Availability, Feature, FeatureContext, FeatureError, FeatureId, FeatureReply};
use problem::Problem;
use text::TextBody;
use workbench_protocol::{
    handshake::Capability,
    ids::{ProjectId, ThreadId},
    namespaces::{
        artifacts::{ArtifactMetadata, ArtifactRef, ArtifactsQuery, ArtifactsReply},
        browser::data::{BlobStart, MAX_IMAGE_BLOB_BYTES, MediaType},
    },
    queries::{Query, QueryReply},
};

/// Rows asked for per page (the protocol allows up to `MAX_ARTIFACT_PAGE`).
const LIST_PAGE: u32 = 32;
/// Rows held for a thread; more are not asked for.
const MAX_LISTED: usize = 512;
const TEXT_VIEW_HEIGHT: f32 = 320.0;
const IMAGE_VIEW_HEIGHT: f32 = 480.0;

enum ListState {
    Loading,
    Ready,
    Failed(Problem),
}

/// What the opened artifact's body is.
enum Body {
    /// `Get` is in flight.
    Waiting,
    Text(TextView),
    Image(ImageView),
    Failed(Problem),
}

struct TextView {
    body: TextBody,
    /// The `ReadContent` in flight.
    read_tag: Option<u64>,
    /// A later chunk failed: what was read stays, reading stops.
    problem: Option<Problem>,
}

enum ImageView {
    /// Confirmed as an image: the data request is yet to be made.
    ToStart,
    /// Requested; the bytes are taken once the connection says they are ready.
    Receiving,
    Decoding,
    Shown {
        texture: egui::TextureHandle,
        size: [usize; 2],
    },
}

struct Open {
    /// Identifies this opening; a decode result for another one is dropped.
    serial: u64,
    reference: ArtifactRef,
    title: String,
    meta: Option<ArtifactMetadata>,
    meta_tag: Option<u64>,
    /// The data request (and, once complete, its held slot) of the image.
    data_tag: Option<u64>,
    body: Body,
}

#[derive(Default)]
pub struct ArtifactsFeature {
    /// Tags of queries and data requests alike; never reused, not even across reset.
    next_tag: u64,
    serial: u64,
    scope: Option<(ProjectId, ThreadId)>,
    list: Option<ListState>,
    list_tag: Option<u64>,
    list_after: Option<String>,
    items: Vec<ArtifactMetadata>,
    next: Option<String>,
    open: Option<Open>,
    /// Data slots to release at the next chance (a finished or abandoned image).
    to_cancel: Vec<u64>,
    /// At most one decode runs at a time, whichever opening it was for.
    job: Option<image::Job>,
}

enum Click {
    Retry,
    Preview,
}

impl ArtifactsFeature {
    fn next_tag(&mut self) -> u64 {
        self.next_tag += 1;
        self.next_tag
    }

    fn query(&mut self, ctx: &mut FeatureContext<'_>, query: ArtifactsQuery) -> u64 {
        let tag = self.next_tag();
        ctx.query(tag, Query::Artifacts(query));
        tag
    }

    /// Forget everything shown for the thread that was selected: its rows, the
    /// opened artifact with its text, image and texture, and what was asked.
    fn leave_scope(&mut self) {
        self.close_open();
        self.list = None;
        self.list_tag = None;
        self.list_after = None;
        self.items.clear();
        self.next = None;
    }

    fn close_open(&mut self) {
        if let Some(open) = self.open.take() {
            self.abandon_decode(open.serial);
            if let Some(tag) = open.data_tag {
                self.to_cancel.push(tag);
            }
        }
    }

    /// Nobody waits for the decode of this opening any more.
    fn abandon_decode(&mut self, serial: u64) {
        if let Some(job) = self.job.as_mut().filter(|job| job.serial == serial) {
            job.abandon();
        }
    }

    fn request_list(&mut self, ctx: &mut FeatureContext<'_>, append: bool) {
        let Some((project, thread)) = self.scope.clone() else {
            return;
        };
        let after = if append { self.next.clone() } else { None };
        if !append {
            self.items.clear();
            self.next = None;
        }
        self.list_after = after.clone();
        let tag = self.query(
            ctx,
            ArtifactsQuery::List {
                project,
                thread,
                after,
                limit: LIST_PAGE,
            },
        );
        self.list_tag = Some(tag);
        self.list = Some(ListState::Loading);
    }

    fn open_artifact(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        reference: ArtifactRef,
        title: String,
    ) {
        self.close_open();
        self.serial += 1;
        let tag = self.query(
            ctx,
            ArtifactsQuery::Get {
                artifact: reference.clone(),
            },
        );
        self.open = Some(Open {
            serial: self.serial,
            reference,
            title,
            meta: None,
            meta_tag: Some(tag),
            data_tag: None,
            body: Body::Waiting,
        });
    }

    fn list_arrived(&mut self, result: Result<QueryReply, FeatureError>) {
        let Some(scope) = self.scope.clone() else {
            return;
        };
        self.list = Some(match result {
            Err(error) => ListState::Failed(Problem::from_error(&error)),
            Ok(QueryReply::Artifacts(ArtifactsReply::List { artifacts, next })) => {
                for item in artifacts {
                    let ours =
                        item.reference.project == scope.0 && item.reference.thread == scope.1;
                    let known = self
                        .items
                        .iter()
                        .any(|held| held.reference.artifact_id == item.reference.artifact_id);
                    if ours && !known && self.items.len() < MAX_LISTED {
                        self.items.push(item);
                    }
                }
                // A cursor that does not move on would page forever.
                self.next = next.filter(|n| {
                    self.items.len() < MAX_LISTED && self.list_after.as_ref() != Some(n)
                });
                ListState::Ready
            }
            Ok(_) => ListState::Failed(Problem::Other("想定外の応答です".to_owned())),
        });
    }

    fn meta_arrived(&mut self, result: Result<QueryReply, FeatureError>) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        open.meta_tag = None;
        open.body = match result {
            Err(error) => Body::Failed(Problem::from_error(&error)),
            Ok(QueryReply::Artifacts(ArtifactsReply::Metadata(meta))) => {
                if meta.reference != open.reference {
                    Body::Failed(Problem::DigestMismatch)
                } else {
                    let body = body_for(&meta);
                    open.meta = Some(meta);
                    body
                }
            }
            Ok(_) => Body::Failed(Problem::Other("想定外の応答です".to_owned())),
        };
    }

    fn chunk_arrived(&mut self, result: Result<QueryReply, FeatureError>) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let Body::Text(view) = &mut open.body else {
            return;
        };
        view.read_tag = None;
        match result {
            Err(error) => view.problem = Some(Problem::from_error(&error)),
            Ok(QueryReply::Artifacts(ArtifactsReply::Content {
                artifact,
                offset,
                text,
                eof,
            })) => {
                if artifact != open.reference {
                    open.body = Body::Failed(Problem::DigestMismatch);
                    return;
                }
                if let Err(error) = view.body.append(offset, &text, eof) {
                    let why = match error {
                        text::AppendError::Gap => "読み取り位置が一致しません",
                        text::AppendError::NoProgress => "読み取りが進みません",
                    };
                    view.problem = Some(Problem::Other(why.to_owned()));
                }
            }
            Ok(_) => view.problem = Some(Problem::Other("想定外の応答です".to_owned())),
        }
    }

    /// A finished decode, for the opening it was started for and only if that
    /// opening still waits for it.
    fn decoded(
        &mut self,
        egui_ctx: &egui::Context,
        serial: u64,
        result: Result<egui::ColorImage, image::ImageProblem>,
    ) {
        let Some(open) = self
            .open
            .as_mut()
            .filter(|o| o.serial == serial && matches!(o.body, Body::Image(ImageView::Decoding)))
        else {
            return;
        };
        match result {
            Ok(color) => {
                let size = color.size;
                let texture = egui_ctx.load_texture(
                    format!("artifact-{serial}"),
                    color,
                    egui::TextureOptions::LINEAR,
                );
                open.body = Body::Image(ImageView::Shown { texture, size });
            }
            Err(failure) => {
                if let Some(tag) = open.data_tag.take() {
                    self.to_cancel.push(tag);
                }
                open.body = Body::Failed(match failure {
                    image::ImageProblem::TooLarge(why) => Problem::TooLarge(why),
                    image::ImageProblem::Decode(why) => {
                        Problem::Other(format!("画像を復号できません: {why}"))
                    }
                });
            }
        }
    }

    fn flush(&mut self, ctx: &mut FeatureContext<'_>) {
        for tag in self.to_cancel.drain(..) {
            ctx.cancel_data(tag);
        }
    }

    /// The work a frame owes the opened artifact: the next chunk, the data
    /// request, the bytes that arrived, the decode that finished.
    fn pump(&mut self, egui_ctx: &egui::Context, ctx: &mut FeatureContext<'_>) {
        if let Some(job) = &self.job {
            match job.poll() {
                Some(result) => {
                    let serial = job.serial;
                    self.job = None;
                    self.decoded(egui_ctx, serial, result);
                }
                // An abandoned decode that has ended frees the way for the next one.
                None if job.abandoned() && job.finished() => self.job = None,
                None => {}
            }
        }
        enum Step {
            Read(u64),
            Start,
            Take(u64),
        }
        let step = match self.open.as_ref() {
            Some(Open {
                reference,
                body: Body::Text(view),
                ..
            }) if view.read_tag.is_none() && view.problem.is_none() && view.body.wants_more() => {
                Some((Step::Read(view.body.len() as u64), Some(reference.clone())))
            }
            Some(Open {
                body: Body::Image(ImageView::ToStart),
                ..
            }) => Some((Step::Start, None)),
            Some(Open {
                data_tag: Some(tag),
                body: Body::Image(ImageView::Receiving),
                ..
            }) if self.job.is_none() => Some((Step::Take(*tag), None)),
            _ => None,
        };
        match step {
            Some((Step::Read(offset), Some(artifact))) => {
                let tag = self.query(
                    ctx,
                    ArtifactsQuery::ReadContent {
                        artifact,
                        offset,
                        max_bytes: text::CHUNK_BYTES,
                    },
                );
                if let Some(Open {
                    body: Body::Text(view),
                    ..
                }) = self.open.as_mut()
                {
                    view.read_tag = Some(tag);
                }
            }
            Some((Step::Start, _)) => {
                let tag = self.next_tag();
                if let Some(open) = self.open.as_mut() {
                    ctx.start_data(tag, DataSource::Artifact(open.reference.clone()));
                    open.data_tag = Some(tag);
                    open.body = Body::Image(ImageView::Receiving);
                }
            }
            Some((Step::Take(tag), _)) => {
                let Some(payload) = ctx.take_data(tag) else {
                    return;
                };
                let Some(open) = self.open.as_mut() else {
                    return;
                };
                match accept_blob(payload, &open.reference, open.meta.as_ref()) {
                    Ok((bytes, media)) => {
                        self.job = Some(image::Job::spawn(
                            open.serial,
                            bytes,
                            media,
                            egui_ctx.clone(),
                        ));
                        open.body = Body::Image(ImageView::Decoding);
                    }
                    Err(problem) => {
                        open.data_tag = None;
                        open.body = Body::Failed(problem);
                        self.to_cancel.push(tag);
                    }
                }
            }
            _ => {}
        }
    }

    fn draw_list(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let mut reload = false;
        ui.horizontal(|ui| {
            ui.label(format!("成果物（{} 件）", self.items.len()));
            reload = ui.button("再読み込み").clicked();
        });
        match &self.list {
            Some(ListState::Loading) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("読み込み中…");
                });
            }
            Some(ListState::Failed(problem)) => {
                ui.colored_label(ui.visuals().error_fg_color, problem.line());
            }
            Some(ListState::Ready) if self.items.is_empty() => {
                ui.weak("このスレッドの成果物はありません。");
            }
            _ => {}
        }
        let open_id = self.open.as_ref().map(|o| o.reference.artifact_id.clone());
        let can_page = matches!(self.list, Some(ListState::Ready));
        let mut chosen = None;
        let more = paged_list(
            ui,
            "artifacts-list",
            self.next
                .as_deref()
                .filter(|_| can_page)
                .map(|_| "さらに読み込む"),
            |ui| {
                for item in &self.items {
                    let selected = open_id.as_deref() == Some(item.reference.artifact_id.as_str());
                    let row = format!("{}  ({}, {} B)", item.title, item.media_type, item.byte_len);
                    if ui.selectable_label(selected, row).clicked() {
                        chosen = Some(item.clone());
                    }
                }
            },
        );
        if self.items.len() >= MAX_LISTED && self.next.is_none() && can_page {
            ui.weak(format!("先頭 {MAX_LISTED} 件までを表示しています。"));
        }
        if reload {
            self.request_list(ctx, false);
        } else if more {
            self.request_list(ctx, true);
        }
        if let Some(item) = chosen {
            self.open_artifact(ctx, item.reference, item.title);
        }
    }

    fn draw_open(&mut self, ui: &mut egui::Ui) -> Option<Click> {
        let Some(open) = self.open.as_mut() else {
            ui.weak("成果物を選ぶと内容を表示します。");
            return None;
        };
        let title = open
            .meta
            .as_ref()
            .map_or(open.title.as_str(), |m| m.title.as_str());
        ui.strong(title);
        let mut click = None;
        let error_color = ui.visuals().error_fg_color;
        let html = open
            .meta
            .as_ref()
            .is_some_and(|m| m.media_type == "text/html");
        match &open.body {
            Body::Waiting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("確認中…");
                });
            }
            Body::Failed(problem) => {
                ui.colored_label(error_color, problem.line());
                if problem.can_retry() && ui.button("再試行").clicked() {
                    click = Some(Click::Retry);
                }
            }
            Body::Text(view) => {
                if html {
                    ui.horizontal(|ui| {
                        ui.label("HTML のソースを表示しています（ここでは実行しません）。");
                        if ui.button("プレビューを開く").clicked() {
                            click = Some(Click::Preview);
                        }
                    });
                }
                draw_text(ui, &view.body);
                if view.read_tag.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("読み込み中…");
                    });
                }
                if view.body.capped() {
                    ui.weak(format!(
                        "先頭 {} KiB までを表示しています。",
                        text::MAX_TEXT_BYTES / 1024
                    ));
                }
                if let Some(problem) = &view.problem {
                    ui.colored_label(error_color, problem.line());
                    if problem.can_retry() && ui.button("再試行").clicked() {
                        click = Some(Click::Retry);
                    }
                }
            }
            Body::Image(ImageView::Shown { texture, size }) => {
                let source = egui::load::SizedTexture::new(
                    texture.id(),
                    egui::vec2(size[0] as f32, size[1] as f32),
                );
                ui.add(
                    egui::Image::new(source)
                        .max_size(egui::vec2(ui.available_width(), IMAGE_VIEW_HEIGHT))
                        .maintain_aspect_ratio(true),
                );
            }
            Body::Image(ImageView::Decoding) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("画像を復号中…");
                });
            }
            Body::Image(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("画像を受信中…");
                });
            }
        }
        click
    }

    fn apply(&mut self, ctx: &mut FeatureContext<'_>, click: Click) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        match click {
            Click::Retry => {
                let (reference, title) = (open.reference.clone(), open.title.clone());
                self.open_artifact(ctx, reference, title);
            }
            Click::Preview => {
                // Only a reference the core confirmed for this thread leaves.
                let confirmed = open
                    .meta
                    .as_ref()
                    .is_some_and(|m| m.reference == open.reference);
                let ours = self.scope.as_ref().is_some_and(|(p, t)| {
                    &open.reference.project == p && &open.reference.thread == t
                });
                if confirmed && ours && open.reference.validate().is_ok() {
                    ctx.open_preview(open.reference.clone());
                }
            }
        }
    }
}

/// The body an artifact of this kind gets once the core confirmed its metadata.
fn body_for(meta: &ArtifactMetadata) -> Body {
    match meta.media_type.as_str() {
        "text/html" | "text/plain" => Body::Text(TextView {
            body: TextBody::default(),
            read_tag: None,
            problem: None,
        }),
        other => match MediaType::from_mime(other) {
            Some(_) if meta.byte_len == 0 || meta.byte_len > MAX_IMAGE_BLOB_BYTES => {
                Body::Failed(Problem::TooLarge(format!(
                    "{} バイト（上限 {} MiB）",
                    meta.byte_len,
                    MAX_IMAGE_BLOB_BYTES / (1024 * 1024)
                )))
            }
            Some(_) => Body::Image(ImageView::ToStart),
            None => Body::Failed(Problem::Unsupported(other.to_owned())),
        },
    }
}

/// The compressed bytes of what arrived for the image asked for: they must be a
/// blob of that artifact (digest, size, type) or nothing is decoded.
fn accept_blob(
    payload: DataPayload,
    reference: &ArtifactRef,
    meta: Option<&ArtifactMetadata>,
) -> Result<(Vec<u8>, MediaType), Problem> {
    let DataPayload::Blob(blob) = payload else {
        return Err(Problem::Other("想定外のデータです".to_owned()));
    };
    let digest =
        BlobStart::artifact_digest(&reference.digest).map_err(|_| Problem::DigestMismatch)?;
    let start = &blob.start;
    if start.digest != digest || start.total_bytes != blob.bytes.len() as u64 {
        return Err(Problem::DigestMismatch);
    }
    if let Some(meta) = meta
        && (meta.byte_len != start.total_bytes || meta.media_type != start.media_type.mime())
    {
        return Err(Problem::DigestMismatch);
    }
    Ok((blob.bytes, start.media_type))
}

fn draw_text(ui: &mut egui::Ui, body: &TextBody) {
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::both()
        .id_salt("artifacts-text")
        .max_height(TEXT_VIEW_HEIGHT)
        .auto_shrink([false, true])
        .show_rows(ui, row_height, body.line_count(), |ui, rows| {
            for index in rows {
                let (line, cut) = body.line(index);
                let shown = if cut {
                    format!("{line}…")
                } else {
                    line.to_owned()
                };
                ui.add(
                    egui::Label::new(egui::RichText::new(shown).monospace())
                        .wrap_mode(egui::TextWrapMode::Extend),
                );
            }
        });
}

impl Feature for ArtifactsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Artifacts
    }

    fn title(&self) -> &'static str {
        "Artifacts"
    }

    fn availability(&self) -> Availability {
        // List/Get/ReadContent are queries; binding an image's bytes is a command.
        Availability::Implemented {
            required: &[Capability::Queries, Capability::Commands],
        }
    }

    /// A new snapshot: the data requests are void (the shared actor dropped
    /// them), so nothing decoded or held for a thread is kept. The selection is
    /// read again at the next frame and the list asked for again.
    fn reset(&mut self) {
        self.leave_scope();
        self.to_cancel.clear();
        self.scope = None;
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let FeatureReply::Query(result) = reply else {
            return;
        };
        if self.list_tag == Some(tag) {
            self.list_tag = None;
            self.list_arrived(result);
        } else if self.open.as_ref().is_some_and(|o| o.meta_tag == Some(tag)) {
            self.meta_arrived(result);
        } else if self
            .open
            .as_ref()
            .is_some_and(|o| matches!(&o.body, Body::Text(view) if view.read_tag == Some(tag)))
        {
            self.chunk_arrived(result);
        }
    }

    fn data(&mut self, tag: u64, notice: DataNotice) {
        let Some(open) = self.open.as_mut().filter(|o| o.data_tag == Some(tag)) else {
            return;
        };
        let serial = open.serial;
        let lost = match notice {
            // Ready is acted on at the next frame (take); the rest is bookkeeping.
            DataNotice::Started | DataNotice::Ready | DataNotice::Completed => return,
            DataNotice::Cancelled => Problem::Disconnected,
            DataNotice::Failed(error) => Problem::from_error(&error),
        };
        // The slot is gone with a failure or a cancellation: drop what was decoded.
        open.data_tag = None;
        open.body = Body::Failed(lost);
        self.abandon_decode(serial);
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let selection = ctx.selection();
        let scope = selection.project.clone().zip(selection.thread.clone());
        if scope != self.scope {
            self.leave_scope();
            self.scope = scope;
        }
        self.flush(ctx);
        if self.scope.is_none() {
            ui.weak("プロジェクトとスレッドを選ぶと、その成果物を表示します。");
            return;
        }
        if self.list.is_none() {
            self.request_list(ctx, false);
        }
        self.pump(&ui.ctx().clone(), ctx);
        self.draw_list(ui, ctx);
        ui.separator();
        if let Some(click) = self.draw_open(ui) {
            self.apply(ctx, click);
        }
        self.flush(ctx);
    }
}

#[cfg(test)]
mod tests;
