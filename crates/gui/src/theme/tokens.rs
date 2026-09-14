use egui::Color32;
use event_bus::AgentRunPhase;
use workspace_ui::{ThreadRunPhase, ThreadState};

#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(non_snake_case)]
pub struct Palette {
    pub CANVAS: Color32,
    pub SURFACE: Color32,
    pub SURFACE_RAISED: Color32,
    pub OVERLAY: Color32,
    pub SIDEBAR: Color32,
    pub TEXT: Color32,
    pub TEXT_MUTED: Color32,
    pub BORDER: Color32,
    pub INPUT: Color32,
    pub ACCENT: Color32,
    pub ACCENT_FG: Color32,
    pub HOVER_ROW: Color32,
    pub ACTIVE_ROW: Color32,
    pub SELECTED_ROW: Color32,
    pub ERROR: Color32,
    pub ERROR_FG: Color32,
    pub ERROR_SURFACE: Color32,
    pub WARNING: Color32,
    pub WARNING_FG: Color32,
    pub WARNING_SURFACE: Color32,
    pub SUCCESS: Color32,
    pub INFO: Color32,
    pub RUNNING: Color32,
    pub WAITING: Color32,
}

impl Palette {
    pub const fn graphite() -> Self {
        Self {
            CANVAS: Color32::from_rgb(0x0a, 0x0a, 0x0a),
            SURFACE: Color32::from_rgb(0x11, 0x11, 0x11),
            SURFACE_RAISED: Color32::from_rgb(0x14, 0x14, 0x14),
            OVERLAY: Color32::from_rgb(0x19, 0x19, 0x19),
            SIDEBAR: Color32::from_rgb(0x00, 0x00, 0x00),
            TEXT: Color32::from_rgb(0xf5, 0xf5, 0xf5),
            TEXT_MUTED: Color32::from_rgb(0x81, 0x81, 0x81),
            BORDER: Color32::from_rgb(0x19, 0x19, 0x19),
            INPUT: Color32::from_rgb(0x1e, 0x1e, 0x1e),
            ACCENT: Color32::from_rgb(0x34, 0x6b, 0xf1),
            ACCENT_FG: Color32::from_rgb(0xff, 0xff, 0xff),
            HOVER_ROW: Color32::from_rgb(0x13, 0x13, 0x13),
            ACTIVE_ROW: Color32::from_rgb(0x1a, 0x1b, 0x1b),
            SELECTED_ROW: Color32::from_rgb(0x11, 0x11, 0x11),
            ERROR: Color32::from_rgb(0xfb, 0x41, 0x4a),
            ERROR_FG: Color32::from_rgb(0xff, 0x64, 0x67),
            ERROR_SURFACE: Color32::from_rgb(0x30, 0x12, 0x14),
            WARNING: Color32::from_rgb(0xfe, 0x9a, 0x00),
            WARNING_FG: Color32::from_rgb(0xff, 0xb9, 0x00),
            WARNING_SURFACE: Color32::from_rgb(0x31, 0x21, 0x08),
            SUCCESS: Color32::from_rgb(0x34, 0xd3, 0x99),
            INFO: Color32::from_rgb(0x60, 0xa5, 0xfa),
            RUNNING: Color32::from_rgb(0x22, 0xd3, 0xee),
            WAITING: Color32::from_rgb(0x60, 0xa5, 0xfa),
        }
    }
}

static CURRENT: std::sync::RwLock<Palette> = std::sync::RwLock::new(Palette::graphite());

pub fn palette() -> Palette {
    *CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn install_palette(p: Palette) {
    *CURRENT
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = p;
}
pub const STATUS_STROKE: f32 = 1.0;

pub const SP_1: f32 = 4.0;
pub const SP_2: f32 = 8.0;
pub const SP_3: f32 = 12.0;
pub const SP_4: f32 = 16.0;
pub const ROW_COMPACT: f32 = 36.0;
pub const COMPOSER_MIN_HEIGHT: f32 = 48.0;
pub const COMPOSER_MAX_HEIGHT: f32 = 180.0;
/// sidebar の project / thread 行の高さ。ROW_COMPACT (36) はエディタ系 (agent.rs) 向けに残す。
pub const ROW_DENSE: f32 = 28.0;
/// 状態ドットの直径。半径は DOT_SIZE / 2.0 で導出する。
// 4px grid の半値例外: body 14px の cap height (~10px) に合わせる光学値。
pub const DOT_SIZE: f32 = 10.0;
pub const TAB_HEIGHT: f32 = 24.0;
// 4px grid の半値例外: タブ本体は TAB_HEIGHT で grid 整合しており、間隔のみ光学補正として 2px を使う。
pub const TAB_GAP: f32 = 2.0;
pub const TAB_MAX_WIDTH: f32 = 144.0;
pub const TOPBAR: f32 = 52.0;

/// Agents telemetry grid の列幅希望下限。最後の手段でこれを下回る縮小も許容する。
pub const AGENTS_COL_MIN: f32 = 56.0;
/// Agents telemetry grid の列幅上限。自然幅が大きすぎる列を抑制する。
pub const AGENTS_COL_MAX: f32 = 160.0;
/// Provider settings modal の幅上限。viewport 幅の 60% を超えないよう制限する。
pub const PROVIDER_MODAL_MAX_WIDTH: f32 = 720.0;
/// Agents telemetry grid のセル内テキスト左右の最小余白。
pub const CELL_PAD_X: f32 = 8.0;

pub const R_SM: u8 = 6;
pub const R_MD: u8 = 8;
pub const R_LG: u8 = 10;
pub const R_XL: u8 = 14;
pub const R_2XL: u8 = 20;
pub const R_PILL: u8 = u8::MAX;

pub const FONT_BODY: f32 = 14.0;
pub const FONT_SMALL: f32 = 12.0;
pub const FONT_MONO: f32 = 12.0;
pub const FONT_H1: f32 = 20.0;
pub const FONT_H2: f32 = 18.0;
pub const FONT_H3: f32 = 16.0;
pub const FONT_H4: f32 = 14.0;
pub const FONT_BADGE: f32 = 11.0;

pub fn text_style_h2() -> egui::TextStyle {
    egui::TextStyle::Name("h2".into())
}

pub fn text_style_h3() -> egui::TextStyle {
    egui::TextStyle::Name("h3".into())
}

pub fn text_style_h4() -> egui::TextStyle {
    egui::TextStyle::Name("h4".into())
}

pub fn text_style_badge() -> egui::TextStyle {
    egui::TextStyle::Name("badge".into())
}

pub fn state_color(state: ThreadState) -> Color32 {
    match state {
        ThreadState::Active => palette().ACCENT,
        ThreadState::Paused => palette().TEXT_MUTED,
        ThreadState::Running => palette().INFO,
        ThreadState::Waiting => palette().WAITING,
        ThreadState::Done => palette().SUCCESS,
        ThreadState::Error => palette().ERROR_FG,
    }
}

pub fn phase_color(phase: ThreadRunPhase) -> Color32 {
    match phase {
        ThreadRunPhase::Pending => palette().TEXT_MUTED,
        ThreadRunPhase::Running => palette().RUNNING,
        ThreadRunPhase::Waiting => palette().WAITING,
        ThreadRunPhase::Done => palette().SUCCESS,
        ThreadRunPhase::Error => palette().ERROR_FG,
    }
}

pub fn agent_phase_color(phase: AgentRunPhase) -> Color32 {
    match phase {
        AgentRunPhase::Pending => palette().TEXT_MUTED,
        AgentRunPhase::Running => palette().RUNNING,
        AgentRunPhase::Waiting => palette().WAITING,
        AgentRunPhase::Done => palette().SUCCESS,
        AgentRunPhase::Error => palette().ERROR_FG,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphite_palette_matches_legacy_values() {
        // Given the legacy Graphite RGB values, when constructing the palette,
        // then every color, including aliases, retains its exact value.
        let p = Palette::graphite();
        assert_eq!(p.CANVAS, Color32::from_rgb(0x0a, 0x0a, 0x0a));
        assert_eq!(p.SURFACE, Color32::from_rgb(0x11, 0x11, 0x11));
        assert_eq!(p.SURFACE_RAISED, Color32::from_rgb(0x14, 0x14, 0x14));
        assert_eq!(p.OVERLAY, Color32::from_rgb(0x19, 0x19, 0x19));
        assert_eq!(p.SIDEBAR, Color32::from_rgb(0x00, 0x00, 0x00));
        assert_eq!(p.TEXT, Color32::from_rgb(0xf5, 0xf5, 0xf5));
        assert_eq!(p.TEXT_MUTED, Color32::from_rgb(0x81, 0x81, 0x81));
        assert_eq!(p.BORDER, Color32::from_rgb(0x19, 0x19, 0x19));
        assert_eq!(p.INPUT, Color32::from_rgb(0x1e, 0x1e, 0x1e));
        assert_eq!(p.ACCENT, Color32::from_rgb(0x34, 0x6b, 0xf1));
        assert_eq!(p.ACCENT_FG, Color32::from_rgb(0xff, 0xff, 0xff));
        assert_eq!(p.HOVER_ROW, Color32::from_rgb(0x13, 0x13, 0x13));
        assert_eq!(p.ACTIVE_ROW, Color32::from_rgb(0x1a, 0x1b, 0x1b));
        assert_eq!(p.SELECTED_ROW, Color32::from_rgb(0x11, 0x11, 0x11));
        assert_eq!(p.ERROR, Color32::from_rgb(0xfb, 0x41, 0x4a));
        assert_eq!(p.ERROR_FG, Color32::from_rgb(0xff, 0x64, 0x67));
        assert_eq!(p.ERROR_SURFACE, Color32::from_rgb(0x30, 0x12, 0x14));
        assert_eq!(p.WARNING, Color32::from_rgb(0xfe, 0x9a, 0x00));
        assert_eq!(p.WARNING_FG, Color32::from_rgb(0xff, 0xb9, 0x00));
        assert_eq!(p.WARNING_SURFACE, Color32::from_rgb(0x31, 0x21, 0x08));
        assert_eq!(p.SUCCESS, Color32::from_rgb(0x34, 0xd3, 0x99));
        assert_eq!(p.INFO, Color32::from_rgb(0x60, 0xa5, 0xfa));
        assert_eq!(p.RUNNING, Color32::from_rgb(0x22, 0xd3, 0xee));
        assert_eq!(p.WAITING, Color32::from_rgb(0x60, 0xa5, 0xfa));
    }

    #[test]
    fn phase_indicator_tokens_are_distinct() {
        let colors = [
            palette().RUNNING,
            palette().WAITING,
            palette().ERROR_FG,
            palette().TEXT_MUTED,
            palette().SUCCESS,
            palette().WARNING_FG,
        ];
        let distinct: std::collections::HashSet<_> = colors.into_iter().collect();
        assert_eq!(distinct.len(), colors.len());
        assert_eq!(palette().WAITING, palette().INFO);
        assert_eq!(phase_color(ThreadRunPhase::Running), palette().RUNNING);
        assert_eq!(phase_color(ThreadRunPhase::Waiting), palette().WAITING);
        assert_eq!(agent_phase_color(AgentRunPhase::Running), palette().RUNNING);
        assert_eq!(agent_phase_color(AgentRunPhase::Waiting), palette().WAITING);
    }

    #[test]
    fn state_color_is_exhaustive_and_distinct() {
        let colors = [
            state_color(ThreadState::Active),
            state_color(ThreadState::Paused),
            state_color(ThreadState::Running),
            state_color(ThreadState::Waiting),
            state_color(ThreadState::Done),
            state_color(ThreadState::Error),
        ];
        let distinct: std::collections::HashSet<_> = colors.iter().copied().collect();
        assert_eq!(distinct.len(), colors.len() - 1);
    }

    #[test]
    fn radius_consts_are_monotonic() {
        const _: () = {
            assert!(R_SM < R_MD);
            assert!(R_MD < R_LG);
            assert!(R_LG < R_XL);
            assert!(R_XL < R_PILL);
        };
    }

    #[test]
    fn spacing_consts_are_multiples_of_four() {
        for value in [
            SP_1,
            SP_2,
            SP_3,
            SP_4,
            ROW_COMPACT,
            ROW_DENSE,
            TAB_HEIGHT,
            TOPBAR,
            AGENTS_COL_MIN,
            AGENTS_COL_MAX,
            CELL_PAD_X,
        ] {
            assert!(value % 4.0 == 0.0, "{value} is not a multiple of 4");
        }
    }

    #[test]
    fn agents_column_bounds_are_ordered() {
        const _: () = assert!(AGENTS_COL_MIN < AGENTS_COL_MAX);
    }

    #[test]
    fn dense_row_is_shorter_than_compact_row() {
        const _: () = assert!(ROW_DENSE < ROW_COMPACT);
    }

    #[test]
    fn dot_fits_inside_dense_row() {
        const _: () = assert!(DOT_SIZE < ROW_DENSE / 2.0);
    }
}
