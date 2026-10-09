//! alacritty_terminal を包む VT エミュレータ。
//!
//! PTY から受け取ったバイト列を解釈し、描画用のグリッド・カーソル・モードを保持します。
//! 端末から PTY への応答 (DSR / 色問い合わせなど) は [`TerminalEmulator::take_responses`]
//! で取り出し、呼び出し側が PTY へ書き戻します。

use std::sync::{Arc, Mutex, PoisonError};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode, viewport_to_point};
use alacritty_terminal::vte::ansi::Processor;

use super::colors;

/// スクロールバックとして保持する最大行数です。
pub const SCROLLBACK_LINES: usize = 10_000;

/// 端末グリッドの列数と行数です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub columns: u16,
    pub lines: u16,
}

impl GridSize {
    pub const DEFAULT: Self = Self {
        columns: 80,
        lines: 24,
    };

    pub fn new(columns: u16, lines: u16) -> Self {
        Self {
            columns: columns.max(2),
            lines: lines.max(1),
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.lines)
    }

    fn columns(&self) -> usize {
        usize::from(self.columns)
    }
}

/// 表示中の領域を基準にしたセル座標です (`line` は 0 が表示領域の先頭)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportPoint {
    pub line: usize,
    pub column: usize,
}

/// `Term` が発行するイベントを UI スレッドで処理するまで溜めておきます。
#[derive(Clone, Default)]
pub struct EventProxy(Arc<Mutex<Vec<Event>>>);

impl EventProxy {
    fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }
}

/// VT エミュレータの状態です。
pub struct TerminalEmulator {
    term: Term<EventProxy>,
    parser: Processor,
    events: EventProxy,
    size: GridSize,
    cell_pixels: (u16, u16),
    responses: Vec<u8>,
    clipboard: Option<String>,
    title: Option<String>,
}

impl TerminalEmulator {
    pub fn new(size: GridSize) -> Self {
        let events = EventProxy::default();
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
            size,
            cell_pixels: (8, 16),
            responses: Vec::new(),
            clipboard: None,
            title: None,
        }
    }

    /// PTY 出力を解釈して画面へ反映します。
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.process_events();
    }

    /// 画面を初期状態へ戻します (再起動時に使用)。
    pub fn reset(&mut self) {
        let cell_pixels = self.cell_pixels;
        *self = Self::new(self.size);
        self.cell_pixels = cell_pixels;
    }

    pub const fn size(&self) -> GridSize {
        self.size
    }

    /// グリッドの大きさを変更します。変化があった場合だけ `true` を返します。
    pub fn resize(&mut self, size: GridSize) -> bool {
        if size == self.size {
            return false;
        }
        self.size = size;
        self.term.resize(size);
        true
    }

    /// テキスト領域サイズ問い合わせ (CSI 14 t) に返すセルのピクセルサイズです。
    pub fn set_cell_pixels(&mut self, width: u16, height: u16) {
        self.cell_pixels = (width, height);
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    pub const fn term(&self) -> &Term<EventProxy> {
        &self.term
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// PTY へ書き戻すべき応答を取り出します。
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// OSC 52 でアプリケーションが要求したクリップボード内容を取り出します。
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// 表示位置を行単位でスクロールします (正の値で過去方向)。
    pub fn scroll_lines(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_page(&mut self, up: bool) {
        self.term
            .scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    pub fn start_selection(&mut self, ty: SelectionType, point: ViewportPoint, side: Side) {
        let point = self.grid_point(point);
        self.term.selection = Some(Selection::new(ty, point, side));
    }

    pub fn update_selection(&mut self, point: ViewportPoint, side: Side) {
        let point = self.grid_point(point);
        if let Some(selection) = &mut self.term.selection {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// 選択範囲の文字列です。空の選択は `None` を返します。
    pub fn selection_text(&self) -> Option<String> {
        self.term
            .selection
            .as_ref()
            .filter(|selection| !selection.is_empty())?;
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    /// 表示中の各行を、末尾の空白を除いた文字列として返します。
    pub fn screen_lines(&self) -> Vec<String> {
        let grid = self.term.grid();
        let offset = self.display_offset() as i32;
        (0..grid.screen_lines() as i32)
            .map(|line| {
                let row = &grid[Line(line - offset)];
                let mut text = String::new();
                for column in 0..grid.columns() {
                    let cell = &row[Column(column)];
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    text.push(cell.c);
                    text.extend(cell.zerowidth().unwrap_or_default());
                }
                text.trim_end().to_owned()
            })
            .collect()
    }

    /// 表示中の内容が空白だけかどうかです。
    pub fn is_blank(&self) -> bool {
        self.screen_lines().iter().all(String::is_empty)
    }

    fn grid_point(&self, point: ViewportPoint) -> Point {
        let line = point.line.min(usize::from(self.size.lines) - 1);
        let column = point.column.min(usize::from(self.size.columns) - 1);
        viewport_to_point(self.display_offset(), Point::new(line, Column(column)))
    }

    fn process_events(&mut self) {
        for event in self.events.take() {
            match event {
                Event::PtyWrite(text) => self.responses.extend_from_slice(text.as_bytes()),
                Event::Title(title) => self.title = Some(title),
                Event::ResetTitle => self.title = None,
                Event::ClipboardStore(_, text) => self.clipboard = Some(text),
                Event::ColorRequest(index, format) => {
                    let rgb = colors::resolve_rgb(index, self.term.colors());
                    self.responses.extend_from_slice(format(rgb).as_bytes());
                }
                Event::TextAreaSizeRequest(format) => {
                    let size = WindowSize {
                        num_lines: self.size.lines,
                        num_cols: self.size.columns,
                        cell_width: self.cell_pixels.0,
                        cell_height: self.cell_pixels.1,
                    };
                    self.responses.extend_from_slice(format(size).as_bytes());
                }
                // OSC 52 の読み出しは既定設定 (OnlyCopy) で拒否されます。他は描画側で毎フレーム反映します。
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::index::Side;
    use alacritty_terminal::selection::SelectionType;
    use alacritty_terminal::term::TermMode;

    use super::{GridSize, TerminalEmulator, ViewportPoint};

    fn emulator(columns: u16, lines: u16) -> TerminalEmulator {
        TerminalEmulator::new(GridSize::new(columns, lines))
    }

    #[test]
    fn cursor_movement_and_erase_sequences_rewrite_the_screen() {
        // Given: a shell-like redraw that moves the cursor and erases the line tail
        let mut term = emulator(20, 3);

        // When: output overwrites text with CR, CUP and EL
        term.feed(b"hello world\rHELLO\x1b[K\r\n\x1b[3;5Hend");

        // Then: the grid reflects the terminal semantics, not the raw bytes
        assert_eq!(term.screen_lines(), ["HELLO", "", "    end"]);
    }

    #[test]
    fn wide_characters_occupy_two_cells() {
        // Given: CJK output followed by an ASCII marker
        let mut term = emulator(10, 1);

        // When: the text is printed
        term.feed("日本x".as_bytes());

        // Then: the marker lands after two double-width cells
        assert_eq!(term.screen_lines(), ["日本x"]);
        let row = &term.term().grid()[alacritty_terminal::index::Line(0)];
        assert_eq!(row[alacritty_terminal::index::Column(4)].c, 'x');
    }

    #[test]
    fn alternate_screen_restores_the_primary_screen() {
        // Given: a primary screen with shell output
        let mut term = emulator(10, 2);
        term.feed(b"prompt$");

        // When: a full-screen app enters and leaves the alternate screen
        term.feed(b"\x1b[?1049h\x1b[2J\x1b[Hvim");
        let in_alt = term.screen_lines();
        let alt_mode = term.mode().contains(TermMode::ALT_SCREEN);
        term.feed(b"\x1b[?1049l");

        // Then: the alternate screen is isolated from the primary one
        assert_eq!(in_alt, ["vim", ""]);
        assert!(alt_mode);
        assert_eq!(term.screen_lines(), ["prompt$", ""]);
    }

    #[test]
    fn scrollback_keeps_lines_that_left_the_screen() {
        // Given: more output than the screen can show
        let mut term = emulator(10, 2);
        term.feed(b"one\r\ntwo\r\nthree");

        // When: the view scrolls back into history
        term.scroll_lines(1);

        // Then: the earlier line is visible again until the view returns to the bottom
        assert_eq!(term.screen_lines(), ["one", "two"]);
        term.scroll_to_bottom();
        assert_eq!(term.screen_lines(), ["two", "three"]);
    }

    #[test]
    fn device_status_report_is_answered_through_responses() {
        // Given: an application asking for the cursor position
        let mut term = emulator(10, 3);
        term.feed(b"ab\x1b[6n");

        // When: responses are drained
        let response = term.take_responses();

        // Then: the reply is queued for the PTY
        assert_eq!(response, b"\x1b[1;3R");
        assert!(term.take_responses().is_empty());
    }

    #[test]
    fn selection_returns_the_selected_text() {
        // Given: two lines of output
        let mut term = emulator(10, 2);
        term.feed(b"first\r\nsecond");

        // When: a drag selects from the middle of the first line into the second
        term.start_selection(
            SelectionType::Simple,
            ViewportPoint { line: 0, column: 2 },
            Side::Left,
        );
        term.update_selection(ViewportPoint { line: 1, column: 2 }, Side::Right);

        // Then: the selected text spans the line break
        assert_eq!(term.selection_text().as_deref(), Some("rst\nsec"));
        term.clear_selection();
        assert_eq!(term.selection_text(), None);
    }

    #[test]
    fn resize_rewraps_to_the_new_width() {
        // Given: a line wider than the next width
        let mut term = emulator(10, 3);
        term.feed(b"abcdefgh");

        // When: the grid shrinks
        let changed = term.resize(GridSize::new(4, 3));

        // Then: the size changes and the line wraps, keeping its head in history
        assert!(changed);
        assert!(!term.resize(GridSize::new(4, 3)));
        assert_eq!(term.screen_lines()[0], "efgh");
        term.scroll_lines(1);
        assert_eq!(term.screen_lines()[..2], ["abcd", "efgh"]);
    }

    #[test]
    fn title_and_clipboard_requests_are_exposed() {
        // Given: an app setting the title and copying through OSC 52
        let mut term = emulator(10, 1);

        // When: both sequences are printed
        term.feed(b"\x1b]2;build\x07\x1b]52;c;aGk=\x07");

        // Then: the UI can show the title and store the clipboard text
        assert_eq!(term.title(), Some("build"));
        assert_eq!(term.take_clipboard().as_deref(), Some("hi"));
    }
}
