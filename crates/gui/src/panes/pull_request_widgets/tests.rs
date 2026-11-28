use super::*;
use egui_kittest::{Harness, kittest::NodeT, kittest::Queryable};

fn input(limit: u32, used: u32, limit_reached: bool) -> AutoFixInput<'static> {
    AutoFixInput {
        thread: "t1",
        link_revision: 4,
        open: true,
        failing: true,
        checks_pending: false,
        enabled: !limit_reached,
        destination: Some("t1"),
        policy_link_revision: Some(4),
        policy_revision: 9,
        used,
        reserved: 0,
        limit,
        limit_reached,
        global_enabled: true,
        cooldown_until_ns: None,
        stop_reason: None,
        head_notice: None,
        this_stopped: false,
        suppressed: false,
        head_from_agent: true,
        head_sha: Some("abc"),
    }
}

fn chip_texts(controls: &AutoFixControls) -> Vec<&str> {
    controls
        .chips
        .iter()
        .map(|chip| chip.text.as_str())
        .collect()
}

#[test]
fn the_count_is_told_against_the_limit_the_owner_gives() {
    let controls = auto_fix_controls_of(&input(5, 2, false), 0);
    assert!(controls.on_here);
    assert_eq!(chip_texts(&controls), ["3 left"]);
    assert_eq!(
        controls.chips[0].hover.as_deref(),
        Some("2 of 5 automatic sends used")
    );
    assert!(controls.reset.is_none());
    assert!(controls.send.enabled);
}

#[test]
fn the_limit_stop_is_what_the_owner_says_not_a_count_reached() {
    let stopped = auto_fix_controls_of(&input(5, 2, true), 0);
    assert_eq!(chip_texts(&stopped), ["Limit reached 2/5"]);
    assert!(!stopped.toggle.enabled);
    assert!(stopped.reset.as_ref().is_some_and(|reset| reset.enabled));
    let mut running = input(5, 5, false);
    running.enabled = true;
    let running = auto_fix_controls_of(&running, 0);
    assert!(running.reset.is_none());
    assert_eq!(chip_texts(&running), ["0 left"]);
}

#[test]
fn a_failure_expands_the_bar_once_and_a_repoll_of_it_never_again() {
    let mut memory = BarMemory::default();
    assert!(
        !memory.observe(Some(1), Some(PrGlyph::Failing)),
        "the first view is a baseline"
    );
    assert!(!memory.observe(Some(1), Some(PrGlyph::Pending)));
    assert!(memory.observe(Some(1), Some(PrGlyph::Failing)));
    assert!(!memory.observe(Some(1), Some(PrGlyph::Failing)));
    assert!(!memory.observe(Some(1), Some(PrGlyph::Unknown)));
    assert!(
        !memory.observe(Some(1), Some(PrGlyph::Failing)),
        "recovering into the same failure is no move"
    );
    assert!(
        !memory.observe(Some(2), Some(PrGlyph::Failing)),
        "a new link starts a new baseline"
    );
}

#[test]
fn a_check_label_nobody_knows_is_shown_as_it_is() {
    assert_eq!(conclusion_word(Some("timed_out")), "timed out");
    assert_eq!(conclusion_word(Some("other")), "unfinished");
    assert_eq!(conclusion_word(None), "unfinished");
    assert_eq!(conclusion_word(Some("brand_new")), "brand_new");
    let unknown = CheckRow {
        name: "x",
        status: "waiting",
        conclusion: None,
    };
    assert_eq!(verdict_of_labels(&[unknown], true), Verdict::Pending);
}

fn link_button_disabled(input: &str, ready: bool) -> bool {
    let reading = Reading::unchecked(input);
    let mut text = input.to_owned();
    let harness = Harness::builder().build_ui(move |ui| {
        link_dialog_view(ui.ctx(), &mut text, &reading, ready, None);
    });
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Link")
        .accesskit_node()
        .is_disabled()
}

#[test]
fn text_the_core_reads_enables_link_only_when_the_thread_is_ready() {
    assert_eq!(Reading::unchecked("  "), Reading::Empty);
    assert_eq!(Reading::unchecked("o/r#7"), Reading::Unchecked);
    assert!(!link_button_disabled("o/r#7", true));
    assert!(link_button_disabled("o/r#7", false));
    assert!(link_button_disabled("", true));
}
