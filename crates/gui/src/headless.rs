//! GUI ウィンドウを生成せず Workbench を実行・描画する API です。

use std::any::Any;
use std::panic::{AssertUnwindSafe, PanicHookInfo, catch_unwind};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use egui::vec2;
use egui_kittest::Harness;

use crate::app::WorkbenchState;
use crate::model::tasks::AgentRunSource;

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

static PANIC_HOOK_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Offscreen 描画で取得した RGBA フレームです。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedFrame {
    /// フレーム幅です。
    pub width: u32,
    /// フレーム高さです。
    pub height: u32,
    /// 行優先の RGBA8 ピクセル列です。
    pub rgba: Vec<u8>,
}

impl CapturedFrame {
    /// フレームを PNG として保存します。
    ///
    /// # Errors
    ///
    /// エンコードまたはファイル書き込みに失敗した場合は
    /// [`OffscreenError::Encode`] を返します。
    pub fn save_png(&self, path: &Path) -> Result<(), OffscreenError> {
        image::save_buffer_with_format(
            path,
            &self.rgba,
            self.width,
            self.height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .map_err(|error| OffscreenError::Encode(error.to_string()))
    }
}

/// Offscreen 描画・PNG 保存の失敗です。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OffscreenError {
    /// 利用可能な wgpu adapter がありません。
    #[error("offscreen adapter unavailable: {0}")]
    AdapterUnavailable(String),
    /// フレーム描画に失敗しました。
    #[error("offscreen rendering failed: {0}")]
    Render(String),
    /// PNG エンコードまたは保存に失敗しました。
    #[error("PNG encoding failed: {0}")]
    Encode(String),
}

/// `egui_kittest` を backend とするウィンドウ不要の Workbench です。
pub struct HeadlessWorkbench<S: AgentRunSource + 'static> {
    harness: Harness<'static, WorkbenchState<S>>,
}

impl<S: AgentRunSource + 'static> HeadlessWorkbench<S> {
    /// 指定サイズの stateful harness を構築します。
    pub fn new(state: WorkbenchState<S>, size: [f32; 2]) -> Self {
        let harness = Harness::builder()
            .with_size(vec2(size[0], size[1]))
            .build_ui_state(
                |ui, state: &mut WorkbenchState<S>| {
                    state.ui(ui, &mut eframe::Frame::_new_kittest());
                },
                state,
            );
        Self { harness }
    }

    /// UI が安定するまでフレームを実行します。
    pub fn run(&mut self) {
        self.harness.run();
    }

    /// 1 フレームだけ実行します。
    pub fn step(&mut self) {
        self.harness.step();
    }

    /// 現在の Workbench 状態を返します。
    pub fn state(&self) -> &WorkbenchState<S> {
        self.harness.state()
    }

    /// 現在の UI を RGBA8 フレームとして取得します。
    ///
    /// # Errors
    ///
    /// adapter が無い場合は [`OffscreenError::AdapterUnavailable`]、その他の描画失敗は
    /// [`OffscreenError::Render`] を返します。backend panic も unwind させません。
    pub fn capture(&mut self) -> Result<CapturedFrame, OffscreenError> {
        let _hook_lock = lock_panic_hook();
        let _hook_guard = PanicHookGuard::suppress();
        let rendered = catch_unwind(AssertUnwindSafe(|| self.harness.render()));

        match rendered {
            Ok(Ok(image)) => {
                let (width, height) = image.dimensions();
                Ok(CapturedFrame {
                    width,
                    height,
                    rgba: image.into_raw(),
                })
            }
            Ok(Err(message)) => Err(classify_render_error(message)),
            Err(payload) => Err(classify_render_error(panic_message(payload.as_ref()))),
        }
    }
}

struct PanicHookGuard {
    previous: Option<PanicHook>,
}

impl PanicHookGuard {
    fn suppress() -> Self {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        Self {
            previous: Some(previous),
        }
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::panic::set_hook(previous);
        }
    }
}

fn lock_panic_hook() -> MutexGuard<'static, ()> {
    match PANIC_HOOK_LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn classify_render_error(message: String) -> OffscreenError {
    if message.to_ascii_lowercase().contains("no adapter found") {
        OffscreenError::AdapterUnavailable(message)
    } else {
        OffscreenError::Render(message)
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "renderer panicked with a non-string payload".to_owned()
    }
}
