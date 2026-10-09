//! 端末グリッドの描画。

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

/// グリッド・選択範囲・カーソルを `rect` へ描きます。
pub(super) fn paint_grid(
    painter: &Painter,
    rect: Rect,
    font: &FontId,
    cell: CellMetrics,
    emulator: &TerminalEmulator,
    focused: bool,
) {
    let term = emulator.term();
    let content = term.renderable_content();
    let overrides: &Colors = content.colors;
    let default_bg = colors::default_background(overrides);
    let cursor_color = colors::cursor(overrides);
    let selection_bg = palette().ACCENT.gamma_multiply(0.45);
    painter.rect_filled(rect, 0.0, default_bg);

    let offset = content.display_offset;
    let cursor = content.cursor;
    let cursor_viewport = point_to_viewport(offset, cursor.point);
    let solid_cursor = focused && cursor.shape == CursorShape::Block;
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
            cursor.shape,
            cursor_color,
            cursor_is_wide(emulator, cursor.point),
        );
    }
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
    shape: CursorShape,
    color: Color32,
    wide: bool,
) {
    let width = if wide { cell.width * 2.0 } else { cell.width };
    let full = Rect::from_min_size(min, Vec2::new(width, cell.height));
    match shape {
        CursorShape::Hidden => {}
        CursorShape::Beam => {
            painter.rect_filled(
                Rect::from_min_size(min, Vec2::new(2.0, cell.height)),
                0.0,
                color,
            );
        }
        CursorShape::Underline => {
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(full.left(), full.bottom() - 2.0), full.max),
                0.0,
                color,
            );
        }
        // Unfocused block cursors are drawn hollow, like most terminals.
        CursorShape::Block | CursorShape::HollowBlock => {
            painter.rect_stroke(
                full.shrink(0.5),
                0.0,
                Stroke::new(1.0, color),
                egui::StrokeKind::Inside,
            );
        }
    }
}
