use egui::{Event, ImeEvent, Key, Modifiers, MouseWheelUnit, PointerButton, Pos2, Rect};

/// `cef_event_flags_t` bits.
pub const SHIFT: u32 = 1 << 1;
pub const CONTROL: u32 = 1 << 2;
pub const ALT: u32 = 1 << 3;
pub const LEFT_MOUSE: u32 = 1 << 4;
pub const MIDDLE_MOUSE: u32 = 1 << 5;
pub const RIGHT_MOUSE: u32 = 1 << 6;
pub const COMMAND: u32 = 1 << 7;

const DOUBLE_CLICK_SECONDS: f64 = 0.5;
const DOUBLE_CLICK_DISTANCE: f32 = 4.0;
const LINE_HEIGHT: f32 = 40.0;

pub fn modifier_flags(modifiers: Modifiers) -> u32 {
    [
        (modifiers.shift, SHIFT),
        (modifiers.ctrl, CONTROL),
        (modifiers.alt, ALT),
        (modifiers.mac_cmd, COMMAND),
    ]
    .into_iter()
    .filter(|(down, _)| *down)
    .fold(0, |flags, (_, flag)| flags | flag)
}

/// Windows virtual-key code and Linux evdev scan code (US layout). CEF on
/// Linux reads `native_key_code` as the XKB keycode, which is evdev + 8.
pub fn key_codes(key: Key) -> Option<(i32, i32)> {
    const LETTERS: [i32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    const LETTER_KEYS: [Key; 26] = [
        Key::A,
        Key::B,
        Key::C,
        Key::D,
        Key::E,
        Key::F,
        Key::G,
        Key::H,
        Key::I,
        Key::J,
        Key::K,
        Key::L,
        Key::M,
        Key::N,
        Key::O,
        Key::P,
        Key::Q,
        Key::R,
        Key::S,
        Key::T,
        Key::U,
        Key::V,
        Key::W,
        Key::X,
        Key::Y,
        Key::Z,
    ];
    const DIGIT_KEYS: [Key; 10] = [
        Key::Num0,
        Key::Num1,
        Key::Num2,
        Key::Num3,
        Key::Num4,
        Key::Num5,
        Key::Num6,
        Key::Num7,
        Key::Num8,
        Key::Num9,
    ];
    if let Some(index) = LETTER_KEYS.iter().position(|letter| *letter == key) {
        return Some((0x41 + index as i32, LETTERS[index]));
    }
    if let Some(n) = DIGIT_KEYS.iter().position(|digit| *digit == key) {
        let n = n as i32;
        return Some((0x30 + n, if n == 0 { 11 } else { 1 + n }));
    }
    Some(match key {
        Key::Enter => (0x0D, 28),
        Key::Escape => (0x1B, 1),
        Key::Backspace => (0x08, 14),
        Key::Tab => (0x09, 15),
        Key::Space => (0x20, 57),
        Key::ArrowLeft => (0x25, 105),
        Key::ArrowUp => (0x26, 103),
        Key::ArrowRight => (0x27, 106),
        Key::ArrowDown => (0x28, 108),
        Key::Home => (0x24, 102),
        Key::End => (0x23, 107),
        Key::PageUp => (0x21, 104),
        Key::PageDown => (0x22, 109),
        Key::Insert => (0x2D, 110),
        Key::Delete => (0x2E, 111),
        Key::F1 => (0x70, 59),
        Key::F2 => (0x71, 60),
        Key::F3 => (0x72, 61),
        Key::F4 => (0x73, 62),
        Key::F5 => (0x74, 63),
        Key::F6 => (0x75, 64),
        Key::F7 => (0x76, 65),
        Key::F8 => (0x77, 66),
        Key::F9 => (0x78, 67),
        Key::F10 => (0x79, 68),
        Key::F11 => (0x7A, 87),
        Key::F12 => (0x7B, 88),
        Key::Minus => (0xBD, 12),
        Key::Equals | Key::Plus => (0xBB, 13),
        Key::OpenBracket | Key::OpenCurlyBracket => (0xDB, 26),
        Key::CloseBracket | Key::CloseCurlyBracket => (0xDD, 27),
        Key::Backslash | Key::Pipe => (0xDC, 43),
        Key::Semicolon | Key::Colon => (0xBA, 39),
        Key::Quote => (0xDE, 40),
        Key::Backtick => (0xC0, 41),
        Key::Comma => (0xBC, 51),
        Key::Period => (0xBE, 52),
        Key::Slash | Key::Questionmark => (0xBF, 53),
        _ => return None,
    })
}

/// Control characters a key produces; printable text arrives as `Event::Text`.
fn control_character(key: Key) -> Option<u16> {
    match key {
        Key::Enter => Some(0x0D),
        Key::Tab => Some(0x09),
        Key::Backspace => Some(0x08),
        Key::Escape => Some(0x1B),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    fn from_egui(button: PointerButton) -> Option<Self> {
        match button {
            PointerButton::Primary => Some(Self::Left),
            PointerButton::Secondary => Some(Self::Right),
            PointerButton::Middle => Some(Self::Middle),
            PointerButton::Extra1 | PointerButton::Extra2 => None,
        }
    }

    pub const fn flag(self) -> u32 {
        match self {
            Self::Left => LEFT_MOUSE,
            Self::Middle => MIDDLE_MOUSE,
            Self::Right => RIGHT_MOUSE,
        }
    }
}

/// One call to make on the CEF browser host, in view (DIP) coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostInput {
    MouseMove {
        x: i32,
        y: i32,
        modifiers: u32,
        leave: bool,
    },
    MouseClick {
        x: i32,
        y: i32,
        modifiers: u32,
        button: MouseButton,
        up: bool,
        count: i32,
    },
    MouseWheel {
        x: i32,
        y: i32,
        modifiers: u32,
        dx: i32,
        dy: i32,
    },
    Key {
        key: Key,
        down: bool,
        modifiers: u32,
    },
    Char {
        unit: u16,
        modifiers: u32,
    },
    ImeComposition {
        text: String,
        caret_utf16: u32,
    },
    ImeCancel,
    ImeCommit(String),
}

pub struct FrameInput {
    pub rect: Rect,
    pub hovered: bool,
    pub focused: bool,
    pub time: f64,
    pub hover_pos: Option<Pos2>,
}

/// Turns egui events into CEF host calls, keeping the button, click-count and
/// IME composition state that egui events alone do not carry.
#[derive(Default)]
pub struct InputTranslator {
    buttons: u32,
    last_press: Option<(f64, Pos2, i32)>,
    composing: bool,
    enter_forwarded: bool,
}

impl InputTranslator {
    pub fn translate(&mut self, events: &[Event], frame: &FrameInput) -> Vec<HostInput> {
        let rect = frame.rect;
        let to_view = |pos: Pos2| {
            (
                (pos.x - rect.min.x).round() as i32,
                (pos.y - rect.min.y).round() as i32,
            )
        };
        let mut out = Vec::new();
        let mut committed = false;
        for event in events {
            match event {
                Event::PointerMoved(pos) if rect.contains(*pos) || self.buttons != 0 => {
                    let (x, y) = to_view(*pos);
                    out.push(HostInput::MouseMove {
                        x,
                        y,
                        modifiers: self.buttons,
                        leave: false,
                    });
                }
                Event::PointerGone if frame.hovered || self.buttons != 0 => {
                    out.push(HostInput::MouseMove {
                        x: -1,
                        y: -1,
                        modifiers: 0,
                        leave: true,
                    });
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let Some(button) = MouseButton::from_egui(*button) else {
                        continue;
                    };
                    let flag = button.flag();
                    let count = if *pressed {
                        if !rect.contains(*pos) {
                            continue;
                        }
                        let count = match self.last_press {
                            Some((at, last, count))
                                if frame.time - at < DOUBLE_CLICK_SECONDS
                                    && last.distance(*pos) < DOUBLE_CLICK_DISTANCE =>
                            {
                                count + 1
                            }
                            _ => 1,
                        };
                        self.last_press = Some((frame.time, *pos, count));
                        self.buttons |= flag;
                        count
                    } else {
                        if self.buttons & flag == 0 {
                            continue;
                        }
                        self.buttons &= !flag;
                        self.last_press.map_or(1, |(_, _, count)| count)
                    };
                    let (x, y) = to_view(*pos);
                    out.push(HostInput::MouseClick {
                        x,
                        y,
                        modifiers: modifier_flags(*modifiers) | if *pressed { flag } else { 0 },
                        button,
                        up: !pressed,
                        count,
                    });
                }
                Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } if frame.hovered => {
                    let scale = match unit {
                        MouseWheelUnit::Point => 1.0,
                        MouseWheelUnit::Line => LINE_HEIGHT,
                        MouseWheelUnit::Page => rect.height(),
                    };
                    let (x, y) = to_view(frame.hover_pos.unwrap_or(rect.center()));
                    out.push(HostInput::MouseWheel {
                        x,
                        y,
                        modifiers: modifier_flags(*modifiers),
                        dx: (delta.x * scale) as i32,
                        dy: (delta.y * scale) as i32,
                    });
                }
                Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } if frame.focused && !self.composing => {
                    if *key == Key::Enter {
                        if (*pressed && committed) || (!*pressed && !self.enter_forwarded) {
                            continue;
                        }
                        self.enter_forwarded = *pressed;
                    }
                    if key_codes(*key).is_none() {
                        continue;
                    }
                    let modifiers = modifier_flags(*modifiers);
                    out.push(HostInput::Key {
                        key: *key,
                        down: *pressed,
                        modifiers,
                    });
                    if *pressed
                        && modifiers & (CONTROL | ALT) == 0
                        && let Some(unit) = control_character(*key)
                    {
                        out.push(HostInput::Char { unit, modifiers });
                    }
                }
                // egui turns Ctrl+C and Ctrl+X into these; the page needs the keys.
                Event::Copy | Event::Cut if frame.focused && !self.composing => {
                    let key = if *event == Event::Copy {
                        Key::C
                    } else {
                        Key::X
                    };
                    for down in [true, false] {
                        out.push(HostInput::Key {
                            key,
                            down,
                            modifiers: CONTROL,
                        });
                    }
                }
                Event::Text(text) if frame.focused && !self.composing => {
                    out.extend(
                        text.encode_utf16()
                            .map(|unit| HostInput::Char { unit, modifiers: 0 }),
                    );
                }
                Event::Ime(ImeEvent::Preedit {
                    text,
                    active_range_chars,
                }) if frame.focused => {
                    if text.is_empty() {
                        if self.composing {
                            out.push(HostInput::ImeCancel);
                        }
                        self.composing = false;
                        continue;
                    }
                    let caret = active_range_chars.as_ref().map_or_else(
                        || text.encode_utf16().count(),
                        |range| text.chars().take(range.end).map(char::len_utf16).sum(),
                    );
                    out.push(HostInput::ImeComposition {
                        text: text.clone(),
                        caret_utf16: caret as u32,
                    });
                    self.composing = true;
                }
                Event::Ime(ImeEvent::Commit(text)) if frame.focused => {
                    out.push(HostInput::ImeCommit(text.clone()));
                    self.composing = false;
                    committed = true;
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use egui::{Vec2, vec2};

    use super::*;

    fn frame(time: f64) -> FrameInput {
        FrameInput {
            rect: Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::splat(100.0)),
            hovered: true,
            focused: true,
            time,
            hover_pos: Some(Pos2::new(15.0, 25.0)),
        }
    }

    fn press(x: f32, y: f32, pressed: bool) -> Event {
        Event::PointerButton {
            pos: Pos2::new(x, y),
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::default(),
        }
    }

    fn key(key: Key, pressed: bool) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn copy_and_cut_reach_a_focused_page_as_their_shortcuts() {
        let mut input = InputTranslator::default();
        let shortcut = |key| {
            [true, false].map(|down| HostInput::Key {
                key,
                down,
                modifiers: CONTROL,
            })
        };

        assert_eq!(
            input.translate(&[Event::Copy, Event::Cut], &frame(1.0)),
            [shortcut(Key::C), shortcut(Key::X)].concat()
        );
        let unfocused = FrameInput {
            focused: false,
            ..frame(1.0)
        };
        assert!(input.translate(&[Event::Copy], &unfocused).is_empty());
    }

    #[test]
    fn clicks_map_to_view_coordinates_and_count_double_clicks_by_input_time() {
        let mut input = InputTranslator::default();

        let first = input.translate(
            &[press(15.0, 25.0, true), press(15.0, 25.0, false)],
            &frame(1.0),
        );
        let second = input.translate(&[press(16.0, 25.0, true)], &frame(1.2));
        let late = input.translate(
            &[press(16.0, 25.0, false), press(16.0, 25.0, true)],
            &frame(2.0),
        );

        assert_eq!(
            first[0],
            HostInput::MouseClick {
                x: 5,
                y: 5,
                modifiers: LEFT_MOUSE,
                button: MouseButton::Left,
                up: false,
                count: 1
            }
        );
        assert!(matches!(
            first[1],
            HostInput::MouseClick {
                up: true,
                modifiers: 0,
                ..
            }
        ));
        assert!(matches!(second[0], HostInput::MouseClick { count: 2, .. }));
        assert!(matches!(
            late[1],
            HostInput::MouseClick {
                count: 1,
                up: false,
                ..
            }
        ));
    }

    #[test]
    fn presses_outside_the_view_are_ignored_but_a_drag_keeps_its_release() {
        let mut input = InputTranslator::default();

        let outside = input.translate(
            &[press(500.0, 500.0, true), press(500.0, 500.0, false)],
            &frame(0.0),
        );
        let drag = input.translate(
            &[
                press(15.0, 25.0, true),
                Event::PointerMoved(Pos2::new(500.0, 500.0)),
                press(500.0, 500.0, false),
            ],
            &frame(1.0),
        );

        assert!(outside.is_empty());
        assert_eq!(drag.len(), 3);
        assert!(matches!(
            drag[1],
            HostInput::MouseMove {
                modifiers: LEFT_MOUSE,
                ..
            }
        ));
        assert!(matches!(drag[2], HostInput::MouseClick { up: true, .. }));
    }

    #[test]
    fn line_wheel_scrolls_by_line_height_at_the_hover_position() {
        let mut input = InputTranslator::default();
        let wheel = Event::MouseWheel {
            unit: MouseWheelUnit::Line,
            delta: vec2(0.0, -2.0),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::default(),
        };

        assert_eq!(
            input.translate(&[wheel], &frame(0.0)),
            [HostInput::MouseWheel {
                x: 5,
                y: 5,
                modifiers: 0,
                dx: 0,
                dy: -80
            }]
        );
    }

    #[test]
    fn enter_sends_a_control_character_but_text_carries_printable_input() {
        let mut input = InputTranslator::default();

        let out = input.translate(
            &[
                key(Key::A, true),
                Event::Text("aあ".into()),
                key(Key::Enter, true),
            ],
            &frame(0.0),
        );

        assert_eq!(
            out,
            [
                HostInput::Key {
                    key: Key::A,
                    down: true,
                    modifiers: 0
                },
                HostInput::Char {
                    unit: 'a' as u16,
                    modifiers: 0
                },
                HostInput::Char {
                    unit: 'あ' as u16,
                    modifiers: 0
                },
                HostInput::Key {
                    key: Key::Enter,
                    down: true,
                    modifiers: 0
                },
                HostInput::Char {
                    unit: 0x0D,
                    modifiers: 0
                },
            ]
        );
    }

    #[test]
    fn composition_withholds_keys_and_the_committing_enter_never_reaches_the_page() {
        let mut input = InputTranslator::default();
        let preedit = |text: &str| {
            Event::Ime(ImeEvent::Preedit {
                text: text.into(),
                active_range_chars: Some(0..1),
            })
        };

        let composing = input.translate(
            &[preedit("に"), key(Key::N, true), Event::Text("n".into())],
            &frame(0.0),
        );
        let commit = input.translate(
            &[
                Event::Ime(ImeEvent::Commit("日本".into())),
                key(Key::Enter, true),
            ],
            &frame(0.1),
        );
        let release = input.translate(&[key(Key::Enter, false)], &frame(0.2));
        let next = input.translate(&[key(Key::Enter, true)], &frame(0.3));

        assert_eq!(
            composing,
            [HostInput::ImeComposition {
                text: "に".into(),
                caret_utf16: 1
            }]
        );
        assert_eq!(commit, [HostInput::ImeCommit("日本".into())]);
        assert!(release.is_empty());
        assert!(matches!(
            next[0],
            HostInput::Key {
                key: Key::Enter,
                down: true,
                ..
            }
        ));
    }

    #[test]
    fn dismissed_composition_is_cancelled_once() {
        let mut input = InputTranslator::default();
        let preedit = |text: &str| {
            Event::Ime(ImeEvent::Preedit {
                text: text.into(),
                active_range_chars: None,
            })
        };

        let out = input.translate(&[preedit("𠮷"), preedit(""), preedit("")], &frame(0.0));

        assert_eq!(
            out,
            [
                HostInput::ImeComposition {
                    text: "𠮷".into(),
                    caret_utf16: 2
                },
                HostInput::ImeCancel,
            ]
        );
    }
}
