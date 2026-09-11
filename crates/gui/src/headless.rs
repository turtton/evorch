//! GUI ウィンドウを生成せず Workbench を実行・描画する API です。

use std::any::Any;
use std::panic::{AssertUnwindSafe, PanicHookInfo, catch_unwind};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use egui::vec2;
use egui::{Key, Modifiers};
use egui_kittest::{Harness, kittest::Queryable};

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
        Self::with_pixels_per_point(state, size, 1.0)
    }

    /// 論理ポイント単位のサイズと DPI スケールを指定して構築します。
    pub fn with_pixels_per_point(
        state: WorkbenchState<S>,
        size: [f32; 2],
        pixels_per_point: f32,
    ) -> Self {
        debug_assert!(pixels_per_point.is_finite() && pixels_per_point > 0.0);
        let harness = Harness::builder()
            .with_size(vec2(size[0], size[1]))
            .with_pixels_per_point(pixels_per_point)
            .build_ui_state(
                |ui, state: &mut WorkbenchState<S>| {
                    state.ui(ui, &mut eframe::Frame::_new_kittest());
                },
                state,
            );
        Self { harness }
    }

    /// 現在の論理ポイントあたりのピクセル数を返します。
    pub fn pixels_per_point(&self) -> f32 {
        self.harness.ctx.pixels_per_point()
    }

    /// 画面全体の矩形を論理ポイント単位で返します。
    pub fn screen_rect(&self) -> egui::Rect {
        self.harness.ctx.viewport_rect()
    }

    /// アニメーション中も停止を待たずに固定フレームを実行します。
    pub fn run(&mut self) {
        // Nested modal and scroll-area sizing needs multiple passes before click coordinates settle.
        self.harness.run_steps(16);
    }

    /// 1 フレームだけ実行します。
    pub fn step(&mut self) {
        self.harness.step();
    }

    /// 現在の Workbench 状態を返します。
    pub fn state(&self) -> &WorkbenchState<S> {
        self.harness.state()
    }

    /// 現在の Workbench 状態を可変で返します。
    pub fn state_mut(&mut self) -> &mut WorkbenchState<S> {
        self.harness.state_mut()
    }

    /// 指定ラベルの UI node をクリックします。
    pub fn click_label(&self, label: &str) {
        self.harness.get_by_label(label).click();
    }

    /// 指定ラベルを表示するスクロール要求を次フレームへ送ります。
    pub fn scroll_label_into_view(&self, label: &str) {
        self.harness.get_by_label(label).scroll_to_me();
    }

    /// 指定ラベルの UI node が存在するか返します。
    pub fn has_label(&self, label: &str) -> bool {
        self.harness.query_by_label(label).is_some()
    }

    /// 指定ラベルに一致する UI node の数を返します。
    pub fn count_labels(&self, label: &str) -> usize {
        self.harness.query_all_by_label(label).count()
    }

    /// 指定ラベルに一致する UI node の矩形を出現順で返します（geometry 検証用）。
    pub fn label_rects(&self, label: &str) -> Vec<egui::Rect> {
        self.harness
            .query_all_by_label(label)
            .map(|node| node.rect())
            .collect()
    }

    /// modifier 付きキー入力を次フレームへ送ります。
    pub fn key_press(&self, modifiers: Modifiers, key: Key) {
        self.harness.key_press_modifiers(modifiers, key);
    }

    /// ポインタを指定座標へ移動します（hover 状態の capture 用）。
    pub fn pointer_move(&self, pos: egui::Pos2) {
        self.harness.hover_at(pos);
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
