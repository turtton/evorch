use super::*;
use crate::panes::tab_attention::PaneAttention;
use crate::theme::icons::{self, with_icon};
use crate::theme::tokens::palette;

fn conversation() -> String {
    format!("• {}", with_icon(icons::CHAT_CIRCLE_TEXT, "Conversation"))
}

fn notifications() -> String {
    format!("• {}", with_icon(icons::BELL, "Notifications"))
}

pub(super) fn flagged(screen: &Screen) -> Vec<String> {
    fn collect(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) if text.galley.text().starts_with("• ") => {
                out.push(text.galley.text().to_owned());
            }
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for clipped in &screen.harness.output().shapes {
        collect(&clipped.shape, &mut out);
    }
    out.sort();
    out
}

impl Screen {
    fn set_focused(&mut self, focused: bool) {
        self.harness
            .input_mut()
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .focused = Some(focused);
        self.harness.run_steps(2);
    }

    fn set_phase(&mut self, run: &str, phase: RunPhase) {
        self.shared
            .lock()
            .unwrap()
            .cache
            .runs
            .get_mut(run)
            .unwrap()
            .phase = phase;
        self.harness.run_steps(2);
    }

    fn attention(&self, tab: CoreTab) -> PaneAttention {
        self.harness.state().attention.of(tab)
    }
}

// Given: thr-1 selected, its root run-1 waiting for approval, the window unfocused
// When: the window gains focus, loses it, run-1 fails, and it gains focus again
// Then: the Conversation tab is flagged until drawn with focus, flagged as an error once
//       run-1 fails, and read again on the next focused frame
#[test]
fn the_conversation_tab_flags_its_root_until_drawn_with_focus() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    assert_eq!(screen.attention(CoreTab::Conversation), PaneAttention::Info);
    assert!(flagged(&screen).contains(&conversation()));
    screen.harness.get_by_label("Conversation tab");

    screen.set_focused(true);
    assert_eq!(screen.attention(CoreTab::Conversation), PaneAttention::None);
    assert!(!flagged(&screen).contains(&conversation()));
    screen.harness.get_by_label("Conversation tab");

    screen.set_focused(false);
    screen.set_phase("run-1", RunPhase::Failed);
    let attention = screen.attention(CoreTab::Conversation);
    assert_eq!(attention, PaneAttention::Error);
    assert_eq!(attention.color(), Some(palette().ERROR_FG));
    assert!(flagged(&screen).contains(&conversation()));

    screen.set_focused(true);
    assert_eq!(screen.attention(CoreTab::Conversation), PaneAttention::None);
}

// Given: the Browser tab in front of the Conversation, the window focused
// When: thr-7, whose root run-7 failed, is selected
// Then: the hidden Conversation tab stays flagged as an error until it is brought forward
#[test]
fn a_hidden_conversation_tab_stays_flagged_until_shown() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    shell::focus(
        &mut screen.harness.state_mut().dock,
        CoreTab::Feature(FeatureId::Browser),
    );
    screen.set_focused(true);

    screen
        .harness
        .get_by_label("Migrate the lane table")
        .click();
    screen.settle();
    screen.harness.run_steps(2);
    assert_eq!(
        screen.harness.state().selected,
        Some(ThreadId::new("thr-7").unwrap())
    );
    assert_eq!(
        screen.attention(CoreTab::Conversation),
        PaneAttention::Error
    );
    assert!(flagged(&screen).contains(&conversation()));
    screen.harness.get_by_label("Conversation tab");
    screen.capture("attention-hidden");

    shell::focus(&mut screen.harness.state_mut().dock, CoreTab::Conversation);
    screen.harness.run_steps(2);
    assert_eq!(screen.attention(CoreTab::Conversation), PaneAttention::None);
    assert!(!flagged(&screen).contains(&conversation()));
}

// Given: every root run of the fixture failed and the Plan tab open
// When: the window is drawn unfocused, then with focus
// Then: only the Conversation and Notifications tabs carry the dot (the Execution and
//       Teams areas, the Plan and the local panes never do), and both are read with focus
#[test]
fn only_the_conversation_run_and_notification_tabs_ask_for_attention() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    {
        let mut shared = screen.shared.lock().unwrap();
        for run in shared.cache.runs.values_mut() {
            run.phase = RunPhase::Failed;
        }
    }
    shell::focus_beside(
        &mut screen.harness.state_mut().dock,
        CoreTab::Plan,
        CoreTab::Feature(FeatureId::Execution),
    );
    shell::focus(&mut screen.harness.state_mut().dock, CoreTab::Terminal);
    screen.harness.run_steps(2);
    let mut expected = vec![conversation(), notifications()];
    expected.sort();
    assert_eq!(flagged(&screen), expected);
    for tab in [
        CoreTab::Projects,
        CoreTab::Plan,
        CoreTab::Diff,
        CoreTab::Terminal,
        CoreTab::Feature(FeatureId::Execution),
        CoreTab::Feature(FeatureId::Teams),
        CoreTab::Feature(FeatureId::Lanes),
    ] {
        assert_eq!(screen.attention(tab), PaneAttention::None, "{tab:?}");
    }

    screen.set_focused(true);
    assert_eq!(screen.harness.state().notifications.unread_count(), 0);
    assert!(flagged(&screen).is_empty(), "{:?}", flagged(&screen));
}

// Given: thr-1 has a superseded root run-0 beside its current root run-1, and the
//        Conversation tab was read with focus
// When: the window loses focus and the session restarts, so thr-1's detail (which names
//       run-1) is missing for a while and comes back
// Then: the Conversation tab stays read throughout
#[test]
fn a_read_conversation_stays_read_while_its_root_is_fetched_again() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    {
        let mut shared = screen.shared.lock().unwrap();
        let superseded: RunSummary = view(json!({
            "id": "run-0", "thread": "thr-1", "role": "orchestrator",
            "phase": "done", "revision": "1",
        }));
        shared.cache.runs.insert("run-0".into(), superseded);
    }
    screen.settle();
    screen.set_focused(true);
    assert_eq!(screen.attention(CoreTab::Conversation), PaneAttention::None);

    screen.set_focused(false);
    screen
        .shared
        .lock()
        .unwrap()
        .features
        .begin_session(&[], Vec::new());
    screen.harness.run_steps(2);
    assert_eq!(screen.attention(CoreTab::Conversation), PaneAttention::None);

    screen.settle();
    screen.harness.run_steps(2);
    assert_eq!(screen.attention(CoreTab::Conversation), PaneAttention::None);
    assert!(!flagged(&screen).contains(&conversation()));
}
