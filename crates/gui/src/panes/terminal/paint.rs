//! 端末グリッドの描画。

use std::time::Duration;

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::point_to_viewport;
use alacritty_terminal::vte::ansi::CursorShape;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Painter, Pos2, Rect, Stroke, Vec2};

use crate::terminal::{TerminalEmulator, colors};
use crate::theme::tokens::palette;

/// 1 セルの大きさ (ポイント単位) です。
#[derive(Debug, Clone, Copy)]
pub(super) struct CellMetrics {
    pub(super) width: f32,
    pub(super) height: f32,
}

#[derive(Clone, PartialEq)]
struct Style {
    color: Color32,
    italics: bool,
    underline: bool,
    strikethrough: bool,
}

/// 同じスタイルで連続する ASCII セルをまとめた描画単位です。
struct Run {
    origin: Pos2,
    next_column: usize,
    line: usize,
    style: Style,
    text: String,
}

struct Painting<'a> {
    painter: &'a Painter,
    font: &'a FontId,
    origin: Pos2,
    cell: CellMetrics,
    run: Option<Run>,
}

impl Painting<'_> {
    fn cell_pos(&self, line: usize, column: usize) -> Pos2 {
        self.origin
            + Vec2::new(
                column as f32 * self.cell.width,
                line as f32 * self.cell.height,
            )
    }

    fn text(&self, origin: Pos2, text: String, style: &Style) {
        let stroke = |on: bool| {
            if on {
                Stroke::new(1.0, style.color)
            } else {
                Stroke::NONE
            }
        };
        let job = LayoutJob::single_section(
            text,
            TextFormat {
                font_id: self.font.clone(),
                color: style.color,
                italics: style.italics,
                underline: stroke(style.underline),
                strikethrough: stroke(style.strikethrough),
                ..TextFormat::default()
            },
        );
        let galley = self.painter.layout_job(job);
        self.painter.galley(origin, galley, style.color);
    }

    fn flush(&mut self) {
        if let Some(run) = self.run.take() {
            self.text(run.origin, run.text, &run.style);
        }
    }

    /// ASCII は連結して描き、全角や記号は 1 文字ずつセル位置へ固定して描きます。
    fn push(&mut self, line: usize, column: usize, text: String, style: Style) {
        let batchable = text.len() == 1 && text.is_ascii();
        if let Some(run) = &mut self.run
            && batchable
            && run.line == line
            && run.next_column == column
            && run.style == style
        {
            run.text.push_str(&text);
            run.next_column += 1;
            return;
        }
        self.flush();
        let origin = self.cell_pos(line, column);
        if batchable {
            self.run = Some(Run {
                origin,
                next_column: column + 1,
                line,
                style,
                text,
            });
        } else {
            self.text(origin, text, &style);
        }
    }
}

fn cell_text(cell: &Cell) -> String {
    let mut text = String::from(cell.c);
    text.extend(cell.zerowidth().unwrap_or_default());
    text
}

/// カーソルの点滅半周期です。
const BLINK_HALF_PERIOD: Duration = Duration::from_millis(530);

/// ある時刻でのカーソル点滅の位相です。時刻だけで決まるので、描画間隔に依存しません。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CursorBlink {
    /// カーソルを描く位相か。
    pub(super) on: bool,
    /// 次に位相が切り替わるまでの時間。
    pub(super) until_toggle: Duration,
}

impl CursorBlink {
    /// `time` (秒) の位相です。
    pub(super) fn at(time: f64) -> Self {
        let half = BLINK_HALF_PERIOD.as_secs_f64();
        let ticks = (time / half).floor();
        Self {
            on: ticks.rem_euclid(2.0) == 0.0,
            until_toggle: Duration::from_secs_f64(((ticks + 1.0) * half - time).max(0.0)),
        }
    }
}

/// カーソルの描き方です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorLook {
    Hidden,
    /// 塗りつぶしのブロック (文字は反転)。
    Block,
    /// 中抜きの枠。
    Hollow,
    Beam,
    Underline,
}

/// フォーカスと点滅位相からカーソルの見た目を決めます。
///
/// フォーカス中は点滅し、アプリケーションが DECSCUSR で指定した形 (ブロック・ビーム・
/// アンダーライン) を保ちます。フォーカス外は形によらず、点滅しない中抜きの枠です。
fn cursor_look(shape: CursorShape, focused: bool, blink_on: bool) -> CursorLook {
    match shape {
        CursorShape::Hidden => CursorLook::Hidden,
        _ if !focused => CursorLook::Hollow,
        _ if !blink_on => CursorLook::Hidden,
        CursorShape::Beam => CursorLook::Beam,
        CursorShape::Underline => CursorLook::Underline,
        CursorShape::Block | CursorShape::HollowBlock => CursorLook::Block,
    }
}

/// グリッド・選択範囲・カーソルを `rect` へ描きます。
///
/// フォーカス中で点滅するカーソルが画面内にあれば `true` を返します。呼び出し側はその間、
/// 次の位相切り替えに合わせて再描画を要求します。
pub(super) fn paint_grid(
    painter: &Painter,
    rect: Rect,
    font: &FontId,
    cell: CellMetrics,
    emulator: &TerminalEmulator,
    focused: bool,
    blink_on: bool,
) -> bool {
    let term = emulator.term();
    let content = term.renderable_content();
    let overrides: &Colors = content.colors;
    let default_bg = colors::default_background(overrides);
    let cursor_color = colors::cursor(overrides);
    let selection_bg = palette().ACCENT.gamma_multiply(0.45);
    painter.rect_filled(rect, 0.0, default_bg);

    let offset = content.display_offset;
    let cursor = content.cursor;
    // Scrolled back, the cursor row sits below the viewport; it is then neither drawn nor blinking.
    let screen_lines = term.grid().screen_lines();
    let cursor_viewport =
        point_to_viewport(offset, cursor.point).filter(|viewport| viewport.line < screen_lines);
    let look = cursor_look(cursor.shape, focused, blink_on);
    let solid_cursor = look == CursorLook::Block;
    let selection = content.selection;

    let mut painting = Painting {
        painter,
        font,
        origin: rect.min,
        cell,
        run: None,
    };
    for indexed in content.display_iter {
        let Some(viewport) = point_to_viewport(offset, indexed.point) else {
            continue;
        };
        let flags = indexed.cell.flags;
        if flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let (line, column) = (viewport.line, viewport.column.0);
        let mut fg = colors::foreground(indexed.cell.fg, flags, overrides);
        let mut bg = colors::background(indexed.cell.bg, overrides);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if selection.is_some_and(|range| range.contains(indexed.point)) {
            bg = selection_bg;
        }
        let is_cursor = cursor_viewport == Some(viewport);
        if is_cursor && solid_cursor {
            fg = default_bg;
            bg = cursor_color;
        }
        let width = if flags.contains(Flags::WIDE_CHAR) {
            2.0
        } else {
            1.0
        };
        if bg != default_bg {
            let min = painting.cell_pos(line, column);
            painter.rect_filled(
                Rect::from_min_size(min, Vec2::new(cell.width * width, cell.height)),
                0.0,
                bg,
            );
        }
        if indexed.cell.c == ' ' || flags.contains(Flags::HIDDEN) {
            continue;
        }
        let style = Style {
            color: fg,
            italics: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
            strikethrough: flags.contains(Flags::STRIKEOUT),
        };
        painting.push(line, column, cell_text(indexed.cell), style);
    }
    painting.flush();

    if let Some(viewport) = cursor_viewport
        && !solid_cursor
    {
        paint_cursor(
            painter,
            painting.cell_pos(viewport.line, viewport.column.0),
            cell,
            look,
            cursor_color,
            cursor_is_wide(emulator, cursor.point),
        );
    }
    focused && cursor_viewport.is_some() && cursor.shape != CursorShape::Hidden
}

fn cursor_is_wide(emulator: &TerminalEmulator, point: Point) -> bool {
    emulator.term().grid()[point]
        .flags
        .contains(Flags::WIDE_CHAR)
}

fn paint_cursor(
    painter: &Painter,
    min: Pos2,
    cell: CellMetrics,
    look: CursorLook,
    color: Color32,
    wide: bool,
) {
    let width = if wide { cell.width * 2.0 } else { cell.width };
    let full = Rect::from_min_size(min, Vec2::new(width, cell.height));
    match look {
        CursorLook::Hidden | CursorLook::Block => {}
        CursorLook::Beam => {
            painter.rect_filled(
                Rect::from_min_size(min, Vec2::new(2.0, cell.height)),
                0.0,
                color,
            );
        }
        CursorLook::Underline => {
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(full.left(), full.bottom() - 2.0), full.max),
                0.0,
                color,
            );
        }
        CursorLook::Hollow => {
            painter.rect_stroke(
                full.shrink(0.5),
                0.0,
                Stroke::new(1.0, color),
                egui::StrokeKind::Inside,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blink_alternates_each_half_period_and_reports_the_next_toggle() {
        let half = BLINK_HALF_PERIOD.as_secs_f64();
        let early = CursorBlink::at(0.1);
        assert!(early.on);
        assert!((early.until_toggle.as_secs_f64() - (half - 0.1)).abs() < 1e-9);

        let late = CursorBlink::at(half + 0.1);
        assert!(!late.on);
        assert!((late.until_toggle.as_secs_f64() - (half - 0.1)).abs() < 1e-9);

        assert!(CursorBlink::at(2.0 * half + 0.1).on);
        assert!(CursorBlink::at(0.0).until_toggle <= BLINK_HALF_PERIOD);
    }

    #[test]
    fn focused_cursor_keeps_the_requested_shape_and_blinks() {
        for (shape, look) in [
            (CursorShape::Block, CursorLook::Block),
            (CursorShape::HollowBlock, CursorLook::Block),
            (CursorShape::Beam, CursorLook::Beam),
            (CursorShape::Underline, CursorLook::Underline),
        ] {
            assert_eq!(cursor_look(shape, true, true), look);
            assert_eq!(cursor_look(shape, true, false), CursorLook::Hidden);
        }
    }

    #[test]
    fn unfocused_cursor_is_a_steady_hollow_box_whatever_the_shape() {
        for shape in [
            CursorShape::Block,
            CursorShape::HollowBlock,
            CursorShape::Beam,
            CursorShape::Underline,
        ] {
            for blink_on in [true, false] {
                assert_eq!(cursor_look(shape, false, blink_on), CursorLook::Hollow);
            }
        }
    }

    #[test]
    fn a_hidden_cursor_stays_hidden() {
        for focused in [true, false] {
            for blink_on in [true, false] {
                assert_eq!(
                    cursor_look(CursorShape::Hidden, focused, blink_on),
                    CursorLook::Hidden
                );
            }
        }
    }

    // Given: output taller than the grid, scrolled back so the cursor row is below the viewport.
    // Then: no blinking cursor is reported, so the blink timer stops.
    #[test]
    fn a_cursor_scrolled_out_of_view_does_not_blink() {
        let mut emulator = TerminalEmulator::new(crate::terminal::GridSize::new(10, 3));
        emulator.feed(b"1\r\n2\r\n3\r\n4\r\n5");
        let ctx = egui::Context::default();
        let blinks = |emulator: &TerminalEmulator| {
            let mut blinks = false;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 30.0));
                let cell = CellMetrics {
                    width: 8.0,
                    height: 10.0,
                };
                blinks = paint_grid(
                    &ui.painter().clone(),
                    rect,
                    &FontId::monospace(10.0),
                    cell,
                    emulator,
                    true,
                    true,
                );
            });
            output.textures_delta.clear();
            blinks
        };

        assert!(blinks(&emulator), "the cursor is on screen at the bottom");
        emulator.scroll_lines(2);
        assert!(
            !blinks(&emulator),
            "scrolled back, the cursor is off screen"
        );
    }
}
