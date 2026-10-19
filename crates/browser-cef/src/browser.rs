use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use cef::*;

use crate::artifact::ArtifactContext;
use crate::clipboard::{COPY_MESSAGE, MAX_COPY_BYTES, MAX_PENDING_COPIES};
use crate::frame::{PaintBuffer, PixelRect};
use crate::offline::is_allowed_url;
use crate::popup::Popup;
use crate::runtime::{SendCell, on_ui, post_ui};

const CREATE_TIMEOUT: Duration = Duration::from_secs(20);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Renderer crashes recovered by reloading before the user has to reload.
const AUTO_RELOADS: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ViewGeometry {
    width: i32,
    height: i32,
    scale: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub cursor: u32,
    pub tooltip: Option<String>,
    pub title: String,
    pub url: String,
    pub loading: bool,
    pub load_end_count: u64,
    pub load_errors: Vec<String>,
    pub blocked_popups: Vec<String>,
    pub blocked_navigations: Vec<String>,
    pub suppressed_dialogs: Vec<String>,
    pub blocked_file_dialogs: u32,
    pub renderer_crashes: u32,
    pub last_crash: Option<String>,
    pub ime_caret: Option<PixelRect>,
    pub popup: Option<Popup>,
    pub dialog: Option<JsDialog>,
    /// Text the page copied, waiting for the view to put it on the clipboard.
    pub copied: Vec<String>,
    pub closed: bool,
}

/// What happens to `alert`, `confirm`, `prompt` and `beforeunload`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialogs {
    /// Wait in [`Status::dialog`] for [`OsrBrowser::answer_dialog`].
    Ask,
    /// Close alerts, dismiss confirm and prompt, leave on beforeunload, and
    /// note each in [`Status::suppressed_dialogs`].
    Suppress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt { default: String },
    BeforeUnload { reload: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsDialog {
    pub id: u64,
    pub kind: DialogKind,
    pub message: String,
}

type Repaint = Box<dyn Fn() + Send + Sync>;

pub(crate) struct Shared {
    view: Mutex<ViewGeometry>,
    pub(crate) frame: Mutex<PaintBuffer>,
    pub(crate) popup_frame: Mutex<PaintBuffer>,
    dialogs: Dialogs,
    // The latest dialog id and the callback still waiting for an answer.
    dialog_callback: Mutex<(u64, Option<SendCell<JsdialogCallback>>)>,
    status: Mutex<Status>,
    browser: Mutex<Option<SendCell<Browser>>>,
    changes: Mutex<u64>,
    changed: Condvar,
    repaint: Mutex<Option<Repaint>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn notify(&self) {
        *lock(&self.changes) += 1;
        self.changed.notify_all();
        if let Some(repaint) = lock(&self.repaint).as_ref() {
            repaint();
        }
    }

    pub(crate) fn wait(
        &self,
        timeout: Option<Duration>,
        mut predicate: impl FnMut(&Self) -> bool,
    ) -> bool {
        // A timeout too large for `Instant` waits without a deadline.
        let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
        loop {
            let seen = *lock(&self.changes);
            if predicate(self) {
                return true;
            }
            let changes = lock(&self.changes);
            if *changes != seen {
                continue;
            }
            match deadline {
                None => drop(
                    self.changed
                        .wait(changes)
                        .unwrap_or_else(|e| e.into_inner()),
                ),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return false;
                    }
                    drop(
                        self.changed
                            .wait_timeout(changes, deadline - now)
                            .unwrap_or_else(|e| e.into_inner()),
                    );
                }
            }
        }
    }

    pub(crate) fn status(&self) -> MutexGuard<'_, Status> {
        lock(&self.status)
    }

    fn browser(&self) -> Option<Browser> {
        lock(&self.browser)
            .as_ref()
            .map(|browser| browser.0.clone())
    }
}

/// A windowless browser showing one artifact. Create it after [`crate::CefRuntime`]
/// and close it before the runtime drops.
pub struct OsrBrowser {
    pub(crate) shared: Arc<Shared>,
    // Released on close, so no CEF reference outlives a closed browser.
    artifact: Mutex<Option<ArtifactContext>>,
}

impl OsrBrowser {
    /// Opens `url` (normally [`ArtifactContext::url`]) at `width`×`height`
    /// DIP rendered at `scale` physical pixels per DIP.
    pub fn create(
        artifact: ArtifactContext,
        url: &str,
        [width, height]: [i32; 2],
        scale: f32,
        dialogs: Dialogs,
    ) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            view: Mutex::new(ViewGeometry {
                width: width.max(1),
                height: height.max(1),
                scale,
            }),
            frame: Mutex::default(),
            popup_frame: Mutex::default(),
            dialogs,
            dialog_callback: Mutex::default(),
            status: Mutex::default(),
            browser: Mutex::default(),
            changes: Mutex::new(0),
            changed: Condvar::new(),
            repaint: Mutex::default(),
        });
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            ..Default::default()
        };
        let settings = BrowserSettings {
            windowless_frame_rate: 60,
            background_color: 0xFFFF_FFFF,
            ..Default::default()
        };
        let mut client = PreviewClient::new(Arc::clone(&shared));
        let mut context = artifact.context.0.clone();
        if browser_host_create_browser(
            Some(&window_info),
            Some(&mut client),
            Some(&CefString::from(url)),
            Some(&settings),
            None,
            Some(&mut context),
        ) != 1
        {
            return Err("CEF refused to create the browser".into());
        }
        if !shared.wait(Some(CREATE_TIMEOUT), |shared| shared.browser().is_some()) {
            return Err("CEF did not create the browser in time".into());
        }
        Ok(Self {
            shared,
            artifact: Mutex::new(Some(artifact)),
        })
    }

    pub fn host(&self) -> Option<BrowserHost> {
        self.shared.browser().and_then(|browser| browser.host())
    }

    pub fn status(&self) -> MutexGuard<'_, Status> {
        self.shared.status()
    }

    pub fn frame_seq(&self) -> u64 {
        lock(&self.shared.frame).seq()
    }

    pub fn popup_frame_seq(&self) -> u64 {
        lock(&self.shared.popup_frame).seq()
    }

    /// Answers the dialog in [`Status::dialog`] with this `id`: OK or Cancel,
    /// with `text` for a prompt. Answers to a dialog that is gone are ignored.
    pub fn answer_dialog(&self, id: u64, accept: bool, text: Option<&str>) {
        let callback = {
            let mut pending = lock(&self.shared.dialog_callback);
            if pending.0 == id {
                pending.1.take()
            } else {
                None
            }
        };
        let Some(callback) = callback else { return };
        self.shared.status().dialog = None;
        let text = text.map(str::to_owned);
        post_ui(move || {
            let text = text.as_deref().map(CefString::from);
            callback.0.cont(accept.into(), text.as_ref());
        });
        self.shared.notify();
    }

    /// Text the page copied since the last call, oldest first.
    pub fn take_copies(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.status().copied)
    }

    /// Waits without a deadline for `predicate`, re-checked after each browser
    /// callback; for callers whose runner bounds the wait, such as tests.
    pub fn wait_for(&self, mut predicate: impl FnMut(&Status) -> bool) {
        self.shared.wait(None, |shared| predicate(&shared.status()));
    }

    /// The first frame painted after the page finished loading. A repaint is
    /// forced once loading ends, so the frame reflects the loaded document.
    pub fn capture_settled(&self, timeout: Duration) -> Result<egui::ColorImage, String> {
        let deadline = Instant::now().checked_add(timeout);
        let remaining =
            || deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if !self.wait_loaded(remaining()) {
            return Err("the page did not finish loading".into());
        }
        let painted = self.frame_seq();
        let host = SendCell(self.host().ok_or("the browser is closed")?);
        on_ui(move || host.0.invalidate(PaintElementType::VIEW));
        if !self
            .shared
            .wait(remaining(), |shared| lock(&shared.frame).seq() > painted)
        {
            return Err("the page did not paint after loading".into());
        }
        lock(&self.shared.frame)
            .frame()
            .ok_or_else(|| "no frame was painted".into())
    }

    /// Waits until the document has loaded and nothing is loading; false if
    /// `timeout` passes or the browser closes first. `None` waits without a deadline.
    pub fn wait_loaded(&self, timeout: Option<Duration>) -> bool {
        self.shared.wait(timeout, |shared| {
            let status = shared.status();
            status.closed || (status.load_end_count > 0 && !status.loading)
        }) && !self.status().closed
    }

    pub fn set_repaint_callback(&self, callback: impl Fn() + Send + Sync + 'static) {
        *lock(&self.shared.repaint) = Some(Box::new(callback));
    }

    /// Resizes the view in DIP; `scale` is egui's pixels per point.
    pub fn resize(&self, width: i32, height: i32, scale: f32) {
        let new = ViewGeometry {
            width: width.max(1),
            height: height.max(1),
            scale,
        };
        let old = std::mem::replace(&mut *lock(&self.shared.view), new);
        if old == new {
            return;
        }
        if let Some(host) = self.host() {
            if old.scale != new.scale {
                host.notify_screen_info_changed();
            }
            host.was_resized();
        }
    }

    pub fn reload(&self) {
        if let Some(browser) = self.shared.browser() {
            browser.reload();
        }
    }

    /// Closes the browser and waits for CEF to release it; required before the
    /// runtime shuts down.
    pub fn close(&self) {
        if let Some(host) = self.host() {
            let host = SendCell(host);
            on_ui(move || host.0.close_browser(1));
            self.shared
                .wait(Some(CLOSE_TIMEOUT), |shared| shared.status().closed);
            lock(&self.shared.browser).take();
        }
        lock(&self.artifact).take();
    }
}

impl Drop for OsrBrowser {
    fn drop(&mut self) {
        self.close();
    }
}

fn text(value: Option<&CefString>) -> String {
    value.map(ToString::to_string).unwrap_or_default()
}

fn pixel_rect(rect: &Rect) -> PixelRect {
    PixelRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

wrap_client! {
    struct PreviewClient {
        shared: Arc<Shared>,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(PreviewRender::new(Arc::clone(&self.shared)))
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(PreviewDisplay::new(Arc::clone(&self.shared)))
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(PreviewLifeSpan::new(Arc::clone(&self.shared)))
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(PreviewLoad::new(Arc::clone(&self.shared)))
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(PreviewRequests::new(Arc::clone(&self.shared)))
        }
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(JsDialogs::new(Arc::clone(&self.shared)))
        }
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(CancelFileDialogs::new(Arc::clone(&self.shared)))
        }
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(NoContextMenu::new())
        }

        fn on_process_message_received(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> i32 {
            let Some(message) = message else { return 0 };
            if source_process != ProcessId::RENDERER || CefString::from(&message.name()).to_string() != COPY_MESSAGE {
                return 0;
            }
            let text = message
                .argument_list()
                .map(|arguments| CefString::from(&arguments.string(0)).to_string())
                .filter(|text| !text.is_empty() && text.len() <= MAX_COPY_BYTES);
            if let Some(text) = text {
                let mut status = self.shared.status();
                if status.copied.len() == MAX_PENDING_COPIES {
                    status.copied.remove(0);
                }
                status.copied.push(text);
                drop(status);
                self.shared.notify();
            }
            1
        }
    }
}

wrap_render_handler! {
    struct PreviewRender {
        shared: Arc<Shared>,
    }

    impl RenderHandler {
        fn root_screen_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) -> i32 {
            let view = *lock(&self.shared.view);
            if let Some(rect) = rect {
                *rect = Rect { x: 0, y: 0, width: view.width, height: view.height };
            }
            1
        }

        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let view = *lock(&self.shared.view);
            if let Some(rect) = rect {
                *rect = Rect { x: 0, y: 0, width: view.width, height: view.height };
            }
        }

        fn screen_point(
            &self,
            _browser: Option<&mut Browser>,
            view_x: i32,
            view_y: i32,
            screen_x: Option<&mut i32>,
            screen_y: Option<&mut i32>,
        ) -> i32 {
            let (Some(x), Some(y)) = (screen_x, screen_y) else { return 0 };
            *x = view_x;
            *y = view_y;
            1
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, screen_info: Option<&mut ScreenInfo>) -> i32 {
            let view = *lock(&self.shared.view);
            let Some(info) = screen_info else { return 0 };
            let rect = Rect { x: 0, y: 0, width: view.width, height: view.height };
            info.device_scale_factor = view.scale;
            info.rect = rect.clone();
            info.available_rect = rect;
            info.depth = 24;
            info.depth_per_component = 8;
            1
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: i32,
            height: i32,
        ) {
            let target = if type_ == PaintElementType::VIEW {
                &self.shared.frame
            } else if type_ == PaintElementType::POPUP {
                &self.shared.popup_frame
            } else {
                return;
            };
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let len = width as usize * height as usize * 4;
            // SAFETY: CEF passes a `width * height` BGRA buffer valid for this call.
            let source = unsafe { std::slice::from_raw_parts(buffer, len) };
            let dirty: Vec<_> = dirty_rects.unwrap_or_default().iter().map(pixel_rect).collect();
            lock(target).paint(source, width, height, &dirty);
            self.shared.notify();
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: i32) {
            if show == 0 {
                self.shared.status().popup = None;
                *lock(&self.shared.popup_frame) = PaintBuffer::default();
                self.shared.notify();
            }
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(rect) = rect else { return };
            let view = *lock(&self.shared.view);
            self.shared.status().popup = Some(Popup::place(pixel_rect(rect), view.width, view.height));
            self.shared.notify();
        }

        fn on_ime_composition_range_changed(
            &self,
            _browser: Option<&mut Browser>,
            _selected_range: Option<&Range>,
            character_bounds: Option<&[Rect]>,
        ) {
            self.shared.status().ime_caret = character_bounds.and_then(|bounds| bounds.last()).map(pixel_rect);
        }
    }
}

wrap_display_handler! {
    struct PreviewDisplay {
        shared: Arc<Shared>,
    }

    impl DisplayHandler {
        fn on_address_change(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_some_and(|frame| frame.is_main() == 1) {
                self.shared.status().url = text(url);
            }
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            self.shared.status().title = text(title);
            self.shared.notify();
        }

        fn on_tooltip(&self, _browser: Option<&mut Browser>, value: Option<&mut CefString>) -> i32 {
            self.shared.status().tooltip = value.map(|value| value.to_string()).filter(|value| !value.is_empty());
            self.shared.notify();
            1
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: std::os::raw::c_ulong,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> i32 {
            self.shared.status().cursor = type_.get_raw();
            self.shared.notify();
            1
        }
    }
}

wrap_life_span_handler! {
    struct PreviewLifeSpan {
        shared: Arc<Shared>,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            self.shared.status().blocked_popups.push(text(target_url));
            self.shared.notify();
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser {
                *lock(&self.shared.browser) = Some(SendCell(browser.clone()));
            }
            self.shared.notify();
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            self.shared.status().closed = true;
            self.shared.notify();
        }
    }
}

wrap_load_handler! {
    struct PreviewLoad {
        shared: Arc<Shared>,
    }

    impl LoadHandler {
        fn on_loading_state_change(&self, _browser: Option<&mut Browser>, is_loading: i32, _back: i32, _forward: i32) {
            let mut status = self.shared.status();
            status.loading = is_loading != 0;
            if !status.loading {
                status.load_end_count += 1;
            }
            drop(status);
            self.shared.notify();
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            self.shared.status().load_errors.push(format!(
                "{error_code:?} {} {}",
                text(error_text),
                text(failed_url)
            ));
            self.shared.notify();
        }
    }
}

wrap_request_handler! {
    struct PreviewRequests {
        shared: Arc<Shared>,
    }

    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: i32,
            _is_redirect: i32,
        ) -> i32 {
            let url = request.map(|request| CefString::from(&request.url()).to_string()).unwrap_or_default();
            if is_allowed_url(&url) {
                return 0;
            }
            self.shared.status().blocked_navigations.push(url);
            self.shared.notify();
            1
        }

        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: i32,
            error_string: Option<&CefString>,
        ) {
            let mut state = self.shared.status();
            state.renderer_crashes += 1;
            state.last_crash = Some(format!("{status:?} code={error_code} {}", text(error_string)));
            let reload = state.renderer_crashes <= AUTO_RELOADS;
            drop(state);
            if reload && let Some(browser) = browser {
                browser.reload();
            }
            self.shared.notify();
        }
    }
}

impl Shared {
    /// Holds a dialog for the user, or settles it at once under [`Dialogs::Suppress`].
    fn open_dialog(
        &self,
        kind: DialogKind,
        message: String,
        callback: Option<&mut JsdialogCallback>,
    ) {
        let Some(callback) = callback else { return };
        if self.dialogs == Dialogs::Suppress {
            let (accept, note) = match &kind {
                DialogKind::Alert => (true, format!("alert {message:?} (closed)")),
                DialogKind::Confirm => (false, format!("confirm {message:?} (dismissed)")),
                DialogKind::Prompt { .. } => (false, format!("prompt {message:?} (dismissed)")),
                DialogKind::BeforeUnload { .. } => {
                    (true, "beforeunload (left the page)".to_owned())
                }
            };
            self.status().suppressed_dialogs.push(note);
            callback.cont(accept.into(), None);
            self.notify();
            return;
        }
        let mut pending = lock(&self.dialog_callback);
        pending.0 += 1;
        pending.1 = Some(SendCell(callback.clone()));
        let id = pending.0;
        drop(pending);
        self.status().dialog = Some(JsDialog { id, kind, message });
        self.notify();
    }

    fn close_dialog(&self) {
        lock(&self.dialog_callback).1 = None;
        self.status().dialog = None;
        self.notify();
    }
}

wrap_jsdialog_handler! {
    struct JsDialogs {
        shared: Arc<Shared>,
    }

    impl JsdialogHandler {
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>,
            _suppress_message: Option<&mut i32>,
        ) -> i32 {
            let kind = if dialog_type == JsdialogType::CONFIRM {
                DialogKind::Confirm
            } else if dialog_type == JsdialogType::PROMPT {
                DialogKind::Prompt { default: text(default_prompt_text) }
            } else {
                DialogKind::Alert
            };
            self.shared.open_dialog(kind, text(message_text), callback);
            1
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            message_text: Option<&CefString>,
            is_reload: i32,
            callback: Option<&mut JsdialogCallback>,
        ) -> i32 {
            let kind = DialogKind::BeforeUnload { reload: is_reload != 0 };
            self.shared.open_dialog(kind, text(message_text), callback);
            1
        }

        fn on_reset_dialog_state(&self, _browser: Option<&mut Browser>) {
            self.shared.close_dialog();
        }
    }
}

wrap_dialog_handler! {
    struct CancelFileDialogs {
        shared: Arc<Shared>,
    }

    impl DialogHandler {
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            _mode: FileDialogMode,
            _title: Option<&CefString>,
            _default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> i32 {
            self.shared.status().blocked_file_dialogs += 1;
            if let Some(callback) = callback {
                callback.cancel();
            }
            1
        }
    }
}

wrap_context_menu_handler! {
    struct NoContextMenu;

    impl ContextMenuHandler {
        fn on_before_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
        ) {
            if let Some(model) = model {
                model.clear();
            }
        }
    }
}
