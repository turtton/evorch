//! Drawing a body that may still be in the store: loading, retrying and failed
//! states in the place the text will take, and the text once it is read.
//!
//! The resolver is found through the egui context, so a pane draws a body the
//! same way whether or not any body is deferred. Nothing here reads storage.

use std::sync::Arc;
use std::time::Duration;

use egui::{Id, RichText, Ui};

use super::resolver::{AUTO_RETRIES, BodyFailure, BodyResolver, BodyView, PinState};
use super::text::{BodyText, Segment};
use super::types::{BodyError, BodyErrorKind, BodyOrigin};
use crate::theme::icons;
use crate::theme::tokens::{SP_1, SP_2, palette};

const RESOLVER_ID: &str = "body-resolver";
const TRANSITION_ID: &str = "body-transition";

/// Whether a body changed between waiting and shown since the last call. The
/// transcript asks after laying out its rows.
pub fn take_transition(ctx: &egui::Context) -> bool {
    ctx.data_mut(|data| data.remove_temp::<bool>(Id::new(TRANSITION_ID)))
        .unwrap_or(false)
}

/// Makes `resolver` the one panes drawing in `ctx` use.
pub fn install(ctx: &egui::Context, resolver: BodyResolver) {
    ctx.data_mut(|data| data.insert_temp(Id::new(RESOLVER_ID), resolver));
}

/// The resolver installed in `ctx`, if any.
#[must_use]
pub fn resolver(ctx: &egui::Context) -> Option<BodyResolver> {
    ctx.data(|data| data.get_temp::<BodyResolver>(Id::new(RESOLVER_ID)))
}

/// The text of a body that is read: borrowed from the transcript when it is
/// inline, otherwise shared with the resolver's cache (a body of one piece) or
/// with the joined text kept for it (several pieces). Asking again never
/// copies the text.
#[derive(Debug, Clone)]
pub enum SharedText<'a> {
    Borrowed(&'a str),
    Shared(Arc<str>),
}

impl std::ops::Deref for SharedText<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        match self {
            Self::Borrowed(text) => text,
            Self::Shared(text) => text,
        }
    }
}

impl<'a> SharedText<'a> {
    /// Whether this is the very text another asker got: a copy would not be.
    #[must_use]
    pub fn shares_with(&self, other: &Arc<str>) -> bool {
        matches!(self, Self::Shared(own) if Arc::ptr_eq(own, other))
    }

    /// Whether both are the same allocation.
    #[must_use]
    pub fn is_same_allocation(&self, other: &SharedText<'_>) -> bool {
        let (Self::Shared(own), SharedText::Shared(theirs)) = (self, other) else {
            return false;
        };
        Arc::ptr_eq(own, theirs)
    }
}

/// Where a body stands for the frame being drawn.
#[derive(Debug, Clone)]
pub enum TextState<'a> {
    Ready(SharedText<'a>),
    Loading {
        bytes: u64,
    },
    Retrying {
        bytes: u64,
        attempt: u8,
        retry_in: Duration,
        error: BodyError,
    },
    Failed(Vec<BodyFailure>),
}

/// Asks the resolver for every deferred piece of `text` (marking them as shown
/// by this frame) and joins them once all are read.
#[must_use]
pub fn poll_text<'a>(ctx: &egui::Context, text: &'a BodyText) -> TextState<'a> {
    if let Some(inline) = text.inline() {
        return TextState::Ready(SharedText::Borrowed(inline));
    }
    let Some(resolver) = resolver(ctx) else {
        return TextState::Failed(vec![no_store_failure(text)]);
    };
    // Joined text that is kept is all a body of several pieces needs: asking
    // for the pieces too would read again what the join made room by evicting.
    if text.segments().len() > 1
        && let Some(joined) = resolver.peek_joined(text)
    {
        return TextState::Ready(SharedText::Shared(joined));
    }
    let mut views = Vec::new();
    for segment in text.segments() {
        if let Segment::Deferred(body) = segment {
            views.push(resolver.view(body));
        }
    }
    let mut failed = Vec::new();
    let mut retrying = None;
    let mut loading = false;
    for view in &views {
        match view {
            BodyView::Failed(failure) => failed.push(failure.clone()),
            BodyView::Retrying {
                attempt,
                retry_in,
                error,
                ..
            } => {
                retrying.get_or_insert((*attempt, *retry_in, error.clone()));
            }
            BodyView::Loading { .. } => loading = true,
            BodyView::Ready(_) => {}
        }
    }
    if !failed.is_empty() {
        return TextState::Failed(failed);
    }
    let bytes = text.byte_len();
    if let Some((attempt, retry_in, error)) = retrying {
        return TextState::Retrying {
            bytes,
            attempt,
            retry_in,
            error,
        };
    }
    if loading {
        return TextState::Loading { bytes };
    }
    let mut ready = views.into_iter().filter_map(|view| match view {
        BodyView::Ready(text) => Some(text),
        _ => None,
    });
    if let [Segment::Deferred(_)] = text.segments()
        && let Some(only) = ready.next()
    {
        return TextState::Ready(SharedText::Shared(only));
    }
    let mut parts = ready;
    let text_joined = resolver.joined(text, |_| parts.next());
    text_joined.map_or_else(
        || TextState::Failed(vec![no_store_failure(text)]),
        |text| TextState::Ready(SharedText::Shared(text)),
    )
}

/// The text of `text` if nothing in it needs a read: inline, or already held.
/// Never asks for a read.
#[must_use]
pub fn peek_text<'a>(ctx: &egui::Context, text: &'a BodyText) -> Option<SharedText<'a>> {
    if let Some(inline) = text.inline() {
        return Some(SharedText::Borrowed(inline));
    }
    let resolver = resolver(ctx)?;
    if let [Segment::Deferred(body)] = text.segments() {
        return resolver.peek(body).map(SharedText::Shared);
    }
    resolver
        .joined(text, |body| resolver.peek(body))
        .map(SharedText::Shared)
}

fn no_store_failure(text: &BodyText) -> BodyFailure {
    let body = text.bodies().next();
    BodyFailure {
        key: body.map_or(
            super::types::BodyRef {
                hash: [0; 32],
                byte_len: 0,
            },
            |body| body.key,
        ),
        error: BodyError::new(BodyErrorKind::Missing, "no body store is attached"),
        origin: body.map(|body| (*body.origin).clone()).unwrap_or_default(),
        attempts: 0,
    }
}

/// Draws `text`. Once every piece is read, `render` draws the joined text;
/// until then a placeholder of the size the text is expected to take stands in
/// its place.
pub fn show_text(ui: &mut Ui, id: Id, text: &BodyText, render: impl FnOnce(&mut Ui, &str)) {
    if let Some(inline) = text.inline() {
        render(ui, inline);
        return;
    }
    let state = poll_text(ui.ctx(), text);
    show_state(ui, id, &state, text, render);
}

/// [`show_text`] for a state the caller polled itself.
pub fn show_state(
    ui: &mut Ui,
    id: Id,
    state: &TextState<'_>,
    text: &BodyText,
    render: impl FnOnce(&mut Ui, &str),
) {
    let height_id = id.with("body-height");
    // A body that changes between waiting and shown resizes its row; the
    // transcript keeps the entry the reader is looking at where it was.
    let shown_id = id.with("body-shown");
    let now_ready = matches!(state, TextState::Ready(_));
    let was_ready: Option<bool> = ui.data(|data| data.get_temp(shown_id));
    ui.data_mut(|data| data.insert_temp(shown_id, now_ready));
    if was_ready.is_some_and(|was| was != now_ready) {
        ui.data_mut(|data| data.insert_temp(Id::new(TRANSITION_ID), true));
    }
    if let TextState::Ready(ready) = state {
        let top = ui.cursor().top();
        render(ui, ready);
        // The extent of what was drawn, without the spacing that trails it.
        let height = (ui.min_rect().bottom() - top).max(0.0);
        ui.data_mut(|data| data.insert_temp(height_id, height));
        return;
    }
    let remembered: Option<f32> = ui.data(|data| data.get_temp(height_id));
    let reserved = remembered.unwrap_or_else(|| estimated_height(ui, text.byte_len()));
    placeholder(ui, id, state, text, reserved);
}

/// Height for text of `bytes` that has not been laid out yet: a line per
/// screen width of characters, bounded.
fn estimated_height(ui: &Ui, bytes: u64) -> f32 {
    let line = ui.text_style_height(&egui::TextStyle::Body);
    let per_line = (ui.available_width() / 7.0).max(20.0);
    let lines = (bytes as f32 / per_line).ceil().max(1.0);
    (lines * line).clamp(line * 2.5, 480.0)
}

/// What a placeholder's buttons asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaceholderClicks {
    pub retry: bool,
    pub copy: bool,
}

fn placeholder(ui: &mut Ui, id: Id, state: &TextState<'_>, text: &BodyText, reserved: f32) {
    let fill = match state {
        TextState::Failed(_) => palette().ERROR_SURFACE,
        _ => palette().SURFACE,
    };
    let frame = egui::Frame::new()
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(SP_2 as i8, SP_1 as i8))
        .corner_radius(egui::CornerRadius::same(4));
    let clicks = frame
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height((reserved - SP_1 * 2.0).max(0.0));
            let mut clicks = PlaceholderClicks::default();
            ui.horizontal_wrapped(|ui| match state {
                TextState::Loading { bytes } => {
                    ui.add(egui::Spinner::new());
                    ui.label(RichText::new(loading_text(*bytes)).color(palette().TEXT_MUTED));
                }
                TextState::Retrying {
                    attempt,
                    retry_in,
                    error,
                    ..
                } => {
                    ui.add(egui::Spinner::new());
                    ui.label(
                        RichText::new(retrying_text(*attempt, *retry_in))
                            .color(palette().WARNING_FG),
                    )
                    .on_hover_text(error.to_string());
                    ui.ctx()
                        .request_repaint_after((*retry_in).min(Duration::from_millis(500)));
                }
                TextState::Failed(failures) => {
                    ui.label(
                        RichText::new(icons::with_icon(icons::WARNING, FAILED_TITLE))
                            .color(palette().ERROR_FG),
                    );
                    if let Some(first) = failures.first() {
                        ui.label(RichText::new(failure_reason(first)).color(palette().TEXT_MUTED));
                    }
                    if ui.button(RETRY).clicked() {
                        clicks.retry = true;
                    }
                    if ui.button(COPY_DETAILS).clicked() {
                        clicks.copy = true;
                    }
                }
                TextState::Ready(_) => {}
            });
            clicks
        })
        .inner;
    if clicks.copy
        && let TextState::Failed(failures) = state
    {
        ui.ctx().copy_text(failure_details(failures));
    }
    if clicks.retry
        && let Some(resolver) = resolver(ui.ctx())
    {
        for body in text.bodies() {
            resolver.retry(body);
        }
    }
    let _ = id;
}

pub const FAILED_TITLE: &str = "Couldn't load content";
pub const RETRY: &str = "Retry";
pub const COPY_DETAILS: &str = "Copy details";

#[must_use]
pub fn loading_text(bytes: u64) -> String {
    format!("Loading content… (about {})", human_bytes(bytes))
}

#[must_use]
pub fn retrying_text(attempt: u8, retry_in: Duration) -> String {
    format!(
        "Couldn't load content, retrying in {}s (retry {attempt} of {AUTO_RETRIES})",
        retry_in.as_secs_f32().ceil() as u64
    )
}

/// One line saying why a body is not shown, in terms of what the user can do.
#[must_use]
pub fn failure_reason(failure: &BodyFailure) -> String {
    if failure.error.kind.is_integrity() {
        format!(
            "The stored content is {} ({})",
            match failure.error.kind {
                BodyErrorKind::Missing => "missing",
                BodyErrorKind::TooLarge => "too large to read",
                _ => "damaged",
            },
            failure.error.reason
        )
    } else {
        format!(
            "The database stayed unavailable after {AUTO_RETRIES} retries ({})",
            failure.error.reason
        )
    }
}

/// The text "Copy details" puts on the clipboard for `failures`.
#[must_use]
pub fn failure_details(failures: &[BodyFailure]) -> String {
    failures
        .iter()
        .map(BodyFailure::details)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A short size for the placeholder ("12.5 KB").
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Where an unreadable event's details come from, for the copy action.
#[must_use]
pub fn unreadable_details(
    id: i64,
    run_id: Option<&str>,
    kind: &str,
    reason: &str,
    reference: Option<&super::types::BodyRef>,
) -> String {
    let mut details = format!(
        "Event could not be read\nevent: {id}\nrun: {}\nkind: {kind}\nreason: {reason}\n",
        run_id.unwrap_or("(none)")
    );
    if let Some(reference) = reference {
        let hash = reference.hash_hex();
        details.push_str(&format!(
            "hash: {hash}\nbyte_len: {}\ninspect: evorch inspect blob {hash}\n",
            reference.byte_len
        ));
    }
    details.push_str(&format!("inspect: evorch inspect events --id {id}\n"));
    details
}

/// Waits on a copy of `text` for an operation: while it is read, `Pending`.
pub fn poll_pin(pin: &mut super::resolver::BodyPin) -> PinState {
    pin.poll()
}

#[allow(dead_code)]
fn _origin_is_used(_: &BodyOrigin) {}
