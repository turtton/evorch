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
    CornerRadius, Event, EventFilter, ImeEvent, Key, Rect, Sense, Stroke, StrokeKind, TextStyle,
    UiBuilder, Vec2, WidgetInfo, WidgetType,
};

use crate::terminal::{
    GridSize, TerminalKey, TerminalSession, TerminalSessions, TerminalStatus, input,
};
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::{R_MD, SP_1, SP_2, palette};
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
    let frame = surface_frame(palette().INPUT)
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
        });
    let focused = frame.inner;
    if focused {
        // The frame has already drawn its neutral border; the accent goes over it.
        ui.painter().rect_stroke(
            frame.response.rect,
            CornerRadius::same(R_MD),
            Stroke::new(1.0, palette().ACCENT),
            StrokeKind::Inside,
        );
    }
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
    // egui only gives focus up on a *click* elsewhere, so pressing to start a text selection
    // elsewhere would leave the terminal grabbing Ctrl+C. Release it as soon as the button
    // goes down outside the grid.
    if pressed_outside(ui, rect) {
        response.surrender_focus();
    }
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

    let blink = paint::CursorBlink::at(ui.input(|input| input.time));
    let cursor_blinks = paint::paint_grid(
        &ui.painter_at(rect),
        rect,
        &font,
        cell,
        session.emulator(),
        focused,
        blink.on,
    );
    if cursor_blinks {
        // Wake only for the next toggle; an idle terminal does not repaint every frame.
        ui.ctx().request_repaint_after(blink.until_toggle);
    }
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

/// このフレームで、`rect` の外でマウスボタンが押されたか。
fn pressed_outside(ui: &egui::Ui, rect: Rect) -> bool {
    ui.input(|input| {
        input.events.iter().any(|event| {
            matches!(
                event,
                Event::PointerButton { pos, pressed: true, .. } if !rect.contains(*pos)
            )
        })
    })
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use egui::epaint::Shape;

    use super::*;

    const SIZE: Vec2 = Vec2::new(400.0, 300.0);

    struct Pane {
        ctx: egui::Context,
        sessions: TerminalSessions,
        cwd: PathBuf,
    }

    struct Frame {
        repaint_delay: Duration,
        accent_border: bool,
    }

    impl Pane {
        fn new() -> Self {
            let mut pane = Self {
                ctx: egui::Context::default(),
                sessions: TerminalSessions::default(),
                cwd: PathBuf::from("/"),
            };
            // A fresh context owes a few immediate frames; pass through them to reach rest.
            for _ in 0..4 {
                pane.frame(Vec::new());
            }
            pane
        }

        fn frame(&mut self, events: Vec<Event>) -> Frame {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, SIZE)),
                time: Some(0.1),
                events,
                ..egui::RawInput::default()
            };
            // The workbench clears this at the start of every frame, before the pane asserts it.
            self.sessions.focused = false;
            let (sessions, cwd) = (&mut self.sessions, &self.cwd);
            let mut output = self.ctx.run_ui(raw, |ui| {
                terminal_pane(ui, sessions, &None, cwd);
            });
            output.textures_delta.clear();
            let accent = palette().ACCENT;
            Frame {
                repaint_delay: output.viewport_output[&egui::ViewportId::ROOT].repaint_delay,
                accent_border: output.shapes.iter().any(|clipped| {
                    matches!(&clipped.shape, Shape::Rect(rect) if rect.stroke.color == accent)
                }),
            }
        }

        fn button(&mut self, pos: egui::Pos2, pressed: bool) -> Frame {
            self.frame(vec![
                Event::PointerMoved(pos),
                Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ])
        }

        fn click_grid(&mut self) -> Frame {
            let pos = egui::pos2(SIZE.x / 2.0, SIZE.y / 2.0);
            self.frame(vec![Event::PointerMoved(pos)]);
            self.button(pos, true);
            self.button(pos, false);
            self.frame(Vec::new())
        }
    }

    // Given: an idle terminal / When: it gains and loses focus / Then: only the focused pane wears the accent border and asks for blink wake-ups.
    #[test]
    fn focus_draws_the_accent_border_and_schedules_blink_repaints_only_while_focused() {
        let mut pane = Pane::new();
        let idle = pane.frame(Vec::new());
        assert!(!idle.accent_border);
        assert_eq!(idle.repaint_delay, Duration::MAX);

        let focused = pane.click_grid();
        assert!(pane.sessions.focused);
        assert!(focused.accent_border);
        assert!(
            focused.repaint_delay <= Duration::from_secs(1),
            "blink needs a timed wake-up, got {:?}",
            focused.repaint_delay
        );

        // A press in the pane's margin is outside the grid.
        let released = pane.button(egui::pos2(2.0, 2.0), true);
        assert!(!pane.sessions.focused);
        assert!(!released.accent_border);
        pane.button(egui::pos2(2.0, 2.0), false);
        let mut after = pane.frame(Vec::new());
        for _ in 0..4 {
            after = pane.frame(Vec::new());
        }
        assert_eq!(after.repaint_delay, Duration::MAX);
    }
}
