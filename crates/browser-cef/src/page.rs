//! An offline artifact page for an out-of-process renderer: the browser, a
//! DevTools session and the notice log behind one synchronous API with no
//! egui, tokio or agent type in it. `evorch-renderer` serves it over IPC.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::artifact::{ArtifactContext, NetLog};
use crate::browser::{Dialogs, OsrBrowser, RawFrame};
use crate::devtools::DevTools;
use crate::notices::NoticeCursor;
use crate::offline::{ArtifactBundle, INDEX};

/// How the page is created.
#[derive(Clone, Copy, Debug)]
pub struct PageSpec {
    /// Width and height in DIP; the device scale is 1, so frames have the same
    /// size in pixels.
    pub viewport: [i32; 2],
    /// Maximum off-screen frame rate (at least 1).
    pub frame_rate: i32,
}

pub struct ArtifactPage {
    browser: OsrBrowser,
    devtools: DevTools,
    log: Arc<NetLog>,
    notices: Mutex<NoticeCursor>,
}

impl ArtifactPage {
    /// Serves `bundle` offline, waits up to `timeout` for the page to load and
    /// paint, and attaches DevTools.
    pub fn open(bundle: ArtifactBundle, spec: PageSpec, timeout: Duration) -> Result<Self, String> {
        let context = ArtifactContext::create(bundle)?;
        let log = Arc::clone(&context.log);
        let browser = OsrBrowser::create(
            context,
            &ArtifactContext::url(INDEX),
            spec.viewport,
            1.0,
            Dialogs::Suppress,
            spec.frame_rate,
        )?;
        browser
            .wait_settled(timeout)
            .map_err(|error| format!("the artifact did not settle: {error}"))?;
        let devtools = DevTools::attach(&browser)?;
        Ok(Self {
            browser,
            devtools,
            log,
            notices: Mutex::default(),
        })
    }

    pub fn set_raw_frame_sink(&self, sink: Option<impl Fn(RawFrame<'_>) + Send + Sync + 'static>) {
        self.browser.set_raw_frame_sink(sink);
    }

    pub fn request_repaint(&self) {
        self.browser.request_repaint();
    }

    pub fn resize(&self, width: i32, height: i32) {
        self.browser.resize(width, height, 1.0);
    }

    /// One DevTools command; `timeout` bounds the wait for its result.
    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.devtools.call(method, params, timeout)
    }

    /// Waits for a load that is under way to finish.
    pub fn settle(&self, timeout: Duration) -> Result<(), String> {
        if self.browser.wait_loaded(Some(timeout)) {
            Ok(())
        } else {
            Err("the page did not finish loading".into())
        }
    }

    pub fn capture_png(&self, timeout: Duration) -> Result<Vec<u8>, String> {
        self.browser.capture_png(timeout)
    }

    /// `(blocked, dialogs)` since the last call: what the offline policy
    /// refused, and the dialogs the page opened (nobody sees them: alerts are
    /// closed, confirm and prompt dismissed, beforeunload left).
    pub fn take_notices(&self) -> (Vec<String>, Vec<String>) {
        let mut cursor = self.notices.lock().unwrap_or_else(|e| e.into_inner());
        let status = self.browser.status();
        (
            cursor.take_blocked(&self.log, &status),
            cursor.take_dialogs(&status),
        )
    }

    /// Blocks until the page's renderer process crashes or the browser closes;
    /// true for a crash. Returns after [`close`](Self::close) too.
    pub fn wait_gone(&self) -> bool {
        self.browser
            .wait_for(|status| status.closed || status.renderer_crashes > 0);
        self.browser.status().renderer_crashes > 0
    }

    /// Detaches DevTools and closes the browser, waiting for CEF to release it.
    pub fn close(&self) {
        self.devtools.detach();
        self.browser.close();
    }
}

impl Drop for ArtifactPage {
    fn drop(&mut self) {
        self.close();
    }
}
