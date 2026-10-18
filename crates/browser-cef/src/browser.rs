use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use cef::*;

use crate::artifact::ArtifactContext;
use crate::frame::{PaintBuffer, PixelRect};
use crate::offline::is_allowed_url;
use crate::runtime::{SendCell, on_ui};

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
    pub closed: bool,
}

type Repaint = Box<dyn Fn() + Send + Sync>;

pub(crate) struct Shared {
    view: Mutex<ViewGeometry>,
    pub(crate) frame: Mutex<PaintBuffer>,
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

    fn wait(&self, timeout: Option<Duration>, mut predicate: impl FnMut(&Self) -> bool) -> bool {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
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
    _artifact: ArtifactContext,
}

impl OsrBrowser {
    /// Opens `url` (normally [`ArtifactContext::url`]) at `width`×`height`
    /// DIP rendered at `scale` physical pixels per DIP.
    pub fn create(
        artifact: ArtifactContext,
        url: &str,
        [width, height]: [i32; 2],
        scale: f32,
    ) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            view: Mutex::new(ViewGeometry {
                width: width.max(1),
                height: height.max(1),
                scale,
            }),
            frame: Mutex::default(),
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
            _artifact: artifact,
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

    /// Waits without a deadline for `predicate`, re-checked after each browser
    /// callback; for callers whose runner bounds the wait, such as tests.
    pub fn wait_for(&self, mut predicate: impl FnMut(&Status) -> bool) {
        self.shared.wait(None, |shared| predicate(&shared.status()));
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
        let Some(host) = self.host() else { return };
        let host = SendCell(host);
        on_ui(move || host.0.close_browser(1));
        self.shared
            .wait(Some(CLOSE_TIMEOUT), |shared| shared.status().closed);
        lock(&self.shared.browser).take();
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
            Some(SuppressJsDialogs::new(Arc::clone(&self.shared)))
        }
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(CancelFileDialogs::new(Arc::clone(&self.shared)))
        }
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(NoContextMenu::new())
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
            if type_ != PaintElementType::VIEW || buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let len = width as usize * height as usize * 4;
            // SAFETY: CEF passes a `width * height` BGRA buffer valid for this call.
            let source = unsafe { std::slice::from_raw_parts(buffer, len) };
            let dirty: Vec<_> = dirty_rects.unwrap_or_default().iter().map(pixel_rect).collect();
            lock(&self.shared.frame).paint(source, width, height, &dirty);
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

wrap_jsdialog_handler! {
    struct SuppressJsDialogs {
        shared: Arc<Shared>,
    }

    impl JsdialogHandler {
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            _dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            _default_prompt_text: Option<&CefString>,
            _callback: Option<&mut JsdialogCallback>,
            suppress_message: Option<&mut i32>,
        ) -> i32 {
            self.shared.status().suppressed_dialogs.push(text(message_text));
            if let Some(suppress) = suppress_message {
                *suppress = 1;
            }
            self.shared.notify();
            0
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            _message_text: Option<&CefString>,
            _is_reload: i32,
            callback: Option<&mut JsdialogCallback>,
        ) -> i32 {
            if let Some(callback) = callback {
                callback.cont(1, None);
            }
            1
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
