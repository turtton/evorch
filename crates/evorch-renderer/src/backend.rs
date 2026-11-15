//! The CEF implementation of [`renderer_ipc::Backend`].
//!
//! Everything CEF- and egui-typed stays in here and in `browser-cef`; the
//! protocol only ever sees ids, bytes and plain data.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use browser_cef::offline::ArtifactBundle;
use browser_cef::{ArtifactPage, CefRuntime, PageSpec, RawFrame, RuntimeConfig};
use renderer_ipc::{
    Backend, BackendContext, ErrorKind, EventSink, FrameQueue, Notices, OpenArtifact, RemoteError,
    TabGoneReason, TabId, Viewport,
};
use serde_json::Value;

struct Tab {
    page: ArtifactPage,
    /// Set before the core's own close, so the watcher does not report it.
    close_requested: AtomicBool,
}

pub struct CefBackend {
    // Fields drop in order: every page is gone before the runtime shuts CEF
    // down, which must happen on the thread that created it.
    tabs: Arc<Mutex<HashMap<TabId, Arc<Tab>>>>,
    watchers: Mutex<Vec<JoinHandle<()>>>,
    frames: Arc<FrameQueue>,
    events: EventSink,
    _runtime: CefRuntime,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Maps a `browser-cef` error string to a wire error.
fn remote(error: String) -> RemoteError {
    let timed_out = error.contains("timed out") || error.contains("did not");
    RemoteError::new(
        if timed_out {
            ErrorKind::Timeout
        } else {
            ErrorKind::Internal
        },
        error,
    )
}

impl CefBackend {
    /// Initializes CEF with the offline settings; must run on the thread that
    /// later drops the backend.
    pub fn start(ctx: BackendContext, config: RuntimeConfig) -> Result<Self, RemoteError> {
        let runtime = CefRuntime::initialize(config).map_err(|error| {
            RemoteError::new(
                ErrorKind::Unavailable,
                format!("the browser engine did not start: {error}"),
            )
        })?;
        Ok(Self {
            tabs: Arc::default(),
            watchers: Mutex::default(),
            frames: ctx.frames,
            events: ctx.events,
            _runtime: runtime,
        })
    }

    fn tab(&self, tab: TabId) -> Result<Arc<Tab>, RemoteError> {
        lock(&self.tabs)
            .get(&tab)
            .cloned()
            .ok_or_else(|| RemoteError::not_found(format!("tab {tab} is not open")))
    }

    fn close(&self, tab: &Tab) {
        tab.close_requested.store(true, Ordering::Release);
        tab.page.close();
    }
}

impl Backend for CefBackend {
    fn open_artifact(
        &self,
        open: &OpenArtifact,
        files: Vec<(String, String, Vec<u8>)>,
    ) -> Result<Viewport, RemoteError> {
        let mut bundle = ArtifactBundle::default();
        for (path, mime, bytes) in files {
            bundle.insert(&path, &mime, bytes);
        }
        let spec = PageSpec {
            viewport: [open.viewport.width as i32, open.viewport.height as i32],
            frame_rate: i32::from(open.frame_rate),
        };
        let timeout = Duration::from_millis(u64::from(open.timeout_ms));
        let page = ArtifactPage::open(bundle, spec, timeout).map_err(remote)?;
        let id = open.tab;
        let frames = Arc::clone(&self.frames);
        page.set_raw_frame_sink(Some(move |frame: RawFrame<'_>| {
            frames.publish(id, frame.width as u32, frame.height as u32, frame.bgra);
        }));
        let tab = Arc::new(Tab {
            page,
            close_requested: AtomicBool::new(false),
        });
        lock(&self.tabs).insert(id, Arc::clone(&tab));
        let watcher = {
            let tabs = Arc::clone(&self.tabs);
            let events = self.events.clone();
            std::thread::spawn(move || {
                let crashed = tab.page.wait_gone();
                if tab.close_requested.load(Ordering::Acquire) {
                    return;
                }
                lock(&tabs).remove(&id);
                events.tab_gone(
                    id,
                    if crashed {
                        TabGoneReason::Crashed
                    } else {
                        TabGoneReason::Closed
                    },
                );
            })
        };
        let mut watchers = lock(&self.watchers);
        // Join the watchers of tabs that are already gone, so repeated opens and
        // closes do not accumulate handles.
        let (done, running): (Vec<_>, Vec<_>) = watchers
            .drain(..)
            .partition(|watcher| watcher.is_finished());
        for watcher in done {
            let _ = watcher.join();
        }
        *watchers = running;
        watchers.push(watcher);
        Ok(open.viewport)
    }

    fn resize(&self, tab: TabId, viewport: Viewport) -> Result<(), RemoteError> {
        self.tab(tab)?
            .page
            .resize(viewport.width as i32, viewport.height as i32);
        Ok(())
    }

    fn set_streaming(&self, tab: TabId, enabled: bool) -> Result<(), RemoteError> {
        let tab = self.tab(tab)?;
        if enabled {
            tab.page.request_repaint();
        }
        Ok(())
    }

    fn capture(&self, tab: TabId, timeout: Duration) -> Result<Vec<u8>, RemoteError> {
        self.tab(tab)?.page.capture_png(timeout).map_err(remote)
    }

    fn cdp(
        &self,
        tab: TabId,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RemoteError> {
        self.tab(tab)?
            .page
            .call(method, params, timeout)
            .map_err(remote)
    }

    fn settle(&self, tab: TabId, timeout: Duration) -> Result<(), RemoteError> {
        self.tab(tab)?.page.settle(timeout).map_err(remote)
    }

    fn take_notices(&self, tab: TabId) -> Result<Notices, RemoteError> {
        let (blocked, dialogs) = self.tab(tab)?.page.take_notices();
        Ok(Notices { blocked, dialogs })
    }

    fn close_tab(&self, tab: TabId) {
        let removed = lock(&self.tabs).remove(&tab);
        if let Some(tab) = removed {
            self.close(&tab);
        }
    }

    fn shutdown(&self) {
        let tabs: Vec<_> = lock(&self.tabs).drain().map(|(_, tab)| tab).collect();
        for tab in &tabs {
            self.close(tab);
        }
        // Each watcher returns once its page is closed.
        let watchers: Vec<_> = lock(&self.watchers).drain(..).collect();
        for watcher in watchers {
            let _ = watcher.join();
        }
    }
}
