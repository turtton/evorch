//! The Features modal in the full window: the ⚙ menu opens it over the core's view, one Save
//! sends the changed sections, the restart band follows the view, and the Pull requests tab
//! opens it and follows its result.
use super::*;
use egui::accesskit::Role;
use egui_kittest::kittest::NodeT;
use std::cell::Cell;
use workbench_protocol::{
    commands::{CommandReply, CommandResult},
    namespaces::{
        pull_requests::{PullRequestsQuery, PullRequestsReply},
        settings::{
            BrowserFeature, FeatureSection, FeatureSettingsView, FeatureValues, PullRequestFeature,
            SettingsCommand, SettingsOperationKind, SettingsOperationView, SettingsQuery,
            SettingsReply, SettingsResult, SettingsRevision, ThreadCleanupFeature,
        },
    },
    operation::{OperationState, OperationView},
};

fn values(pull_requests_on: bool) -> FeatureValues {
    FeatureValues {
        pull_requests: PullRequestFeature {
            enabled: pull_requests_on,
            auto_fix_enabled: false,
            include_in_review: true,
            pending_poll_seconds: 30,
            stable_poll_seconds: 300,
            timeout_seconds: 30,
        },
        agent_web_tools: true,
        browser: BrowserFeature {
            enabled: true,
            executable: None,
            max_pages: 8,
            max_tabs: 4,
            coordinate_actions: false,
        },
        thread_cleanup: ThreadCleanupFeature {
            auto_archive_after_days: 0,
            archived_delete_after_days: 30,
        },
    }
}

fn view(revision: char, saved: FeatureValues, restart: &[FeatureSection]) -> FeatureSettingsView {
    let mut running = saved.clone();
    // What the core runs differs from the file in the sections that wait for a restart.
    if restart.contains(&FeatureSection::Browser) {
        running.browser.enabled = false;
    }
    FeatureSettingsView {
        revision: SettingsRevision::new(revision.to_string().repeat(64)).unwrap(),
        saved,
        running,
        restart_pending: restart.to_vec(),
        problem: None,
    }
}

fn thread_read(thread: &ThreadId, tracking: bool) -> ThreadPullRequest {
    let mut read = pull_request_read(thread);
    read.tracking_enabled = tracking;
    read.link = None;
    read.snapshot = None;
    read.auto_fix = None;
    read
}

use workbench_protocol::namespaces::pull_requests::ThreadPullRequest;

fn pull_request_read(thread: &ThreadId) -> ThreadPullRequest {
    serde_json::from_value(json!({
        "thread": thread.as_str(),
        "tracking_enabled": true,
        "owned_here": true,
        "lifecycle_revision": "3",
        "transition_seq": "1",
        "candidates": [],
        "head_attribution": "agent",
    }))
    .unwrap()
}

/// What the core answers: the feature view in `current`, and the pull request reads with
/// tracking as `tracking` says. Returns the settings commands sent.
struct Core {
    current: std::cell::RefCell<FeatureSettingsView>,
    tracking: Cell<bool>,
    reads: Cell<usize>,
}

impl Screen {
    fn serve_features(&mut self, core: &Core) -> Vec<SettingsCommand> {
        let mut commands = Vec::new();
        for _ in 0..16 {
            self.harness.run_steps(1);
            while let Ok(action) = self.actions.try_recv() {
                let Action::Feature(FeatureAction {
                    feature,
                    tag,
                    request,
                }) = action
                else {
                    continue;
                };
                let reply = match request {
                    FeatureRequest::Query(Query::Workspace(query)) => {
                        FeatureReply::Query(Ok(QueryReply::Workspace(answer(&query))))
                    }
                    FeatureRequest::Query(Query::PullRequests(
                        PullRequestsQuery::GetThreadPullRequest { thread },
                    )) => {
                        core.reads.set(core.reads.get() + 1);
                        FeatureReply::Query(Ok(QueryReply::PullRequests(
                            PullRequestsReply::Thread(Box::new(thread_read(
                                &thread,
                                core.tracking.get(),
                            ))),
                        )))
                    }
                    FeatureRequest::Query(Query::Settings(
                        SettingsQuery::GetFeatureSettings {},
                    )) => FeatureReply::Query(Ok(QueryReply::Settings(
                        SettingsReply::FeatureSettings(Box::new(core.current.borrow().clone())),
                    ))),
                    FeatureRequest::Command(workbench_protocol::commands::Command::Settings(
                        command @ SettingsCommand::SaveFeatureSettings(_),
                    )) => {
                        let SettingsCommand::SaveFeatureSettings(save) = &command else {
                            unreachable!()
                        };
                        let reply = FeatureReply::Command(Ok(CommandReply::with_result(
                            CommandResult::Settings(SettingsResult::Operation(
                                SettingsOperationView {
                                    operation: OperationView {
                                        operation_id: save.operation_id.clone(),
                                        state: OperationState::Ready,
                                        revision: Revision(1),
                                        result: None,
                                    },
                                    kind: SettingsOperationKind::SaveFeatureSettings,
                                    revision: None,
                                    message: None,
                                },
                            )),
                        )));
                        commands.push(command);
                        reply
                    }
                    _ => continue,
                };
                self.shared
                    .lock()
                    .unwrap()
                    .features
                    .reply(feature, tag, reply);
            }
        }
        commands
    }

    fn open_features_from_menu(&mut self, core: &Core) {
        self.serve_features(core);
        self.harness.get_by_label("⚙").click();
        self.harness.run_steps(2);
        self.harness.get_by_label_contains("Features").click();
        self.serve_features(core);
        self.harness.run_steps(8);
    }

    fn switch(&mut self, label: &str) {
        self.harness
            .get_by_role_and_label(Role::CheckBox, label)
            .click();
        self.harness.run_steps(4);
    }

    fn enabled(&self, label: &str) -> bool {
        !self
            .harness
            .get_by_role_and_label(Role::Button, label)
            .accesskit_node()
            .is_disabled()
    }
}

// Given: a core whose browser is saved on but runs off (a restart is waiting)
// When: ⚙ → Features
// Then: the four cards and nothing else are drawn, the band names the browser and offers
//       the way to the Restart screen, the web tools and PR view say they apply now, and
//       nothing is sent before Save
#[test]
fn the_features_modal_shows_four_cards_and_the_restart_band() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let core = Core {
        current: view('a', values(true), &[FeatureSection::Browser]).into(),
        tracking: Cell::new(true),
        reads: Cell::new(0),
    };
    screen.open_features_from_menu(&core);
    let harness = &screen.harness;
    for title in [
        "Pull request view",
        "Agent web tools",
        "Agent browser",
        "Thread cleanup",
    ] {
        assert!(harness.get_all_by_label(title).count() >= 1, "{title}");
    }
    assert_eq!(harness.get_all_by_label_contains("Applies now").count(), 2);
    assert!(
        harness
            .query_all_by_label_contains("Restart to apply")
            .count()
            >= 2
    );
    harness.get_by_label_contains("Agent browser saved. A core restart applies");
    harness.get_by_label("Restart core");
    assert!(
        harness.query_by_label("Session only").is_none(),
        "no session-only switch"
    );
    assert!(!screen.enabled("Save"), "nothing to save yet");
    screen.capture("features");
    assert!(screen.serve_features(&core).is_empty());
}

// Given: the Features modal on a view with PR view off
// When: PR view and cleanup are changed and Save is pressed
// Then: one command carries only those two sections, quoting the revision read; Discard
//       before it restores the view
#[test]
fn one_save_sends_only_the_changed_sections_against_the_revision_read() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let core = Core {
        current: view('a', values(false), &[]).into(),
        tracking: Cell::new(false),
        reads: Cell::new(0),
    };
    screen.open_features_from_menu(&core);
    screen.switch("Pull request view");
    screen.harness.run_steps(4);
    screen.harness.get_by_label_contains("1 unsaved change");
    screen.harness.get_by_label("Discard").click();
    screen.harness.run_steps(4);
    assert!(
        screen
            .harness
            .query_by_label_contains("1 unsaved change")
            .is_none()
    );
    screen.switch("Pull request view");
    screen.switch("Archive inactive threads");
    screen.harness.run_steps(4);
    screen.harness.get_by_label_contains("2 unsaved change");
    screen.harness.get_by_label("Save").click();
    let commands = screen.serve_features(&core);
    let [SettingsCommand::SaveFeatureSettings(save)] = commands.as_slice() else {
        panic!("one save was expected: {commands:?}");
    };
    assert_eq!(save.expected_revision.as_str(), "a".repeat(64));
    assert!(save.changes.pull_requests.is_some_and(|pr| pr.enabled));
    assert!(save.changes.thread_cleanup.is_some());
    assert!(save.changes.browser.is_none());
    assert!(save.changes.agent_web_tools.is_none());
}

// Given: the Pull requests tab of a thread while PR view is off
// When: Open Features is pressed, PR view is switched on and saved
// Then: the Features modal opens, and once the save completes the tab reads the thread
//       again and the "off" message is gone, without selecting another thread
#[test]
fn the_pull_requests_tab_opens_features_and_follows_the_saved_result() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let core = Core {
        current: view('a', values(false), &[]).into(),
        tracking: Cell::new(false),
        reads: Cell::new(0),
    };
    screen.serve_features(&core);
    shell::focus(
        &mut screen.harness.state_mut().dock,
        CoreTab::Feature(FeatureId::PullRequests),
    );
    screen.serve_features(&core);
    screen.harness.get_by_label("Pull request view is off");
    screen.capture("pull-requests-off");
    screen.harness.get_by_label("Open Features").click();
    screen.serve_features(&core);
    screen.harness.run_steps(8);
    assert!(screen.harness.get_all_by_label("Agent web tools").count() >= 1);
    let reads = core.reads.get();

    core.tracking.set(true);
    *core.current.borrow_mut() = view('b', values(true), &[]);
    screen.switch("Pull request view");
    screen.harness.run_steps(4);
    screen.harness.get_by_label("Save").click();
    let commands = screen.serve_features(&core);
    assert_eq!(commands.len(), 1);
    assert!(core.reads.get() > reads, "the tab read the thread again");
    assert!(
        screen
            .harness
            .query_by_label("Pull request view is off")
            .is_none()
    );
}

// Given: the smallest window the shell supports
// When: ⚙ → Features is opened
// Then: the footer (Save and Discard) stays inside the window, and the cards past the
//       visible part of the body are reached by scrolling, not cut off
#[test]
fn at_the_minimum_window_size_save_stays_in_view_and_the_cards_scroll() {
    let size = egui::vec2(960.0, 600.0);
    let mut screen = screen(size);
    let core = Core {
        current: view('a', values(true), &[FeatureSection::Browser]).into(),
        tracking: Cell::new(true),
        reads: Cell::new(0),
    };
    screen.open_features_from_menu(&core);
    let inside =
        |rect: egui::Rect| egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(rect);
    for label in ["Save", "Discard", "Close"] {
        let node = screen.harness.get_by_label(label);
        let bounds = node.accesskit_node().bounding_box().expect(label);
        let rect = egui::Rect::from_min_max(
            egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
            egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
        );
        assert!(inside(rect), "{label} is outside the window: {rect:?}");
    }
    screen.harness.get_by_label("Thread cleanup").scroll_to_me();
    screen.harness.run_steps(4);
    screen.capture("features-min");
    screen
        .harness
        .get_by_role_and_label(Role::CheckBox, "Archive inactive threads");
}
