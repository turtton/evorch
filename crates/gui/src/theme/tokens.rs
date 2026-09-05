use egui::Color32;
use event_bus::AgentRunPhase;
use workspace_ui::{ThreadRunPhase, ThreadState};

pub const CANVAS: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0a);
pub const SURFACE: Color32 = Color32::from_rgb(0x11, 0x11, 0x11);
pub const SURFACE_RAISED: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
pub const OVERLAY: Color32 = Color32::from_rgb(0x19, 0x19, 0x19);
pub const SIDEBAR: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
pub const TEXT: Color32 = Color32::from_rgb(0xf5, 0xf5, 0xf5);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x81, 0x81, 0x81);
pub const BORDER: Color32 = Color32::from_rgb(0x19, 0x19, 0x19);
pub const INPUT: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x1e);
pub const ACCENT: Color32 = Color32::from_rgb(0x34, 0x6b, 0xf1);
pub const ACCENT_FG: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
pub const HOVER_ROW: Color32 = Color32::from_rgb(0x13, 0x13, 0x13);
pub const ACTIVE_ROW: Color32 = Color32::from_rgb(0x1a, 0x1b, 0x1b);
pub const SELECTED_ROW: Color32 = SURFACE;
pub const ERROR: Color32 = Color32::from_rgb(0xfb, 0x41, 0x4a);
pub const ERROR_FG: Color32 = Color32::from_rgb(0xff, 0x64, 0x67);
pub const ERROR_SURFACE: Color32 = Color32::from_rgb(0x30, 0x12, 0x14);
pub const WARNING: Color32 = Color32::from_rgb(0xfe, 0x9a, 0x00);
pub const WARNING_FG: Color32 = Color32::from_rgb(0xff, 0xb9, 0x00);
pub const WARNING_SURFACE: Color32 = Color32::from_rgb(0x31, 0x21, 0x08);
pub const SUCCESS: Color32 = Color32::from_rgb(0x34, 0xd3, 0x99);
pub const INFO: Color32 = Color32::from_rgb(0x60, 0xa5, 0xfa);

pub const SP_1: f32 = 4.0;
pub const SP_2: f32 = 8.0;
pub const SP_3: f32 = 12.0;
pub const SP_4: f32 = 16.0;
pub const ROW_COMPACT: f32 = 36.0;
pub const TAB_HEIGHT: f32 = 24.0;
pub const TAB_GAP: f32 = 2.0;
pub const TAB_MAX_WIDTH: f32 = 144.0;
pub const TOPBAR: f32 = 52.0;

pub const R_SM: u8 = 6;
pub const R_MD: u8 = 8;
pub const R_LG: u8 = 10;
pub const R_XL: u8 = 14;
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

pub const fn state_color(state: ThreadState) -> Color32 {
    match state {
        ThreadState::Active => ACCENT,
        ThreadState::Paused => TEXT_MUTED,
        ThreadState::Running => INFO,
        ThreadState::Waiting => WARNING_FG,
        ThreadState::Done => SUCCESS,
        ThreadState::Error => ERROR_FG,
    }
}

pub fn phase_color(phase: ThreadRunPhase) -> Color32 {
    match phase {
        ThreadRunPhase::Pending => TEXT_MUTED,
        ThreadRunPhase::Running => INFO,
        ThreadRunPhase::Waiting => WARNING_FG,
        ThreadRunPhase::Done => SUCCESS,
        ThreadRunPhase::Error => ERROR_FG,
    }
}

pub const fn agent_phase_color(phase: AgentRunPhase) -> Color32 {
    match phase {
        AgentRunPhase::Pending => TEXT_MUTED,
        AgentRunPhase::Running => INFO,
        AgentRunPhase::Waiting => WARNING_FG,
        AgentRunPhase::Done => SUCCESS,
        AgentRunPhase::Error => ERROR_FG,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(distinct.len(), colors.len());
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
        for value in [SP_1, SP_2, SP_3, SP_4, ROW_COMPACT, TAB_HEIGHT, TOPBAR] {
            assert!(value % 4.0 == 0.0, "{value} is not a multiple of 4");
        }
    }
}
