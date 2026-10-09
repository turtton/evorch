//! 端末グリッド上のマウス操作 (選択・ホイール・マウス報告)。

use alacritty_terminal::index::Side;
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::TermMode;
use egui::{Event, Key, Modifiers, MouseWheelUnit, PointerButton, Pos2, Rect, Response};

use super::paint::CellMetrics;
use crate::terminal::input::{self, MouseAction, MouseButton};
use crate::terminal::{TerminalSession, ViewportPoint};

/// グリッド上の位置と、セルの左右どちら側かを求めます。
fn cell_at(rect: Rect, cell: CellMetrics, pos: Pos2) -> (ViewportPoint, Side) {
    let x = ((pos.x - rect.left()) / cell.width).max(0.0);
    let y = ((pos.y - rect.top()) / cell.height).max(0.0);
    let side = if x.fract() < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    (
        ViewportPoint {
            line: y as usize,
            column: x as usize,
        },
        side,
    )
}

fn report_button(button: PointerButton) -> Option<MouseButton> {
    match button {
        PointerButton::Primary => Some(MouseButton::Left),
        PointerButton::Middle => Some(MouseButton::Middle),
        PointerButton::Secondary => Some(MouseButton::Right),
        _ => None,
    }
}

/// マウス操作を処理します。
pub(super) fn handle_pointer(
    ui: &egui::Ui,
    response: &Response,
    rect: Rect,
    cell: CellMetrics,
    session: &mut TerminalSession,
) {
    let mode = session.emulator().mode();
    let shift = ui.input(|input| input.modifiers.shift);
    // Shift held bypasses application mouse reporting so text can still be selected.
    let reporting = mode.intersects(TermMode::MOUSE_MODE) && !shift;
    if response.hovered() {
        handle_wheel(ui, rect, cell, session, reporting);
    }
    if reporting {
        report_buttons(ui, response, rect, cell, session);
    } else {
        select(response, rect, cell, session);
    }
}

fn select(response: &Response, rect: Rect, cell: CellMetrics, session: &mut TerminalSession) {
    let Some(pos) = response.interact_pointer_pos() else {
        return;
    };
    let (point, side) = cell_at(rect, cell, pos);
    let emulator = session.emulator_mut();
    if response.triple_clicked_by(PointerButton::Primary) {
        emulator.start_selection(SelectionType::Lines, point, side);
    } else if response.double_clicked_by(PointerButton::Primary) {
        emulator.start_selection(SelectionType::Semantic, point, side);
    } else if response.drag_started_by(PointerButton::Primary) {
        emulator.start_selection(SelectionType::Simple, point, side);
    } else if response.dragged_by(PointerButton::Primary) {
        emulator.update_selection(point, side);
    } else if response.clicked_by(PointerButton::Primary) {
        emulator.clear_selection();
    }
}

fn report_buttons(
    ui: &egui::Ui,
    response: &Response,
    rect: Rect,
    cell: CellMetrics,
    session: &mut TerminalSession,
) {
    let mode = session.emulator().mode();
    let events = ui.input(|input| input.events.clone());
    for event in events {
        let Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers,
        } = event
        else {
            continue;
        };
        if !rect.contains(pos) && pressed {
            continue;
        }
        let Some(button) = report_button(button) else {
            continue;
        };
        let action = if pressed {
            MouseAction::Press
        } else {
            MouseAction::Release
        };
        let (point, _) = cell_at(rect, cell, pos);
        if let Some(bytes) = input::mouse_bytes(button, action, point, modifiers, mode) {
            session.write_raw(&bytes);
        }
    }
    if response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let (point, _) = cell_at(rect, cell, pos);
        let button = [
            PointerButton::Primary,
            PointerButton::Middle,
            PointerButton::Secondary,
        ]
        .into_iter()
        .find(|button| response.dragged_by(*button))
        .and_then(report_button);
        if let Some(button) = button
            && session.last_drag_cell.replace(point) != Some(point)
        {
            let modifiers = ui.input(|input| input.modifiers);
            if let Some(bytes) =
                input::mouse_bytes(button, MouseAction::Drag, point, modifiers, mode)
            {
                session.write_raw(&bytes);
            }
        }
    } else {
        session.last_drag_cell = None;
    }
}

fn handle_wheel(
    ui: &egui::Ui,
    rect: Rect,
    cell: CellMetrics,
    session: &mut TerminalSession,
    reporting: bool,
) {
    let lines_per_page = f32::from(session.emulator().size().lines);
    let (delta, pointer) = ui.input(|input| {
        let delta: f32 = input
            .events
            .iter()
            .filter_map(|event| match event {
                Event::MouseWheel { unit, delta, .. } => Some(match unit {
                    MouseWheelUnit::Point => delta.y / cell.height,
                    MouseWheelUnit::Line => delta.y,
                    MouseWheelUnit::Page => delta.y * lines_per_page,
                }),
                _ => None,
            })
            .sum();
        (delta, input.pointer.hover_pos())
    });
    if delta == 0.0 {
        return;
    }
    session.scroll_remainder += delta;
    let lines = session.scroll_remainder.trunc();
    session.scroll_remainder -= lines;
    let count = lines.abs() as usize;
    if count == 0 {
        return;
    }
    let up = lines > 0.0;
    let mode = session.emulator().mode();
    if reporting {
        let point = pointer.map_or(ViewportPoint { line: 0, column: 0 }, |pos| {
            cell_at(rect, cell, pos).0
        });
        let button = if up {
            MouseButton::WheelUp
        } else {
            MouseButton::WheelDown
        };
        let modifiers = ui.input(|input| input.modifiers);
        if let Some(bytes) = input::mouse_bytes(button, MouseAction::Press, point, modifiers, mode)
        {
            session.write_raw(&bytes.repeat(count));
        }
    } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        // Full-screen apps without mouse support (less, man) scroll with arrow keys.
        let key = if up { Key::ArrowUp } else { Key::ArrowDown };
        if let Some(bytes) = input::key_bytes(key, Modifiers::NONE, mode) {
            session.write_raw(&bytes.repeat(count));
        }
    } else {
        let lines = i32::try_from(count).unwrap_or(i32::MAX);
        session
            .emulator_mut()
            .scroll_lines(if up { lines } else { -lines });
    }
}
