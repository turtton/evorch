//! 端末セルの色を egui の色へ解決します。
//!
//! 既定の前景・背景はテーマのパレットに合わせ、ANSI 16 色は暗色テーマ向けの固定値を使います。
//! アプリケーションが OSC 4 / 10 / 11 で上書きした色は `Colors` を優先します。

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use egui::Color32;

use crate::theme::tokens::palette;

const ANSI: [(u8, u8, u8); 16] = [
    (0x15, 0x16, 0x1e),
    (0xf7, 0x76, 0x8e),
    (0x9e, 0xce, 0x6a),
    (0xe0, 0xaf, 0x68),
    (0x7a, 0xa2, 0xf7),
    (0xbb, 0x9a, 0xf7),
    (0x7d, 0xcf, 0xff),
    (0xa9, 0xb1, 0xd6),
    (0x41, 0x48, 0x68),
    (0xff, 0x89, 0x9d),
    (0xb9, 0xf2, 0x7c),
    (0xff, 0x9e, 0x64),
    (0x8d, 0xb6, 0xff),
    (0xc7, 0xa9, 0xff),
    (0xa4, 0xda, 0xff),
    (0xc0, 0xca, 0xf5),
];

const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn rgb(color: Color32) -> Rgb {
    Rgb {
        r: color.r(),
        g: color.g(),
        b: color.b(),
    }
}

fn dim(color: Rgb) -> Rgb {
    let scale = |value: u8| (u16::from(value) * 2 / 3) as u8;
    Rgb {
        r: scale(color.r),
        g: scale(color.g),
        b: scale(color.b),
    }
}

/// 色番号 (0..256 の indexed、または `NamedColor` の値) の既定 RGB です。
fn default_rgb(index: usize) -> Rgb {
    let theme = palette();
    match index {
        0..16 => {
            let (r, g, b) = ANSI[index];
            Rgb { r, g, b }
        }
        16..232 => {
            let cube = index - 16;
            Rgb {
                r: CUBE_LEVELS[cube / 36],
                g: CUBE_LEVELS[cube / 6 % 6],
                b: CUBE_LEVELS[cube % 6],
            }
        }
        232..256 => {
            let level = 8 + 10 * (index - 232) as u8;
            Rgb {
                r: level,
                g: level,
                b: level,
            }
        }
        _ => match index {
            i if i == NamedColor::Foreground as usize => rgb(theme.TEXT),
            i if i == NamedColor::Background as usize => rgb(theme.INPUT),
            i if i == NamedColor::Cursor as usize => rgb(theme.TEXT),
            i if i == NamedColor::BrightForeground as usize => rgb(theme.TEXT),
            i if i == NamedColor::DimForeground as usize => dim(rgb(theme.TEXT)),
            i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
                dim(default_rgb(i - NamedColor::DimBlack as usize))
            }
            _ => rgb(theme.TEXT),
        },
    }
}

/// アプリケーションによる上書きを考慮した RGB を返します。
pub fn resolve_rgb(index: usize, overrides: &Colors) -> Rgb {
    overrides[index].unwrap_or_else(|| default_rgb(index))
}

fn to_color32(color: Rgb) -> Color32 {
    Color32::from_rgb(color.r, color.g, color.b)
}

/// 前景色を太字・減光の属性込みで解決します。
pub fn foreground(color: Color, flags: Flags, overrides: &Colors) -> Color32 {
    let index = match color {
        Color::Spec(spec) => {
            let spec = if flags.contains(Flags::DIM) {
                dim(spec)
            } else {
                spec
            };
            return to_color32(spec);
        }
        Color::Indexed(index) => {
            let index = usize::from(index);
            if index < 8 && flags.contains(Flags::BOLD) {
                index + 8
            } else {
                index
            }
        }
        Color::Named(named) => {
            let named = if flags.contains(Flags::DIM) {
                named.to_dim()
            } else if flags.contains(Flags::BOLD) {
                named.to_bright()
            } else {
                named
            };
            named as usize
        }
    };
    to_color32(resolve_rgb(index, overrides))
}

/// 背景色を解決します。
pub fn background(color: Color, overrides: &Colors) -> Color32 {
    match color {
        Color::Spec(spec) => to_color32(spec),
        Color::Indexed(index) => to_color32(resolve_rgb(usize::from(index), overrides)),
        Color::Named(named) => to_color32(resolve_rgb(named as usize, overrides)),
    }
}

/// 既定の背景色です。
pub fn default_background(overrides: &Colors) -> Color32 {
    background(Color::Named(NamedColor::Background), overrides)
}

/// カーソル色です。
pub fn cursor(overrides: &Colors) -> Color32 {
    to_color32(resolve_rgb(NamedColor::Cursor as usize, overrides))
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::term::color::Colors;
    use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
    use egui::Color32;

    use super::{background, foreground};

    #[test]
    fn indexed_colors_follow_the_xterm_256_layout() {
        // Given: no application overrides
        let colors = Colors::default();

        // When: cube and grayscale entries are resolved
        let cube = background(Color::Indexed(196), &colors);
        let gray = background(Color::Indexed(244), &colors);

        // Then: they match the xterm 256-color table
        assert_eq!(cube, Color32::from_rgb(255, 0, 0));
        assert_eq!(gray, Color32::from_rgb(128, 128, 128));
    }

    #[test]
    fn bold_brightens_and_overrides_win() {
        // Given: an application that redefined red through OSC 4
        let mut colors = Colors::default();
        colors[NamedColor::Red as usize] = Some(Rgb { r: 1, g: 2, b: 3 });

        // When: plain and bold red are resolved
        let red = foreground(Color::Named(NamedColor::Red), Flags::empty(), &colors);
        let bold = foreground(Color::Named(NamedColor::Red), Flags::BOLD, &colors);

        // Then: the override applies and bold maps to the bright variant
        assert_eq!(red, Color32::from_rgb(1, 2, 3));
        assert_eq!(
            bold,
            foreground(Color::Named(NamedColor::BrightRed), Flags::empty(), &colors)
        );
    }
}
