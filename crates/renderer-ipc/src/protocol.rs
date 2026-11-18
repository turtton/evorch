//! Wire types and their validation. These are the whole contract between the
//! core and the helper: no egui or CEF type, database handle or credential
//! appears here, and every size is bounded before it reaches either side.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::ids::{RendererGeneration, RequestId, SessionId, TabId};

pub const PROTOCOL_VERSION: u16 = 1;

/// Tabs one helper hosts at once. Bounds the pending-frame memory too.
pub const MAX_TABS: usize = 4;
pub const MAX_VIEWPORT_WIDTH: u32 = 3840;
pub const MAX_VIEWPORT_HEIGHT: u32 = 2160;
/// Bytes of the largest BGRA frame: 4K UHD, four bytes per pixel.
pub const MAX_FRAME_BYTES: usize = MAX_VIEWPORT_WIDTH as usize * MAX_VIEWPORT_HEIGHT as usize * 4;
pub const MAX_FRAME_RATE: u8 = 60;
pub const MAX_BUNDLE_FILES: usize = 64;
pub const MAX_PATH_BYTES: usize = 256;
pub const MAX_MIME_BYTES: usize = 128;
pub const MAX_METHOD_BYTES: usize = 128;
/// Longest time a single request may ask the helper to wait.
pub const MAX_TIMEOUT_MS: u32 = 120_000;
/// Longest error or notice text carried over the wire.
pub const MAX_TEXT_BYTES: usize = 4096;
/// Notices (blocked requests, dialogs) returned by one `TakeNotices`.
pub const MAX_NOTICES: usize = 256;
/// Index document every artifact bundle must contain.
pub const INDEX_PATH: &str = "index.html";

/// Size of the page in CSS pixels at device scale 1, so a frame has exactly
/// `width * height * 4` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    pub fn validate(self) -> Result<(), RemoteError> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_VIEWPORT_WIDTH
            || self.height > MAX_VIEWPORT_HEIGHT
        {
            return Err(RemoteError::invalid(format!(
                "viewport {}x{} is outside 1x1..={MAX_VIEWPORT_WIDTH}x{MAX_VIEWPORT_HEIGHT}",
                self.width, self.height
            )));
        }
        Ok(())
    }

    pub fn frame_bytes(self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleFile {
    pub path: String,
    pub mime: String,
    pub len: u64,
}

/// Opens an offline artifact page. The request body is the bundle's files
/// concatenated in `files` order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenArtifact {
    pub session: SessionId,
    pub tab: TabId,
    pub viewport: Viewport,
    /// Maximum off-screen frame rate, 1..=[`MAX_FRAME_RATE`].
    pub frame_rate: u8,
    pub files: Vec<BundleFile>,
    /// How long the helper waits for the page to load and paint.
    pub timeout_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Must be the first request; carries the protocol version.
    Hello {
        protocol: u16,
    },
    OpenArtifact(OpenArtifact),
    Resize {
        tab: TabId,
        viewport: Viewport,
    },
    /// Starts or stops raw-frame delivery for the tab on the frame channel.
    SetStreaming {
        tab: TabId,
        enabled: bool,
    },
    /// Changes the off-screen painting limit without replacing the tab.
    SetFrameRate {
        tab: TabId,
        frame_rate: u8,
    },
    /// Replies with a PNG of the loaded page in the response body.
    Capture {
        tab: TabId,
        timeout_ms: u32,
    },
    /// One DevTools command on the tab's page.
    Cdp {
        tab: TabId,
        method: String,
        params: Value,
        timeout_ms: u32,
    },
    /// Waits for a load that is under way to finish.
    Settle {
        tab: TabId,
        timeout_ms: u32,
    },
    /// What the offline policy blocked and which dialogs the page opened since
    /// the last call.
    TakeNotices {
        tab: TabId,
    },
    CloseTab {
        tab: TabId,
    },
    /// Closes every tab of a session; closing a session without tabs succeeds.
    CloseSession {
        session: SessionId,
    },
    /// Ends the helper after answering. Requests still queued are answered
    /// with an error first.
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestFrame {
    pub generation: RendererGeneration,
    pub id: RequestId,
    pub request: Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Invalid,
    Unsupported,
    NotFound,
    Conflict,
    /// The helper's request queue is full.
    Busy,
    Timeout,
    TooLarge,
    /// The renderer cannot run here (CEF missing, no sandbox support).
    Unavailable,
    StaleGeneration,
    Internal,
    /// The helper is shutting down.
    ShuttingDown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteError {
    pub kind: ErrorKind,
    pub message: String,
}

impl RemoteError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: truncate(message.into(), MAX_TEXT_BYTES),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for RemoteError {}

/// Cuts `text` to at most `max` bytes on a character boundary.
pub fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notices {
    pub blocked: Vec<String>,
    pub dialogs: Vec<String>,
}

impl Notices {
    /// Bounds the lists and every entry to what the wire allows.
    pub fn bounded(self) -> Self {
        let bound = |list: Vec<String>| {
            list.into_iter()
                .take(MAX_NOTICES)
                .map(|entry| truncate(entry, MAX_TEXT_BYTES))
                .collect()
        };
        Self {
            blocked: bound(self.blocked),
            dialogs: bound(self.dialogs),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Hello {
        protocol: u16,
        helper: String,
    },
    Opened {
        tab: TabId,
        viewport: Viewport,
    },
    Done,
    /// The response body is a PNG.
    Png,
    Cdp {
        result: Value,
    },
    Notices(Notices),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Ok { reply: Reply },
    Err { error: RemoteError },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResponseFrame {
    pub generation: RendererGeneration,
    pub id: RequestId,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabGoneReason {
    /// The page's renderer process crashed.
    Crashed,
    /// The browser closed without the core asking.
    Closed,
}

/// Something that happened without a request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    TabGone { tab: TabId, reason: TabGoneReason },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFrame {
    pub generation: RendererGeneration,
    pub event: EventKind,
}

/// Meta of one raw frame; the body is `width * height * 4` BGRA bytes
/// (premultiplied, as CEF paints them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMeta {
    pub generation: RendererGeneration,
    pub tab: TabId,
    pub seq: u64,
    pub width: u32,
    pub height: u32,
}

impl FrameMeta {
    /// Whether `body_len` matches the stated size within the frame limits.
    pub fn check(&self, body_len: usize) -> Result<(), RemoteError> {
        Viewport {
            width: self.width,
            height: self.height,
        }
        .validate()?;
        let expected = self.width as usize * self.height as usize * 4;
        if body_len != expected {
            return Err(RemoteError::invalid(format!(
                "frame {} has {body_len} bytes, expected {expected}",
                self.seq
            )));
        }
        Ok(())
    }
}

/// The core's receipt for one frame; the helper sends the next only after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub seq: u64,
}

/// The DevTools commands an artifact page may receive, by full name.
///
/// Whole domains are deliberately not allowed: `DOM` alone holds
/// `setFileInputFiles` and `getFileInfo`, which would let a caller read host
/// files the artifact never contained, and `Input` holds `dispatchDragEvent`
/// with file paths. What is listed inspects the loaded page (DOM, accessibility,
/// layout, styles, screenshots), runs script in it and drives it with input
/// events. Script evaluation (`Runtime.*`) can also change the page, its DOM
/// included; that stays inside the artifact, whose own scripts could do the
/// same, and is why this list is not a read-only guarantee. Anything that
/// navigates, replaces the document, touches host files, downloads, the
/// network, storage, targets, emulation or the browser itself is absent, and so
/// is anything a future DevTools version adds.
pub const CDP_ALLOWED_METHODS: &[&str] = &[
    "Accessibility.disable",
    "Accessibility.enable",
    "Accessibility.getChildAXNodes",
    "Accessibility.getFullAXTree",
    "Accessibility.getPartialAXTree",
    "Accessibility.getRootAXNode",
    "Accessibility.queryAXTree",
    "CSS.disable",
    "CSS.enable",
    "CSS.getComputedStyleForNode",
    "CSS.getInlineStylesForNode",
    "CSS.getMatchedStylesForNode",
    "CSS.getPlatformFontsForNode",
    "DOM.describeNode",
    "DOM.disable",
    "DOM.discardSearchResults",
    "DOM.enable",
    "DOM.focus",
    "DOM.getAttributes",
    "DOM.getBoxModel",
    "DOM.getContentQuads",
    "DOM.getDocument",
    "DOM.getFlattenedDocument",
    "DOM.getNodeForLocation",
    "DOM.getOuterHTML",
    "DOM.getSearchResults",
    "DOM.performSearch",
    "DOM.pushNodesByBackendIdsToFrontend",
    "DOM.querySelector",
    "DOM.querySelectorAll",
    "DOM.requestChildNodes",
    "DOM.requestNode",
    "DOM.resolveNode",
    "DOM.scrollIntoViewIfNeeded",
    "DOMSnapshot.captureSnapshot",
    "DOMSnapshot.disable",
    "DOMSnapshot.enable",
    "Input.dispatchKeyEvent",
    "Input.dispatchMouseEvent",
    "Input.imeSetComposition",
    "Input.insertText",
    "Log.clear",
    "Log.disable",
    "Log.enable",
    "Page.captureScreenshot",
    "Page.disable",
    "Page.enable",
    "Page.getFrameTree",
    "Page.getLayoutMetrics",
    "Runtime.awaitPromise",
    "Runtime.callFunctionOn",
    "Runtime.disable",
    "Runtime.enable",
    "Runtime.evaluate",
    "Runtime.getProperties",
    "Runtime.releaseObject",
    "Runtime.releaseObjectGroup",
];

pub fn cdp_method_allowed(method: &str) -> bool {
    CDP_ALLOWED_METHODS.contains(&method)
}

fn validate_timeout(timeout_ms: u32) -> Result<(), RemoteError> {
    if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
        return Err(RemoteError::invalid(format!(
            "timeout_ms {timeout_ms} is outside 1..={MAX_TIMEOUT_MS}"
        )));
    }
    Ok(())
}

fn validate_bundle_path(path: &str) -> Result<(), RemoteError> {
    let bad = path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.contains(['\\', '\0', '?', '#'])
        || path.split('/').any(|part| part.is_empty() || part == "..");
    if bad {
        return Err(RemoteError::invalid(format!(
            "bundle path {path:?} is not a clean relative path"
        )));
    }
    Ok(())
}

impl OpenArtifact {
    fn validate(&self, body_len: usize) -> Result<(), RemoteError> {
        self.viewport.validate()?;
        if self.frame_rate == 0 || self.frame_rate > MAX_FRAME_RATE {
            return Err(RemoteError::invalid(format!(
                "frame_rate {} is outside 1..={MAX_FRAME_RATE}",
                self.frame_rate
            )));
        }
        validate_timeout(self.timeout_ms)?;
        if self.files.is_empty() || self.files.len() > MAX_BUNDLE_FILES {
            return Err(RemoteError::invalid(format!(
                "a bundle has 1..={MAX_BUNDLE_FILES} files, got {}",
                self.files.len()
            )));
        }
        let mut seen = std::collections::HashSet::new();
        let mut total: u64 = 0;
        for file in &self.files {
            validate_bundle_path(&file.path)?;
            if file.mime.is_empty()
                || file.mime.len() > MAX_MIME_BYTES
                || file.mime.bytes().any(|b| b.is_ascii_control())
            {
                return Err(RemoteError::invalid(format!(
                    "bundle file {:?} has an invalid mime type",
                    file.path
                )));
            }
            if !seen.insert(file.path.as_str()) {
                return Err(RemoteError::invalid(format!(
                    "bundle file {:?} appears twice",
                    file.path
                )));
            }
            total = total.saturating_add(file.len);
        }
        if !seen.contains(INDEX_PATH) {
            return Err(RemoteError::invalid(format!(
                "a bundle must contain {INDEX_PATH}"
            )));
        }
        if total != body_len as u64 {
            return Err(RemoteError::invalid(format!(
                "bundle files add up to {total} bytes but the body has {body_len}"
            )));
        }
        Ok(())
    }

    /// Builds the request and its body from `(path, mime, bytes)` files.
    pub fn with_files(
        session: SessionId,
        tab: TabId,
        viewport: Viewport,
        frame_rate: u8,
        timeout_ms: u32,
        files: Vec<(String, String, Vec<u8>)>,
    ) -> (Self, Vec<u8>) {
        let mut body = Vec::new();
        let files = files
            .into_iter()
            .map(|(path, mime, bytes)| {
                body.extend_from_slice(&bytes);
                BundleFile {
                    path,
                    mime,
                    len: bytes.len() as u64,
                }
            })
            .collect();
        let open = Self {
            session,
            tab,
            viewport,
            frame_rate,
            files,
            timeout_ms,
        };
        (open, body)
    }

    /// Splits a request body into the files it announces. The body length was
    /// checked by [`Request::validate`].
    pub fn split_body(&self, body: &[u8]) -> Vec<(String, String, Vec<u8>)> {
        let mut offset = 0usize;
        self.files
            .iter()
            .map(|file| {
                let end = offset + file.len as usize;
                let bytes = body[offset..end].to_vec();
                offset = end;
                (file.path.clone(), file.mime.clone(), bytes)
            })
            .collect()
    }
}

impl Request {
    /// Checks everything the helper can know without a browser: bounds,
    /// shapes and the body length. `first` is whether this is the first
    /// request of the connection.
    pub fn validate(&self, body_len: usize) -> Result<(), RemoteError> {
        let no_body = || {
            if body_len == 0 {
                Ok(())
            } else {
                Err(RemoteError::invalid("this request takes no body"))
            }
        };
        match self {
            Self::Hello { protocol } => {
                no_body()?;
                if *protocol != PROTOCOL_VERSION {
                    return Err(RemoteError::new(
                        ErrorKind::Unsupported,
                        format!(
                            "protocol {protocol} is not supported (this is {PROTOCOL_VERSION})"
                        ),
                    ));
                }
                Ok(())
            }
            Self::OpenArtifact(open) => open.validate(body_len),
            Self::Resize { viewport, .. } => {
                no_body()?;
                viewport.validate()
            }
            Self::SetFrameRate { frame_rate, .. } => {
                no_body()?;
                if *frame_rate == 0 || *frame_rate > MAX_FRAME_RATE {
                    return Err(RemoteError::invalid("frame rate outside 1..=60"));
                }
                Ok(())
            }
            Self::SetStreaming { .. }
            | Self::TakeNotices { .. }
            | Self::CloseTab { .. }
            | Self::CloseSession { .. }
            | Self::Shutdown => no_body(),
            Self::Capture { timeout_ms, .. } | Self::Settle { timeout_ms, .. } => {
                no_body()?;
                validate_timeout(*timeout_ms)
            }
            Self::Cdp {
                method, timeout_ms, ..
            } => {
                no_body()?;
                validate_timeout(*timeout_ms)?;
                if method.len() > MAX_METHOD_BYTES || !cdp_method_allowed(method) {
                    return Err(RemoteError::new(
                        ErrorKind::Unsupported,
                        format!("DevTools method {method:?} is not available on an artifact page"),
                    ));
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(files: Vec<BundleFile>) -> OpenArtifact {
        OpenArtifact {
            session: SessionId(1),
            tab: TabId(1),
            viewport: Viewport {
                width: 1280,
                height: 720,
            },
            frame_rate: 30,
            files,
            timeout_ms: 5000,
        }
    }

    fn file(path: &str, len: u64) -> BundleFile {
        BundleFile {
            path: path.into(),
            mime: "text/html".into(),
            len,
        }
    }

    #[test]
    fn a_well_formed_open_splits_its_body_into_the_announced_files() {
        let open = open(vec![file("index.html", 3), file("img/a.png", 2)]);
        open.validate(5).unwrap();
        let parts = open.split_body(b"abcde");
        assert_eq!(parts[0].2, b"abc");
        assert_eq!(parts[1].0, "img/a.png");
        assert_eq!(parts[1].2, b"de");
    }

    #[test]
    fn a_bundle_that_lies_about_its_size_or_names_is_rejected() {
        assert!(open(vec![file("index.html", 3)]).validate(4).is_err());
        assert!(open(vec![file("a.html", 1)]).validate(1).is_err());
        assert!(
            open(vec![file("index.html", 1), file("index.html", 1)])
                .validate(2)
                .is_err()
        );
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a//b",
            "a/../b",
            "a\\b",
            "a?b",
            "a#b",
        ] {
            assert!(
                open(vec![file("index.html", 1), file(bad, 1)])
                    .validate(2)
                    .is_err(),
                "{bad:?}"
            );
        }
        let too_many = (0..=MAX_BUNDLE_FILES)
            .map(|i| file(&format!("f{i}.html"), 0))
            .chain([file("index.html", 0)])
            .collect();
        assert!(open(too_many).validate(0).is_err());
    }

    #[test]
    fn viewport_and_timeout_bounds_hold() {
        let ok = Viewport {
            width: MAX_VIEWPORT_WIDTH,
            height: MAX_VIEWPORT_HEIGHT,
        };
        assert!(ok.validate().is_ok());
        assert_eq!(ok.frame_bytes(), MAX_FRAME_BYTES);
        for bad in [
            (0, 1),
            (1, 0),
            (MAX_VIEWPORT_WIDTH + 1, 1),
            (1, MAX_VIEWPORT_HEIGHT + 1),
        ] {
            let viewport = Viewport {
                width: bad.0,
                height: bad.1,
            };
            assert!(viewport.validate().is_err(), "{bad:?}");
        }
        let settle = |timeout_ms| Request::Settle {
            tab: TabId(1),
            timeout_ms,
        };
        assert!(settle(1).validate(0).is_ok());
        assert!(settle(0).validate(0).is_err());
        assert!(settle(MAX_TIMEOUT_MS + 1).validate(0).is_err());
        assert!(settle(1).validate(1).is_err(), "a body is not allowed");
    }

    #[test]
    fn only_page_operating_devtools_methods_are_allowed() {
        for allowed in [
            "Runtime.evaluate",
            "DOM.resolveNode",
            "Input.dispatchMouseEvent",
            "Page.captureScreenshot",
            "Accessibility.getFullAXTree",
        ] {
            assert!(cdp_method_allowed(allowed), "{allowed}");
        }
        for denied in [
            "DOM.setFileInputFiles",
            "DOM.getFileInfo",
            "DOM.setOuterHTML",
            "DOM.setNodeValue",
            "Input.dispatchDragEvent",
            "Input.setInterceptDrags",
            "Page.setInterceptFileChooserDialog",
            "Page.setDownloadBehavior",
            "Page.setBypassCSP",
            "Page.addScriptToEvaluateOnNewDocument",
            "Page.reload",
            "Page.printToPDF",
            "Page.getResourceContent",
            "Browser.setDownloadBehavior",
            "IO.read",
            "Emulation.setDeviceMetricsOverride",
            "Runtime.compileScript",
            "DOM.enable ",
            "dom.enable",
            "Page.navigate",
            "Page.navigateToHistoryEntry",
            "Page.setDocumentContent",
            "Target.createTarget",
            "Browser.close",
            "Fetch.enable",
            "Network.enable",
            "Storage.clearDataForOrigin",
            "Runtime",
            "Runtime.evaluate;x",
            "",
        ] {
            assert!(!cdp_method_allowed(denied), "{denied}");
        }
    }

    #[test]
    fn the_allowlist_lists_full_names_and_nothing_that_reaches_files_or_navigates() {
        for method in CDP_ALLOWED_METHODS {
            let (domain, name) = method.split_once('.').expect("Domain.method");
            assert!(!domain.is_empty() && !name.is_empty(), "{method}");
            for word in [
                "File",
                "navigate",
                "Download",
                "Drag",
                "Intercept",
                "Bypass",
                "Script",
                "ResourceContent",
            ] {
                assert!(!name.contains(word), "{method} contains {word}");
            }
        }
        let mut sorted = CDP_ALLOWED_METHODS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted, CDP_ALLOWED_METHODS,
            "keep the list sorted and unique"
        );
    }

    #[test]
    fn a_frame_must_match_its_stated_size() {
        let meta = FrameMeta {
            generation: RendererGeneration(1),
            tab: TabId(1),
            seq: 1,
            width: 4,
            height: 2,
        };
        assert!(meta.check(32).is_ok());
        assert!(meta.check(31).is_err());
        let huge = FrameMeta {
            width: MAX_VIEWPORT_WIDTH + 1,
            ..meta
        };
        assert!(huge.check(huge.width as usize * 8).is_err());
    }

    #[test]
    fn text_is_cut_on_a_character_boundary() {
        let text = "あいう".to_owned();
        assert_eq!(truncate(text.clone(), 4), "あ");
        assert_eq!(truncate(text, 100), "あいう");
    }

    #[test]
    fn requests_round_trip_through_json() {
        let request = RequestFrame {
            generation: RendererGeneration(3),
            id: RequestId(9),
            request: Request::Cdp {
                tab: TabId(2),
                method: "Runtime.evaluate".into(),
                params: serde_json::json!({"expression": "1+1"}),
                timeout_ms: 1000,
            },
        };
        let json = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            serde_json::from_slice::<RequestFrame>(&json).unwrap(),
            request
        );
    }
}

#[cfg(test)]
#[test]
fn dynamic_frame_rate_uses_the_same_one_to_sixty_fps_bound_and_no_body() {
    for (frame_rate, valid) in [
        (0, false),
        (1, true),
        (10, true),
        (30, true),
        (60, true),
        (61, false),
        (255, false),
    ] {
        let request = Request::SetFrameRate {
            tab: TabId(1),
            frame_rate,
        };
        assert_eq!(request.validate(0).is_ok(), valid);
        assert!(request.validate(1).is_err());
    }
}
