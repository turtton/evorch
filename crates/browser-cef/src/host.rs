use cef::*;

use crate::input::{HostInput, MouseButton, key_codes};

/// CEF rejects a null range; this is its "no range" value.
const NO_RANGE: Range = Range {
    from: u32::MAX,
    to: u32::MAX,
};

fn mouse(x: i32, y: i32, modifiers: u32) -> MouseEvent {
    MouseEvent { x, y, modifiers }
}

fn key_event(type_: KeyEventType, windows_key_code: i32, modifiers: u32) -> KeyEvent {
    KeyEvent {
        size: std::mem::size_of::<KeyEvent>(),
        type_,
        modifiers,
        windows_key_code,
        ..Default::default()
    }
}

pub(crate) fn apply(host: &BrowserHost, input: &HostInput) {
    match input {
        HostInput::MouseMove {
            x,
            y,
            modifiers,
            leave,
        } => host.send_mouse_move_event(Some(&mouse(*x, *y, *modifiers)), (*leave).into()),
        HostInput::MouseClick {
            x,
            y,
            modifiers,
            button,
            up,
            count,
        } => {
            let button = match button {
                MouseButton::Left => MouseButtonType::LEFT,
                MouseButton::Middle => MouseButtonType::MIDDLE,
                MouseButton::Right => MouseButtonType::RIGHT,
            };
            host.send_mouse_click_event(
                Some(&mouse(*x, *y, *modifiers)),
                button,
                (*up).into(),
                *count,
            );
        }
        HostInput::MouseWheel {
            x,
            y,
            modifiers,
            dx,
            dy,
        } => host.send_mouse_wheel_event(Some(&mouse(*x, *y, *modifiers)), *dx, *dy),
        HostInput::Key {
            key,
            down,
            modifiers,
        } => {
            let Some((windows, evdev)) = key_codes(*key) else {
                return;
            };
            let kind = if *down {
                KeyEventType::RAWKEYDOWN
            } else {
                KeyEventType::KEYUP
            };
            host.send_key_event(Some(&KeyEvent {
                native_key_code: evdev + 8,
                ..key_event(kind, windows, *modifiers)
            }));
        }
        HostInput::Char { unit, modifiers } => host.send_key_event(Some(&KeyEvent {
            character: *unit,
            unmodified_character: *unit,
            ..key_event(KeyEventType::CHAR, i32::from(*unit), *modifiers)
        })),
        HostInput::ImeComposition { text, caret_utf16 } => {
            let length = text.encode_utf16().count() as u32;
            let underline = CompositionUnderline {
                size: std::mem::size_of::<CompositionUnderline>(),
                range: Range {
                    from: 0,
                    to: length,
                },
                color: 0xFF00_0000,
                ..Default::default()
            };
            host.ime_set_composition(
                Some(&CefString::from(text.as_str())),
                Some(&[underline]),
                Some(&NO_RANGE),
                Some(&Range {
                    from: *caret_utf16,
                    to: *caret_utf16,
                }),
            );
        }
        HostInput::ImeCancel => host.ime_cancel_composition(),
        HostInput::ImeCommit(text) => {
            host.ime_commit_text(Some(&CefString::from(text.as_str())), Some(&NO_RANGE), 0);
        }
    }
}

pub(crate) fn cursor_icon(cursor: u32) -> egui::CursorIcon {
    use egui::CursorIcon as Icon;
    const MAP: &[(CursorType, Icon)] = &[
        (CursorType::HAND, Icon::PointingHand),
        (CursorType::IBEAM, Icon::Text),
        (CursorType::VERTICALTEXT, Icon::VerticalText),
        (CursorType::CROSS, Icon::Crosshair),
        (CursorType::WAIT, Icon::Wait),
        (CursorType::PROGRESS, Icon::Progress),
        (CursorType::HELP, Icon::Help),
        (CursorType::GRAB, Icon::Grab),
        (CursorType::GRABBING, Icon::Grabbing),
        (CursorType::MOVE, Icon::Move),
        (CursorType::NOTALLOWED, Icon::NotAllowed),
        (CursorType::NODROP, Icon::NotAllowed),
        (CursorType::EASTWESTRESIZE, Icon::ResizeHorizontal),
        (CursorType::COLUMNRESIZE, Icon::ResizeColumn),
        (CursorType::NORTHSOUTHRESIZE, Icon::ResizeVertical),
        (CursorType::ROWRESIZE, Icon::ResizeRow),
        (CursorType::EASTRESIZE, Icon::ResizeEast),
        (CursorType::WESTRESIZE, Icon::ResizeWest),
        (CursorType::NORTHRESIZE, Icon::ResizeNorth),
        (CursorType::SOUTHRESIZE, Icon::ResizeSouth),
        (CursorType::NORTHEASTRESIZE, Icon::ResizeNorthEast),
        (CursorType::NORTHWESTRESIZE, Icon::ResizeNorthWest),
        (CursorType::SOUTHEASTRESIZE, Icon::ResizeSouthEast),
        (CursorType::SOUTHWESTRESIZE, Icon::ResizeSouthWest),
        (CursorType::CONTEXTMENU, Icon::ContextMenu),
        (CursorType::CELL, Icon::Cell),
        (CursorType::ALIAS, Icon::Alias),
        (CursorType::COPY, Icon::Copy),
        (CursorType::ZOOMIN, Icon::ZoomIn),
        (CursorType::ZOOMOUT, Icon::ZoomOut),
        (CursorType::NONE, Icon::None),
    ];
    MAP.iter()
        .find(|(kind, _)| kind.get_raw() == cursor)
        .map_or(Icon::Default, |(_, icon)| *icon)
}
