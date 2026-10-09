//! Terminal ペインの描画と入力処理。
//!
//! 選択中プロジェクトのシェルセッションを、ペインの大きさに合わせたグリッドとして描きます。
//! キーボード入力は `WorkbenchState::raw_input_hook` が取り分けた
//! [`TerminalSessions::pending_events`] から読み、PTY へ直接送ります。

mod paint;
mod pointer;

use std::path::Path;
use std::sync::Arc;

use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::point_to_viewport;
use egui::{
    Event, EventFilter, ImeEvent, Key, Rect, Sense, TextStyle, UiBuilder, Vec2, WidgetInfo,
    WidgetType,
};

use crate::terminal::{
    GridSize, TerminalKey, TerminalSession, TerminalSessions, TerminalStatus, input,
};
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::{SP_1, SP_2, palette};
use crate::theme::widgets::{empty_state, icon_button, surface_frame};
use paint::CellMetrics;

/// Terminal ペインを描画します。
pub fn terminal_pane(
    ui: &mut egui::Ui,
    sessions: &mut TerminalSessions,
    key: &TerminalKey,
    cwd: &Path,
) {
    let spawner = sessions.spawner();
    let pending = std::mem::take(&mut sessions.pending_events);
    let session = sessions.session_mut(key, cwd);
    let focused = surface_frame(palette().INPUT)
        .inner_margin(egui::Margin::same(SP_2 as i8))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            header(ui, session, spawner.is_some());
            ui.add_space(SP_1);
            let focused = grid(ui, session, spawner.is_some());
            if let Some(spawner) = &spawner {
                let ctx = ui.ctx().clone();
                session.ensure_started(spawner.as_ref(), Arc::new(move || ctx.request_repaint()));
            }
            if focused {
                handle_keys(ui, session, pending);
            }
            focused
        })
        .inner;
    sessions.focused |= focused;
}

fn display_path(path: &Path) -> String {
    if let Some(home) = std::env::home_dir()
        && let Ok(relative) = path.strip_prefix(&home)
    {
        return if relative.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", relative.display())
        };
    }
    path.display().to_string()
}

fn header(ui: &mut egui::Ui, session: &mut TerminalSession, has_spawner: bool) {
    ui.horizontal(|ui| {
        ui.label(muted(icons::with_icon(
            icons::FOLDER_SIMPLE,
            display_path(session.cwd()),
        )));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if has_spawner && icon_button(ui, icons::ARROW_CLOCKWISE, "Restart terminal").clicked()
            {
                session.restart();
            }
            match session.status() {
                TerminalStatus::Exited(code) => {
                    ui.label(muted(format!(
                        "Exited with code {code} · press Enter to restart"
                    )));
                }
                TerminalStatus::Failed(error) => {
                    ui.colored_label(
                        palette().ERROR_FG,
                        format!("Failed to start shell: {error}"),
                    );
                }
                TerminalStatus::Running => {
                    if let Some(title) = session.emulator().title() {
                        ui.add(egui::Label::new(muted(title)).truncate());
                    }
                }
                TerminalStatus::NotStarted => {}
            }
        });
    });
}

/// グリッドを確保・描画し、フォーカスとマウス操作を処理します。フォーカス中なら `true`。
fn grid(ui: &mut egui::Ui, session: &mut TerminalSession, has_spawner: bool) -> bool {
    let font = TextStyle::Monospace.resolve(ui.style());
    let cell = ui.fonts_mut(|fonts| CellMetrics {
        width: fonts.glyph_width(&font, 'M'),
        height: fonts.row_height(&font),
    });
    let rect = ui.available_rect_before_wrap();
    let size = GridSize::new(
        (rect.width() / cell.width).floor() as u16,
        (rect.height() / cell.height).floor() as u16,
    );
    session.resize(size);
    let pixels = ui.ctx().pixels_per_point();
    session.emulator_mut().set_cell_pixels(
        (cell.width * pixels).round() as u16,
        (cell.height * pixels).round() as u16,
    );

    let id = ui.id().with("terminal-grid");
    let response = ui.interact(rect, id, Sense::click_and_drag());
    ui.advance_cursor_after_rect(rect);
    if response.clicked() || response.drag_started() || response.secondary_clicked() {
        response.request_focus();
    }
    let focused = response.has_focus();
    if focused {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                id,
                EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            );
        });
    }
    session.set_focused(focused);
    response.widget_info(|| {
        let mut info = WidgetInfo::labeled(WidgetType::Other, true, "Terminal");
        info.current_text_value = Some(session.emulator().screen_lines().join("\n"));
        info
    });

    pointer::handle_pointer(ui, &response, rect, cell, session);
    response.context_menu(|ui| context_menu(ui, session));
    if let Some(text) = session.emulator_mut().take_clipboard() {
        ui.ctx().copy_text(text);
    }

    paint::paint_grid(
        &ui.painter_at(rect),
        rect,
        &font,
        cell,
        session.emulator(),
        focused,
    );
    if !has_spawner
        && session.status() == &TerminalStatus::NotStarted
        && session.emulator().is_blank()
    {
        ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            empty_state(
                ui,
                "Waiting for terminal connection",
                "Shell output will appear when a terminal session is connected.",
                None,
            );
        });
    }
    if focused {
        publish_ime_area(ui, rect, cell, session);
    }
    focused
}

fn context_menu(ui: &mut egui::Ui, session: &mut TerminalSession) {
    let selection = session.emulator().selection_text();
    if ui
        .add_enabled(selection.is_some(), egui::Button::new("Copy"))
        .clicked()
    {
        if let Some(text) = selection {
            ui.ctx().copy_text(text);
        }
        ui.close();
    }
    if ui.button("Paste").clicked() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        ui.close();
    }
}

/// IME の候補ウィンドウをカーソル位置へ出すため、入力領域を platform へ伝えます。
fn publish_ime_area(ui: &egui::Ui, rect: Rect, cell: CellMetrics, session: &TerminalSession) {
    let term = session.emulator().term();
    let offset = term.grid().display_offset();
    let Some(cursor) = point_to_viewport(offset, term.grid().cursor.point) else {
        return;
    };
    let min = rect.min
        + Vec2::new(
            cursor.column.0 as f32 * cell.width,
            cursor.line as f32 * cell.height,
        );
    let cursor_rect = Rect::from_min_size(min, Vec2::new(1.0, cell.height));
    ui.ctx().output_mut(|output| {
        output.ime = Some(egui::output::IMEOutput {
            purpose: egui::IMEPurpose::Normal,
            rect,
            cursor_rect,
            should_interrupt_composition: false,
        });
    });
}

fn handle_keys(ui: &egui::Ui, session: &mut TerminalSession, events: Vec<Event>) {
    let current = ui.input(|input| input.modifiers);
    for event in events {
        let mode = session.emulator().mode();
        let exited = matches!(session.status(), TerminalStatus::Exited(_));
        match event {
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                if exited {
                    if key == Key::Enter {
                        session.restart();
                    }
                } else if modifiers.shift_only()
                    && matches!(key, Key::PageUp | Key::PageDown)
                    && !mode.contains(TermMode::ALT_SCREEN)
                {
                    session.emulator_mut().scroll_page(key == Key::PageUp);
                } else if let Some(bytes) = input::key_bytes(key, modifiers, mode) {
                    session.write_input(&bytes);
                }
            }
            Event::Text(text) if !text.chars().any(char::is_control) => {
                session.write_input(&input::text_bytes(&text, current.alt));
            }
            Event::Ime(ImeEvent::Commit(text)) => session.write_input(text.as_bytes()),
            Event::Paste(text) => session.write_input(&input::paste_bytes(&text, mode)),
            Event::Copy => {
                // Ctrl+Shift+C always copies; Ctrl+C copies only while text is selected.
                let selection = session.emulator().selection_text();
                if current.shift || selection.is_some() {
                    if let Some(text) = selection {
                        ui.ctx().copy_text(text);
                    }
                    session.emulator_mut().clear_selection();
                } else {
                    session.write_input(b"\x03");
                }
            }
            Event::Cut if !current.shift => session.write_input(b"\x18"),
            _ => {}
        }
    }
}
