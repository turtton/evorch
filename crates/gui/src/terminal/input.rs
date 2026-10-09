//! egui の入力を xterm 互換のバイト列へ変換します。

use alacritty_terminal::term::TermMode;
use egui::{Key, Modifiers};

use super::emulator::ViewportPoint;

/// CSI の修飾子パラメータ (1 + shift + alt*2 + ctrl*4) です。修飾なしは `None`。
fn modifier_param(modifiers: Modifiers) -> Option<u8> {
    let value = u8::from(modifiers.shift)
        | (u8::from(modifiers.alt) << 1)
        | (u8::from(modifiers.ctrl || modifiers.command) << 2);
    (value != 0).then_some(value + 1)
}

fn with_alt(modifiers: Modifiers, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if modifiers.alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

/// カーソル系キー (`ESC [ A` / アプリケーションモードでは `ESC O A`) です。
fn cursor_key(final_byte: u8, modifiers: Modifiers, mode: TermMode) -> Vec<u8> {
    match modifier_param(modifiers) {
        Some(param) => format!("\x1b[1;{param}{}", final_byte as char).into_bytes(),
        None if mode.contains(TermMode::APP_CURSOR) => vec![0x1b, b'O', final_byte],
        None => vec![0x1b, b'[', final_byte],
    }
}

/// `ESC [ n ~` 形式の編集・ファンクションキーです。
fn tilde_key(number: u8, modifiers: Modifiers) -> Vec<u8> {
    match modifier_param(modifiers) {
        Some(param) => format!("\x1b[{number};{param}~").into_bytes(),
        None => format!("\x1b[{number}~").into_bytes(),
    }
}

/// F1-F4 (`ESC O P` など) です。
fn ss3_function_key(final_byte: u8, modifiers: Modifiers) -> Vec<u8> {
    match modifier_param(modifiers) {
        Some(param) => format!("\x1b[1;{param}{}", final_byte as char).into_bytes(),
        None => vec![0x1b, b'O', final_byte],
    }
}

fn control_byte(key: Key) -> Option<u8> {
    let name = key.name();
    if name.len() == 1 {
        let ch = name.as_bytes()[0];
        if ch.is_ascii_uppercase() {
            return Some(ch - b'A' + 1);
        }
    }
    Some(match key {
        Key::Space | Key::Num2 => 0x00,
        Key::OpenBracket | Key::Num3 => 0x1b,
        Key::Backslash | Key::Num4 => 0x1c,
        Key::CloseBracket | Key::Num5 => 0x1d,
        Key::Num6 => 0x1e,
        Key::Slash | Key::Minus | Key::Num7 => 0x1f,
        Key::Num8 => 0x7f,
        _ => return None,
    })
}

/// 押下されたキーのバイト列です。文字入力は `Event::Text` 側で扱うため `None` を返します。
pub fn key_bytes(key: Key, modifiers: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let ctrl = modifiers.ctrl || modifiers.command;
    let bytes = match key {
        Key::Enter => with_alt(modifiers, b"\r"),
        Key::Tab if modifiers.shift => b"\x1b[Z".to_vec(),
        Key::Tab => with_alt(modifiers, b"\t"),
        Key::Backspace if ctrl => with_alt(modifiers, b"\x08"),
        Key::Backspace => with_alt(modifiers, b"\x7f"),
        Key::Escape => with_alt(modifiers, b"\x1b"),
        Key::ArrowUp => cursor_key(b'A', modifiers, mode),
        Key::ArrowDown => cursor_key(b'B', modifiers, mode),
        Key::ArrowRight => cursor_key(b'C', modifiers, mode),
        Key::ArrowLeft => cursor_key(b'D', modifiers, mode),
        Key::Home => cursor_key(b'H', modifiers, mode),
        Key::End => cursor_key(b'F', modifiers, mode),
        Key::Insert => tilde_key(2, modifiers),
        Key::Delete => tilde_key(3, modifiers),
        Key::PageUp => tilde_key(5, modifiers),
        Key::PageDown => tilde_key(6, modifiers),
        Key::F1 => ss3_function_key(b'P', modifiers),
        Key::F2 => ss3_function_key(b'Q', modifiers),
        Key::F3 => ss3_function_key(b'R', modifiers),
        Key::F4 => ss3_function_key(b'S', modifiers),
        Key::F5 => tilde_key(15, modifiers),
        Key::F6 => tilde_key(17, modifiers),
        Key::F7 => tilde_key(18, modifiers),
        Key::F8 => tilde_key(19, modifiers),
        Key::F9 => tilde_key(20, modifiers),
        Key::F10 => tilde_key(21, modifiers),
        Key::F11 => tilde_key(23, modifiers),
        Key::F12 => tilde_key(24, modifiers),
        _ if ctrl => with_alt(modifiers, &[control_byte(key)?]),
        _ => return None,
    };
    Some(bytes)
}

/// 文字入力のバイト列です。Alt 併用時は ESC を前置します (meta 送信)。
pub fn text_bytes(text: &str, alt: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 1);
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(text.as_bytes());
    out
}

/// 貼り付けのバイト列です。bracketed paste 有効時は ESC を除去して括ります。
pub fn paste_bytes(text: &str, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = b"\x1b[200~".to_vec();
        out.extend(text.replace('\x1b', "").bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// フォーカス変化の通知 (`?1004h` 有効時のみ) です。
pub fn focus_bytes(focused: bool, mode: TermMode) -> Option<&'static [u8]> {
    mode.contains(TermMode::FOCUS_IN_OUT)
        .then_some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

/// マウス報告のボタン種別です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

impl MouseButton {
    const fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
            Self::WheelUp => 64,
            Self::WheelDown => 65,
        }
    }
}

/// マウスイベントの種類です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Drag,
}

/// アプリケーションがマウス報告を要求している場合のバイト列です。
pub fn mouse_bytes(
    button: MouseButton,
    action: MouseAction,
    point: ViewportPoint,
    modifiers: Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let wants = match action {
        MouseAction::Press | MouseAction::Release => mode.contains(TermMode::MOUSE_REPORT_CLICK),
        MouseAction::Drag => mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
    } || (mode.intersects(TermMode::MOUSE_MODE)
        && matches!(button, MouseButton::WheelUp | MouseButton::WheelDown));
    if !wants {
        return None;
    }
    let mut code = button.code();
    if modifiers.shift {
        code += 4;
    }
    if modifiers.alt {
        code += 8;
    }
    if modifiers.ctrl || modifiers.command {
        code += 16;
    }
    if action == MouseAction::Drag {
        code += 32;
    }
    let column = point.column + 1;
    let line = point.line + 1;
    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if action == MouseAction::Release {
            'm'
        } else {
            'M'
        };
        return Some(format!("\x1b[<{code};{column};{line}{suffix}").into_bytes());
    }
    if action == MouseAction::Release {
        code = (code & !0b11) | 3;
    }
    let encode = |value: usize| u8::try_from(value + 32).ok();
    Some(vec![
        0x1b,
        b'[',
        b'M',
        encode(usize::from(code))?,
        encode(column)?,
        encode(line)?,
    ])
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::term::TermMode;
    use egui::{Key, Modifiers};

    use super::{MouseAction, MouseButton, key_bytes, mouse_bytes, paste_bytes};
    use crate::terminal::emulator::ViewportPoint;

    #[test]
    fn control_and_editing_keys_map_to_xterm_sequences() {
        // Given: the default (normal cursor) mode
        let mode = TermMode::empty();
        let bytes = |key, modifiers| key_bytes(key, modifiers, mode);

        // Then: shell editing keys produce the control codes shells expect
        assert_eq!(
            bytes(Key::C, Modifiers::CTRL).as_deref(),
            Some(&b"\x03"[..])
        );
        assert_eq!(
            bytes(Key::D, Modifiers::CTRL).as_deref(),
            Some(&b"\x04"[..])
        );
        assert_eq!(
            bytes(Key::Enter, Modifiers::NONE).as_deref(),
            Some(&b"\r"[..])
        );
        assert_eq!(
            bytes(Key::Backspace, Modifiers::NONE).as_deref(),
            Some(&b"\x7f"[..])
        );
        assert_eq!(
            bytes(Key::Tab, Modifiers::SHIFT).as_deref(),
            Some(&b"\x1b[Z"[..])
        );
        assert_eq!(
            bytes(Key::Delete, Modifiers::NONE).as_deref(),
            Some(&b"\x1b[3~"[..])
        );
        assert_eq!(
            bytes(Key::ArrowLeft, Modifiers::CTRL).as_deref(),
            Some(&b"\x1b[1;5D"[..])
        );
        // Printable keys are delivered by Event::Text instead.
        assert_eq!(bytes(Key::A, Modifiers::NONE), None);
    }

    #[test]
    fn application_cursor_mode_switches_arrow_encoding() {
        // Given: an application (e.g. vim) that enabled DECCKM
        let mode = TermMode::APP_CURSOR;

        // When: an arrow key is pressed
        let up = key_bytes(Key::ArrowUp, Modifiers::NONE, mode);

        // Then: the SS3 form is sent
        assert_eq!(up.as_deref(), Some(&b"\x1bOA"[..]));
    }

    #[test]
    fn paste_respects_bracketed_mode() {
        // Given: text containing newlines and an escape byte
        let text = "a\nb\x1b";

        // When: pasted with and without bracketed paste
        let plain = paste_bytes(text, TermMode::empty());
        let bracketed = paste_bytes(text, TermMode::BRACKETED_PASTE);

        // Then: newlines become CR, and bracketed paste strips ESC inside the markers
        assert_eq!(plain, b"a\rb\x1b");
        assert_eq!(bracketed, b"\x1b[200~a\nb\x1b[201~");
    }

    #[test]
    fn mouse_reports_follow_the_requested_protocol() {
        // Given: a click at the third column of the second line
        let point = ViewportPoint { line: 1, column: 2 };
        let report =
            |action, mode| mouse_bytes(MouseButton::Left, action, point, Modifiers::NONE, mode);

        // Then: SGR and legacy encodings are produced only when requested
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            report(MouseAction::Press, sgr).as_deref(),
            Some(&b"\x1b[<0;3;2M"[..])
        );
        assert_eq!(
            report(MouseAction::Release, sgr).as_deref(),
            Some(&b"\x1b[<0;3;2m"[..])
        );
        assert_eq!(
            report(MouseAction::Press, TermMode::MOUSE_REPORT_CLICK).as_deref(),
            Some(&[0x1b, b'[', b'M', 32, 35, 34][..])
        );
        assert_eq!(report(MouseAction::Press, TermMode::empty()), None);
        assert_eq!(report(MouseAction::Drag, sgr), None);
    }
}
