//! egui input to the DevTools input methods the renderer allows
//! (`Input.dispatchMouseEvent`, `Input.dispatchKeyEvent`,
//! `Input.imeSetComposition` and `Input.insertText`, and nothing else).
//!
//! Three rules shape it:
//!
//! * Coordinates are CSS pixels of the page. The widget shows the frame
//!   stretched over its rectangle, so a point is mapped by the ratio between
//!   the size of what is shown and the rectangle, and the display scale of the
//!   window does not enter.
//! * The queue is bounded and coalesces what may be: a move replaces a move that
//!   is still last in the queue, and consecutive wheel steps add up. A press, a
//!   key down, a character and a composition are never merged or reordered, and
//!   a release is kept (also past the bound) so that the page never sees a press
//!   without its release; when a press had to be dropped its release goes too.
//! * While an IME composition is open, keys are withheld from the page and the
//!   Enter that commits it is not sent.
use std::collections::VecDeque;

use egui::{Event, ImeEvent, Key, Modifiers, MouseWheelUnit, PointerButton, Pos2, Rect, Vec2};
use serde_json::{Value, json};

/// Queued inputs per tab. Past this, new moves, presses and characters are dropped.
pub(super) const MAX_QUEUED: usize = 128;
/// Room kept for releases beyond [`MAX_QUEUED`].
const RELEASE_RESERVE: usize = 32;
/// Keys that may be down at once; with the buttons they fit in the reserve.
const MAX_HELD_KEYS: usize = 24;
const MAX_WHEEL_DELTA: f64 = 10_000.0;
const LINE_HEIGHT: f32 = 40.0;
const DOUBLE_CLICK_SECONDS: f64 = 0.5;
const DOUBLE_CLICK_DISTANCE: f32 = 4.0;
/// Longest text one paste or commit sends (the wire bounds a whole request).
pub(super) const MAX_TEXT_CHARS: usize = 8192;

pub(super) const METHOD_MOUSE: &str = "Input.dispatchMouseEvent";
pub(super) const METHOD_KEY: &str = "Input.dispatchKeyEvent";
pub(super) const METHOD_IME: &str = "Input.imeSetComposition";
pub(super) const METHOD_INSERT: &str = "Input.insertText";

const ALT: u32 = 1;
const CTRL: u32 = 2;
const META: u32 = 4;
const SHIFT: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    fn from_egui(button: PointerButton) -> Option<Self> {
        match button {
            PointerButton::Primary => Some(Self::Left),
            PointerButton::Secondary => Some(Self::Right),
            PointerButton::Middle => Some(Self::Middle),
            PointerButton::Extra1 | PointerButton::Extra2 => None,
        }
    }
    /// CDP `buttons` bit.
    const fn bit(self) -> u32 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 4,
        }
    }
    const fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Middle => "middle",
            Self::Right => "right",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KeyInfo {
    pub key: String,
    pub code: &'static str,
    pub vk: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Input {
    Move {
        x: f64,
        y: f64,
        buttons: u32,
    },
    Press {
        x: f64,
        y: f64,
        button: Button,
        buttons: u32,
        modifiers: u32,
        count: u32,
    },
    Release {
        x: f64,
        y: f64,
        button: Button,
        buttons: u32,
        modifiers: u32,
        count: u32,
    },
    Wheel {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        modifiers: u32,
    },
    KeyDown {
        key: KeyInfo,
        modifiers: u32,
    },
    KeyUp {
        key: KeyInfo,
        modifiers: u32,
    },
    Char {
        text: String,
        modifiers: u32,
    },
    ImeSet {
        text: String,
        start: u32,
        end: u32,
    },
    Insert {
        text: String,
    },
}

impl Input {
    /// The DevTools method and its parameters.
    pub fn cdp(&self) -> (&'static str, Value) {
        match self {
            Self::Move { x, y, buttons } => (
                METHOD_MOUSE,
                json!({
                    "type": "mouseMoved", "x": x, "y": y,
                    "button": first_button(*buttons), "buttons": buttons,
                    "pointerType": "mouse",
                }),
            ),
            Self::Press {
                x,
                y,
                button,
                buttons,
                modifiers,
                count,
            } => (
                METHOD_MOUSE,
                json!({
                    "type": "mousePressed", "x": x, "y": y, "button": button.name(),
                    "buttons": buttons, "clickCount": count, "modifiers": modifiers,
                    "pointerType": "mouse",
                }),
            ),
            Self::Release {
                x,
                y,
                button,
                buttons,
                modifiers,
                count,
            } => (
                METHOD_MOUSE,
                json!({
                    "type": "mouseReleased", "x": x, "y": y, "button": button.name(),
                    "buttons": buttons, "clickCount": count, "modifiers": modifiers,
                    "pointerType": "mouse",
                }),
            ),
            Self::Wheel {
                x,
                y,
                dx,
                dy,
                modifiers,
            } => (
                METHOD_MOUSE,
                json!({
                    "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy,
                    "modifiers": modifiers,
                }),
            ),
            Self::KeyDown { key, modifiers } => {
                (METHOD_KEY, key_params("rawKeyDown", key, *modifiers))
            }
            Self::KeyUp { key, modifiers } => (METHOD_KEY, key_params("keyUp", key, *modifiers)),
            Self::Char { text, modifiers } => (
                METHOD_KEY,
                json!({
                    "type": "char", "text": text, "unmodifiedText": text,
                    "modifiers": modifiers,
                }),
            ),
            Self::ImeSet { text, start, end } => (
                METHOD_IME,
                json!({ "text": text, "selectionStart": start, "selectionEnd": end }),
            ),
            Self::Insert { text } => (METHOD_INSERT, json!({ "text": text })),
        }
    }

    fn is_pointer_motion(&self) -> bool {
        matches!(self, Self::Move { .. } | Self::Wheel { .. })
    }
}

fn first_button(buttons: u32) -> &'static str {
    [Button::Left, Button::Right, Button::Middle]
        .into_iter()
        .find(|b| buttons & b.bit() != 0)
        .map_or("none", Button::name)
}

fn key_params(kind: &str, key: &KeyInfo, modifiers: u32) -> Value {
    json!({
        "type": kind, "key": key.key, "code": key.code,
        "windowsVirtualKeyCode": key.vk, "modifiers": modifiers,
    })
}

/// Inputs waiting to be sent, in the order the user made them.
#[derive(Default)]
pub(super) struct InputQueue {
    items: VecDeque<Input>,
    dropped: u64,
    /// Buttons whose press was dropped: the release is dropped with it.
    dropped_buttons: u32,
    /// Keys whose first press was dropped (each once): their release is dropped.
    dropped_keys: Vec<KeyInfo>,
    /// Buttons and keys whose press was queued and whose release was not: the
    /// releases the reserve is kept for.
    down_buttons: u32,
    down_keys: Vec<KeyInfo>,
}

impl InputQueue {
    /// Entries of the held and dropped press bookkeeping.
    #[cfg(test)]
    pub fn bookkeeping(&self) -> usize {
        self.down_keys.len() + self.dropped_keys.len()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }
    /// How many inputs were dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
    pub fn pop(&mut self) -> Option<Input> {
        self.items.pop_front()
    }
    pub fn clear(&mut self) {
        self.items.clear();
        self.dropped_buttons = 0;
        self.dropped_keys.clear();
        self.down_buttons = 0;
        self.down_keys.clear();
    }

    /// Room for one more input inside the normal capacity: the reserve is for
    /// releases only. Pointer motion is given up for it, as many as it takes.
    fn make_room(&mut self) -> bool {
        while self.items.len() >= MAX_QUEUED {
            let Some(at) = self.items.iter().position(Input::is_pointer_motion) else {
                return false;
            };
            self.items.remove(at);
            self.dropped += 1;
        }
        true
    }

    pub fn push(&mut self, input: Input) {
        match input {
            Input::Move { .. } => {
                if matches!(self.items.back(), Some(Input::Move { .. })) {
                    *self.items.back_mut().unwrap() = input;
                } else if self.make_room() {
                    self.items.push_back(input);
                } else {
                    self.dropped += 1;
                }
            }
            Input::Wheel {
                x,
                y,
                dx,
                dy,
                modifiers,
            } => {
                if let Some(Input::Wheel {
                    x: bx,
                    y: by,
                    dx: bdx,
                    dy: bdy,
                    modifiers: bm,
                }) = self.items.back_mut()
                    && *bm == modifiers
                {
                    *bx = x;
                    *by = y;
                    *bdx = (*bdx + dx).clamp(-MAX_WHEEL_DELTA, MAX_WHEEL_DELTA);
                    *bdy = (*bdy + dy).clamp(-MAX_WHEEL_DELTA, MAX_WHEEL_DELTA);
                } else if self.make_room() {
                    self.items.push_back(input);
                } else {
                    self.dropped += 1;
                }
            }
            Input::Release { button, .. } => {
                if self.dropped_buttons & button.bit() != 0 {
                    self.dropped_buttons &= !button.bit();
                    self.dropped += 1;
                } else if self.items.len() < MAX_QUEUED + RELEASE_RESERVE {
                    self.down_buttons &= !button.bit();
                    self.items.push_back(input);
                } else {
                    self.dropped += 1;
                }
            }
            Input::KeyUp { ref key, .. } => {
                if let Some(at) = self.dropped_keys.iter().position(|k| k == key) {
                    self.dropped_keys.remove(at);
                    self.dropped += 1;
                } else if self.items.len() < MAX_QUEUED + RELEASE_RESERVE {
                    if let Some(at) = self.down_keys.iter().position(|k| k == key) {
                        self.down_keys.remove(at);
                    }
                    self.items.push_back(input);
                } else {
                    self.dropped += 1;
                }
            }
            Input::Press { button, .. } => {
                if self.make_room() {
                    self.dropped_buttons &= !button.bit();
                    self.down_buttons |= button.bit();
                    self.items.push_back(input);
                } else {
                    self.dropped += 1;
                    // A press that is down already was sent: its release must be too.
                    if self.down_buttons & button.bit() == 0 {
                        self.dropped_buttons |= button.bit();
                    }
                }
            }
            Input::KeyDown { ref key, .. } => {
                let repeat = self.down_keys.contains(key);
                let held = repeat || self.down_keys.len() < MAX_HELD_KEYS;
                if held && self.make_room() {
                    if !repeat {
                        self.down_keys.push(key.clone());
                        self.dropped_keys.retain(|k| k != key);
                    }
                    self.items.push_back(input);
                } else {
                    self.dropped += 1;
                    if !repeat && !self.dropped_keys.contains(key) {
                        self.dropped_keys.push(key.clone());
                    }
                }
            }
            other => {
                if self.make_room() {
                    self.items.push_back(other);
                } else {
                    self.dropped += 1;
                }
            }
        }
    }
}

/// What the translator needs of the widget for one frame.
pub(super) struct Surface {
    pub rect: Rect,
    /// The size, in CSS pixels, of the page the rectangle shows.
    pub css: Vec2,
    pub hovered: bool,
    pub focused: bool,
    pub time: f64,
    pub hover_pos: Option<Pos2>,
}

impl Surface {
    fn ratio(&self) -> (f64, f64) {
        let w = f64::from(self.rect.width().max(1.0));
        let h = f64::from(self.rect.height().max(1.0));
        (f64::from(self.css.x) / w, f64::from(self.css.y) / h)
    }

    pub fn to_css(&self, pos: Pos2) -> (f64, f64) {
        let (rx, ry) = self.ratio();
        let x = (f64::from(pos.x - self.rect.min.x) * rx).clamp(0.0, f64::from(self.css.x));
        let y = (f64::from(pos.y - self.rect.min.y) * ry).clamp(0.0, f64::from(self.css.y));
        (round2(x), round2(y))
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn modifier_bits(m: Modifiers) -> u32 {
    [
        (m.alt, ALT),
        (m.ctrl, CTRL),
        (m.mac_cmd, META),
        (m.shift, SHIFT),
    ]
    .into_iter()
    .filter(|(down, _)| *down)
    .fold(0, |bits, (_, bit)| bits | bit)
}

fn utf16(text: &str, chars: usize) -> u32 {
    text.chars().take(chars).map(char::len_utf16).sum::<usize>() as u32
}

fn limited(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

/// The page-facing identity of an egui key, or `None` for a key the page is
/// not sent.
pub(super) fn key_info(key: Key, shift: bool) -> Option<KeyInfo> {
    let named = |key: &str, code: &'static str, vk: i32| {
        Some(KeyInfo {
            key: key.to_owned(),
            code,
            vk,
        })
    };
    let name = key.name();
    if name.len() == 1 {
        let c = name.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            const CODES: [&str; 26] = [
                "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ",
                "KeyK", "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT",
                "KeyU", "KeyV", "KeyW", "KeyX", "KeyY", "KeyZ",
            ];
            let index = (c.to_ascii_uppercase() as u8 - b'A') as usize;
            let letter = if shift {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            return Some(KeyInfo {
                key: letter.to_string(),
                code: CODES[index],
                vk: 0x41 + index as i32,
            });
        }
        if c.is_ascii_digit() {
            const CODES: [&str; 10] = [
                "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7",
                "Digit8", "Digit9",
            ];
            let n = (c as u8 - b'0') as usize;
            return Some(KeyInfo {
                key: c.to_string(),
                code: CODES[n],
                vk: 0x30 + n as i32,
            });
        }
    }
    match key {
        Key::Enter => named("Enter", "Enter", 0x0D),
        Key::Tab => named("Tab", "Tab", 0x09),
        Key::Backspace => named("Backspace", "Backspace", 0x08),
        Key::Escape => named("Escape", "Escape", 0x1B),
        Key::Space => named(" ", "Space", 0x20),
        Key::Delete => named("Delete", "Delete", 0x2E),
        Key::Insert => named("Insert", "Insert", 0x2D),
        Key::Home => named("Home", "Home", 0x24),
        Key::End => named("End", "End", 0x23),
        Key::PageUp => named("PageUp", "PageUp", 0x21),
        Key::PageDown => named("PageDown", "PageDown", 0x22),
        Key::ArrowLeft => named("ArrowLeft", "ArrowLeft", 0x25),
        Key::ArrowUp => named("ArrowUp", "ArrowUp", 0x26),
        Key::ArrowRight => named("ArrowRight", "ArrowRight", 0x27),
        Key::ArrowDown => named("ArrowDown", "ArrowDown", 0x28),
        Key::F1 => named("F1", "F1", 0x70),
        Key::F2 => named("F2", "F2", 0x71),
        Key::F3 => named("F3", "F3", 0x72),
        Key::F4 => named("F4", "F4", 0x73),
        Key::F5 => named("F5", "F5", 0x74),
        Key::F6 => named("F6", "F6", 0x75),
        Key::F7 => named("F7", "F7", 0x76),
        Key::F8 => named("F8", "F8", 0x77),
        Key::F9 => named("F9", "F9", 0x78),
        Key::F10 => named("F10", "F10", 0x79),
        Key::F11 => named("F11", "F11", 0x7A),
        Key::F12 => named("F12", "F12", 0x7B),
        Key::Minus => named("-", "Minus", 0xBD),
        Key::Equals | Key::Plus => named("=", "Equal", 0xBB),
        Key::OpenBracket => named("[", "BracketLeft", 0xDB),
        Key::CloseBracket => named("]", "BracketRight", 0xDD),
        Key::Backslash => named("\\", "Backslash", 0xDC),
        Key::Semicolon => named(";", "Semicolon", 0xBA),
        Key::Quote => named("'", "Quote", 0xDE),
        Key::Backtick => named("`", "Backquote", 0xC0),
        Key::Comma => named(",", "Comma", 0xBC),
        Key::Period => named(".", "Period", 0xBE),
        Key::Slash => named("/", "Slash", 0xBF),
        _ => None,
    }
}

/// The text a control key adds to its key event.
fn control_text(key: Key) -> Option<&'static str> {
    match key {
        Key::Enter => Some("\r"),
        Key::Tab => Some("\t"),
        _ => None,
    }
}

/// Turns the events of a frame into queued inputs, keeping what the events do
/// not carry: which buttons are down, the click count, which keys are held and
/// the state of an IME composition.
#[derive(Default)]
pub(super) struct Translator {
    buttons: u32,
    last_press: Option<(f64, Pos2, u32)>,
    last_pos: (f64, f64),
    held: Vec<(Key, KeyInfo)>,
    composing: bool,
    enter_forwarded: bool,
}

impl Translator {
    #[cfg(test)]
    pub fn composing(&self) -> bool {
        self.composing
    }

    /// The focus left the widget: what is still down is released, in the order
    /// it went down for keys and with the last known position for buttons.
    pub fn release_all(&mut self, out: &mut InputQueue) {
        for (button, bit) in [
            (Button::Left, 1u32),
            (Button::Right, 2),
            (Button::Middle, 4),
        ] {
            if self.buttons & bit != 0 {
                self.buttons &= !bit;
                out.push(Input::Release {
                    x: self.last_pos.0,
                    y: self.last_pos.1,
                    button,
                    buttons: self.buttons,
                    modifiers: 0,
                    count: 1,
                });
            }
        }
        for (_, key) in std::mem::take(&mut self.held) {
            out.push(Input::KeyUp { key, modifiers: 0 });
        }
        if self.composing {
            out.push(Input::ImeSet {
                text: String::new(),
                start: 0,
                end: 0,
            });
            self.composing = false;
        }
        self.enter_forwarded = false;
    }

    pub fn translate(&mut self, events: &[Event], surface: &Surface, out: &mut InputQueue) {
        let mut committed = false;
        for event in events {
            match event {
                Event::PointerMoved(pos) if surface.rect.contains(*pos) || self.buttons != 0 => {
                    let (x, y) = surface.to_css(*pos);
                    self.last_pos = (x, y);
                    out.push(Input::Move {
                        x,
                        y,
                        buttons: self.buttons,
                    });
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let Some(button) = Button::from_egui(*button) else {
                        continue;
                    };
                    let bit = button.bit();
                    let count = if *pressed {
                        if !surface.rect.contains(*pos) {
                            continue;
                        }
                        let count = match self.last_press {
                            Some((at, last, count))
                                if surface.time - at < DOUBLE_CLICK_SECONDS
                                    && last.distance(*pos) < DOUBLE_CLICK_DISTANCE =>
                            {
                                count + 1
                            }
                            _ => 1,
                        };
                        self.last_press = Some((surface.time, *pos, count));
                        self.buttons |= bit;
                        count
                    } else {
                        if self.buttons & bit == 0 {
                            continue;
                        }
                        self.buttons &= !bit;
                        self.last_press.map_or(1, |(_, _, count)| count)
                    };
                    let (x, y) = surface.to_css(*pos);
                    self.last_pos = (x, y);
                    let (modifiers, buttons) = (modifier_bits(*modifiers), self.buttons);
                    out.push(if *pressed {
                        Input::Press {
                            x,
                            y,
                            button,
                            buttons,
                            modifiers,
                            count,
                        }
                    } else {
                        Input::Release {
                            x,
                            y,
                            button,
                            buttons,
                            modifiers,
                            count,
                        }
                    });
                }
                Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } if surface.hovered => {
                    let scale = match unit {
                        MouseWheelUnit::Point => 1.0,
                        MouseWheelUnit::Line => LINE_HEIGHT,
                        MouseWheelUnit::Page => surface.rect.height(),
                    };
                    let (rx, ry) = surface.ratio();
                    let (x, y) = surface.to_css(surface.hover_pos.unwrap_or(surface.rect.center()));
                    // A page scrolls down for a positive delta; egui's is positive upwards.
                    out.push(Input::Wheel {
                        x,
                        y,
                        dx: round2(-f64::from(delta.x * scale) * rx),
                        dy: round2(-f64::from(delta.y * scale) * ry),
                        modifiers: modifier_bits(*modifiers),
                    });
                }
                Event::Key {
                    key,
                    pressed,
                    modifiers,
                    repeat,
                    ..
                } if surface.focused && !self.composing => {
                    if *key == Key::Enter {
                        if (*pressed && committed) || (!*pressed && !self.enter_forwarded) {
                            continue;
                        }
                        self.enter_forwarded = *pressed;
                    }
                    let Some(info) = key_info(*key, modifiers.shift) else {
                        continue;
                    };
                    let bits = modifier_bits(*modifiers);
                    if *pressed {
                        if !*repeat && !self.held.iter().any(|(k, _)| k == key) {
                            self.held.push((*key, info.clone()));
                        }
                        out.push(Input::KeyDown {
                            key: info,
                            modifiers: bits,
                        });
                        if bits & (CTRL | ALT | META) == 0
                            && let Some(text) = control_text(*key)
                        {
                            out.push(Input::Char {
                                text: text.to_owned(),
                                modifiers: bits,
                            });
                        }
                    } else {
                        let info = self
                            .held
                            .iter()
                            .position(|(k, _)| k == key)
                            .map(|at| self.held.remove(at).1)
                            .unwrap_or(info);
                        out.push(Input::KeyUp {
                            key: info,
                            modifiers: bits,
                        });
                    }
                }
                Event::Copy | Event::Cut if surface.focused && !self.composing => {
                    let key = if *event == Event::Copy {
                        Key::C
                    } else {
                        Key::X
                    };
                    if let Some(info) = key_info(key, false) {
                        out.push(Input::KeyDown {
                            key: info.clone(),
                            modifiers: CTRL,
                        });
                        out.push(Input::KeyUp {
                            key: info,
                            modifiers: CTRL,
                        });
                    }
                }
                Event::Paste(text) if surface.focused && !self.composing && !text.is_empty() => {
                    out.push(Input::Insert {
                        text: limited(text),
                    });
                }
                Event::Text(text) if surface.focused && !self.composing && !text.is_empty() => {
                    out.push(Input::Char {
                        text: limited(text),
                        modifiers: 0,
                    });
                }
                Event::Ime(ImeEvent::Preedit {
                    text,
                    active_range_chars,
                }) if surface.focused => {
                    if text.is_empty() {
                        if self.composing {
                            out.push(Input::ImeSet {
                                text: String::new(),
                                start: 0,
                                end: 0,
                            });
                        }
                        self.composing = false;
                        continue;
                    }
                    let total = text.encode_utf16().count() as u32;
                    let (start, end) = active_range_chars.as_ref().map_or((total, total), |r| {
                        (utf16(text, r.start), utf16(text, r.end))
                    });
                    out.push(Input::ImeSet {
                        text: limited(text),
                        start,
                        end,
                    });
                    self.composing = true;
                }
                Event::Ime(ImeEvent::Commit(text)) if surface.focused => {
                    if !text.is_empty() {
                        out.push(Input::Insert {
                            text: limited(text),
                        });
                    }
                    self.composing = false;
                    committed = true;
                }
                _ => {}
            }
        }
    }
}
