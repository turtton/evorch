//! Tab attention shared by the embedded workbench and the core client: run phases and
//! acknowledgements only, never where they come from.

pub mod ack;

use std::collections::BTreeMap;

use egui::Color32;
use egui_dock::TabStyle;
use workbench_protocol::views::RunPhase;
use workspace_ui::ThreadRunPhase;

use crate::theme::tokens::palette;
use ack::AttentionAck;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PaneAttention {
    None,
    Info,
    Error,
}

impl PaneAttention {
    #[must_use]
    pub fn color(self) -> Option<Color32> {
        match self {
            PaneAttention::None => None,
            PaneAttention::Info => Some(palette().INFO),
            PaneAttention::Error => Some(palette().ERROR_FG),
        }
    }
}

#[must_use]
pub const fn thread_phase_attention(phase: ThreadRunPhase) -> PaneAttention {
    match phase {
        ThreadRunPhase::Pending | ThreadRunPhase::Running => PaneAttention::None,
        ThreadRunPhase::Done | ThreadRunPhase::Waiting | ThreadRunPhase::Stopped => {
            PaneAttention::Info
        }
        ThreadRunPhase::Error => PaneAttention::Error,
    }
}

/// The core reports a stopped run as cancelled.
#[must_use]
pub const fn run_phase(phase: RunPhase) -> ThreadRunPhase {
    match phase {
        RunPhase::Pending => ThreadRunPhase::Pending,
        RunPhase::Running => ThreadRunPhase::Running,
        RunPhase::WaitingForUser | RunPhase::WaitingForApproval => ThreadRunPhase::Waiting,
        RunPhase::Done => ThreadRunPhase::Done,
        RunPhase::Failed => ThreadRunPhase::Error,
        RunPhase::Cancelled => ThreadRunPhase::Stopped,
    }
}

pub fn strongest<'a>(acks: impl IntoIterator<Item = &'a AttentionAck>) -> PaneAttention {
    acks.into_iter()
        .filter(|ack| ack.is_unread())
        .map(|ack| thread_phase_attention(ack.phase()))
        .fold(PaneAttention::None, PaneAttention::max)
}

pub fn acknowledged_attention<P: PartialEq, R>(
    acks: &BTreeMap<(P, R), AttentionAck>,
    panel: &P,
) -> PaneAttention {
    strongest(
        acks.iter()
            .filter(|((id, _), _)| id == panel)
            .map(|(_, ack)| ack),
    )
}

/// egui_dock paints tab titles without accesskit nodes, so the "• " prefix never reaches
/// label-based queries. U+2022 is used because egui's bundled fonts lack U+25CF.
#[must_use]
pub fn tab_title(title: String, attention: PaneAttention) -> egui::WidgetText {
    match attention.color() {
        Some(color) => egui::RichText::new(format!("• {title}"))
            .color(color)
            .into(),
        None => title.into(),
    }
}

#[must_use]
pub fn tab_style(base: &TabStyle, attention: PaneAttention) -> Option<TabStyle> {
    attention
        .color()
        .map(|color| crate::theme::dock::attention_tab_style(base, color))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(phase: ThreadRunPhase) -> AttentionAck {
        let mut ack = AttentionAck::default();
        ack.observe(phase).expect("first observation");
        ack
    }

    #[test]
    fn core_run_phases_map_to_workbench_attention() {
        for (phase, expected) in [
            (RunPhase::Pending, PaneAttention::None),
            (RunPhase::Running, PaneAttention::None),
            (RunPhase::WaitingForUser, PaneAttention::Info),
            (RunPhase::WaitingForApproval, PaneAttention::Info),
            (RunPhase::Done, PaneAttention::Info),
            (RunPhase::Cancelled, PaneAttention::Info),
            (RunPhase::Failed, PaneAttention::Error),
        ] {
            assert_eq!(
                thread_phase_attention(run_phase(phase)),
                expected,
                "{phase:?}"
            );
        }
    }

    #[test]
    fn strongest_folds_unread_acks_only() {
        // Given: a read failure, a running run and an unread done run
        let mut read = ack(ThreadRunPhase::Error);
        let revision = read.revision();
        assert!(read.acknowledge_surface(Some(&revision), Some(true)));
        let running = ack(ThreadRunPhase::Running);
        let done = ack(ThreadRunPhase::Done);

        // Then: only the unread done run counts, and an unread failure outranks it
        assert_eq!(strongest([&read, &running, &done]), PaneAttention::Info);
        let failed = ack(ThreadRunPhase::Error);
        assert_eq!(strongest([&done, &failed]), PaneAttention::Error);
        assert_eq!(strongest([&read, &running]), PaneAttention::None);
    }

    #[test]
    fn acknowledged_attention_reads_one_panel() {
        let acks = BTreeMap::from([
            (("a", "run-1"), ack(ThreadRunPhase::Done)),
            (("b", "run-2"), ack(ThreadRunPhase::Error)),
        ]);
        assert_eq!(acknowledged_attention(&acks, &"a"), PaneAttention::Info);
        assert_eq!(acknowledged_attention(&acks, &"b"), PaneAttention::Error);
        assert_eq!(acknowledged_attention(&acks, &"c"), PaneAttention::None);
    }

    #[test]
    fn only_flagged_tabs_get_a_dot_and_a_style() {
        assert_eq!(
            tab_title("Conversation".to_owned(), PaneAttention::None).text(),
            "Conversation"
        );
        assert_eq!(
            tab_title("Conversation".to_owned(), PaneAttention::Error).text(),
            "• Conversation"
        );
        let base = TabStyle::default();
        assert!(tab_style(&base, PaneAttention::None).is_none());
        let style = tab_style(&base, PaneAttention::Info).expect("flagged style");
        assert_eq!(style.inactive.text_color, palette().INFO);
    }
}
