//! The state of one preview: the tab the core opened for an artifact, how its
//! frames reach the screen, what is waiting to be sent for it and what it
//! answered.
use std::{collections::VecDeque, time::Instant};

use workbench_protocol::namespaces::{
    artifacts::ArtifactRef,
    browser::{RendererId, RendererTab, RendererViewport},
};

use super::{
    frame::{DecodeJob, Picture},
    input::{InputQueue, Translator},
    ops::OpId,
};

pub(super) type PreviewId = u64;

/// Longest text of an inspection result, in bytes.
pub(super) const MAX_RESULT_BYTES: usize = 32 * 1024;
/// Entries of the blocked-request and dialog notices that are shown.
pub(super) const MAX_NOTICES: usize = 64;
pub(super) const MAX_NOTICE_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Phase {
    /// The artifact is known; a session is needed first.
    Waiting,
    /// `OpenPreview` was sent; its operation is not finished.
    Opening(OpId),
    /// The result of `OpenPreview` never came; the tab list may still hold it.
    OpenUnknown,
    Open,
    /// The connection was replaced and the tab is being found again.
    Suspended,
    /// Gone for good: opening it again is the user's decision.
    Lost(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Stream {
    Off,
    Subscribing(OpId),
    /// Subscribed; the data connection is being bound.
    Binding {
        subscription: RendererId,
        tag: u64,
    },
    Live {
        subscription: RendererId,
        tag: u64,
    },
    /// Needs the user (or a fresh read of the state) before it starts again.
    Blocked(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InspectKind {
    Layout,
    Accessibility,
    Document,
}

impl InspectKind {
    pub const ALL: [Self; 3] = [Self::Layout, Self::Accessibility, Self::Document];
    pub fn label(self) -> &'static str {
        match self {
            Self::Layout => "Layout",
            Self::Accessibility => "Accessibility",
            Self::Document => "DOM",
        }
    }
    pub fn method(self) -> &'static str {
        match self {
            Self::Layout => "Page.getLayoutMetrics",
            Self::Accessibility => "Accessibility.getFullAXTree",
            Self::Document => "DOM.getDocument",
        }
    }
    pub fn params(self) -> serde_json::Value {
        match self {
            Self::Document => serde_json::json!({ "depth": 2 }),
            _ => serde_json::json!({}),
        }
    }
}

/// What the user asked for that goes out one at a time with the tab's other
/// requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    Unsubscribe(RendererId),
    StreamingOff,
    Capture,
    Notices,
    Inspect(InspectKind),
    Close,
}

pub(super) enum Capture {
    Idle,
    /// The operation is running or the bytes are on their way (data tag).
    Receiving(Option<u64>),
    Decoding(DecodeJob),
    Failed(String),
}

#[derive(Debug, Clone, Default)]
pub(super) struct Notices {
    pub blocked: Vec<String>,
    pub dialogs: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub(super) struct Inspection {
    pub label: &'static str,
    pub text: String,
    pub truncated: bool,
}

pub(super) struct Preview {
    pub id: PreviewId,
    pub artifact: ArtifactRef,
    pub title: String,
    pub phase: Phase,
    pub tab: Option<RendererTab>,
    pub stream: Stream,
    pub picture: Picture,
    /// A `Ready` notice came: the latest frame is waiting to be taken.
    pub frame_ready: bool,
    /// CSS size of the frame on screen (a frame has one pixel per CSS pixel).
    pub shown: Option<[u32; 2]>,
    pub visible: bool,
    /// The stream is not started again before this instant.
    pub cooldown: Option<Instant>,
    pub attempts: u8,
    /// `SetRendererStreaming(true)` was sent for the current subscription.
    pub streaming_asked: bool,
    /// The mutation in flight for the tab: one at a time keeps their order.
    pub lane: Option<OpId>,
    pub want_viewport: Option<RendererViewport>,
    pub last_resize: Option<Instant>,
    /// A size whose resize failed or ended unknown: it is not sent again by itself.
    pub resize_failed: Option<RendererViewport>,
    pub input: InputQueue,
    pub translator: Translator,
    pub focused: bool,
    pub actions: VecDeque<Action>,
    pub capture: Capture,
    pub capture_picture: Picture,
    pub notices: Option<Notices>,
    pub inspection: Option<Inspection>,
    pub status: Option<String>,
}

impl Preview {
    pub fn new(id: PreviewId, artifact: ArtifactRef) -> Self {
        let title = artifact.artifact_id.clone();
        Self {
            id,
            artifact,
            title,
            phase: Phase::Waiting,
            tab: None,
            stream: Stream::Off,
            picture: Picture::default(),
            frame_ready: false,
            shown: None,
            visible: false,
            cooldown: None,
            attempts: 0,
            streaming_asked: false,
            lane: None,
            want_viewport: None,
            last_resize: None,
            resize_failed: None,
            input: InputQueue::default(),
            translator: Translator::default(),
            focused: false,
            actions: VecDeque::new(),
            capture: Capture::Idle,
            capture_picture: Picture::default(),
            notices: None,
            inspection: None,
            status: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.phase == Phase::Open
    }

    /// What was derived from the connection that is gone: textures, the frame
    /// waiting, unsent input and a capture on its way.
    pub fn forget_connection(&mut self) {
        self.picture.clear();
        self.frame_ready = false;
        self.shown = None;
        self.stream = Stream::Off;
        self.attempts = 0;
        self.streaming_asked = false;
        self.cooldown = None;
        self.input.clear();
        self.translator = Translator::default();
        self.actions
            .retain(|a| !matches!(a, Action::Unsubscribe(_) | Action::StreamingOff));
        self.capture = Capture::Idle;
        self.capture_picture.clear();
    }

    pub fn queue(&mut self, action: Action) {
        const MAX_ACTIONS: usize = 8;
        if self.actions.len() < MAX_ACTIONS && !self.actions.contains(&action) {
            self.actions.push_back(action);
        }
    }
}

/// `text` cut to at most `max` bytes on a character boundary.
pub(super) fn bounded(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// Notice lines as shown: a bounded number of bounded lines.
pub(super) fn bounded_lines(lines: &[String]) -> (Vec<String>, bool) {
    let mut cut = lines.len() > MAX_NOTICES;
    let shown = lines
        .iter()
        .take(MAX_NOTICES)
        .map(|line| {
            let (text, truncated) = bounded(line, MAX_NOTICE_CHARS);
            cut |= truncated;
            text
        })
        .collect();
    (shown, cut)
}
