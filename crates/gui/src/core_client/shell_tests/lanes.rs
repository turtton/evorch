use super::*;
use egui_kittest::kittest::NodeT;
use workbench_protocol::commands::Command;
use workbench_protocol::namespaces::{
    ConversationCommand, ExecutionQuery, LanesCommand, LanesQuery,
};

const LANE_TITLE: &str = "↳ Auth regression tests";

fn lanes_reply(payload: serde_json::Value) -> QueryReply {
    view(json!({"type": "lanes", "payload": payload}))
}

fn lane_summary() -> serde_json::Value {
    json!({"lane_id": "lane-4", "thread_id": "thr-4", "lead_thread_id": "thr-1",
        "policy": "attached", "management_revision": "5", "brief_revision": "2",
        "objective_preview": "Cover the redirect with tests", "generation": "1",
        "start": "started", "last_activity_at": "1", "latest_report_status": "working",
        "revision": "1"})
}

/// The lane's detail as `GetLane` answers it: its lead is stopped with two notices held.
fn held_detail() -> serde_json::Value {
    json!({
        "summary": lane_summary(),
        "latest_report": {"seq": "3", "status": "working",
            "summary": "Redirect tests written; running the suite", "needs_human": false,
            "management_revision": "5", "brief_revision": "2", "created_at": "1"},
        "delivery": {"held": true, "pending_count": 2},
        "attention": []})
}

/// Answers a lanes query; the lane is listed as `detail` summarizes it.
fn answer_lanes(query: &Query, detail: &serde_json::Value) -> QueryReply {
    let summary = detail["summary"].clone();
    match query {
        Query::Lanes(LanesQuery::ListLanes(list)) => {
            let items = if list.detached || list.lead_thread.as_str() != "thr-1" {
                vec![]
            } else {
                vec![summary]
            };
            lanes_reply(json!({"type": "lanes", "payload": {"items": items,
                "snapshot_revision": "1"}}))
        }
        Query::Lanes(LanesQuery::GetLane { .. }) => {
            lanes_reply(json!({"type": "lane", "payload": detail}))
        }
        Query::Lanes(LanesQuery::GetLaneClosePreview { .. }) => {
            lanes_reply(json!({"type": "lane_close_preview", "payload": {
                "lane_id": "lane-4", "thread_id": "thr-4",
                "objective": "Cover the redirect with tests",
                "acceptance_criteria": ["A `next` inside the site is honoured",
                    "An invalid `next` falls back to /home"],
                "brief_revision": "2", "management_revision": "5", "request_revision": "6",
                "open_question_count": 1, "running": true}}))
        }
        Query::Execution(ExecutionQuery::ListRunDetails(_)) => view(json!({"type": "execution",
            "payload": {"type": "run_details", "payload": {"items": [{
                "summary": {"id": "run-1", "thread": "thr-1", "role": "orchestrator",
                    "phase": "running", "revision": "1"},
                "name": "run-1", "lifecycle": "active", "source": "live", "child_count": 0,
                "revision": "1"}], "snapshot_revision": "1"}}})),
        other => panic!("the lanes test does not answer {other:?}"),
    }
}

impl Screen {
    fn settle_lanes(&mut self) -> (Vec<Query>, Vec<Action>) {
        self.settle_lanes_with(&held_detail())
    }

    /// Answers the workspace and lanes queries, the lane's `GetLane` with `detail`, for a
    /// fixed number of frames; returns the lanes queries asked and the other actions.
    fn settle_lanes_with(&mut self, detail: &serde_json::Value) -> (Vec<Query>, Vec<Action>) {
        let (mut asked, mut others) = (Vec::new(), Vec::new());
        for _ in 0..12 {
            self.harness.run_steps(1);
            while let Ok(action) = self.actions.try_recv() {
                match action {
                    Action::Feature(FeatureAction {
                        feature: FeatureId::Workspace,
                        tag,
                        request: FeatureRequest::Query(Query::Workspace(query)),
                    }) => {
                        let reply = FeatureReply::Query(Ok(QueryReply::Workspace(answer(&query))));
                        let mut shared = self.shared.lock().unwrap();
                        shared.features.reply(FeatureId::Workspace, tag, reply);
                    }
                    Action::Feature(FeatureAction {
                        feature: FeatureId::Lanes,
                        tag,
                        request: FeatureRequest::Query(query),
                    }) => {
                        let reply = FeatureReply::Query(Ok(answer_lanes(&query, detail)));
                        let mut shared = self.shared.lock().unwrap();
                        shared.features.reply(FeatureId::Lanes, tag, reply);
                        asked.push(query);
                    }
                    other => others.push(other),
                }
            }
        }
        (asked, others)
    }

    fn open_lane_menu(&mut self) {
        self.settle();
        self.harness.get_by_label(LANE_TITLE).click_secondary();
        self.harness.run_steps(2);
    }
}

fn lanes_commands(actions: &[Action]) -> Vec<&LanesCommand> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Feature(FeatureAction {
                feature: FeatureId::Lanes,
                request: FeatureRequest::Command(Command::Lanes(command)),
                ..
            }) => Some(command),
            _ => None,
        })
        .collect()
}

// Given: thr-4, a lane its lead thr-1 manages
// When: its row's menu opens, the lane is read, and Detach lane is chosen, and the core
//       refuses it
// Then: the menu says it reads the lane, then offers the lane's operations as the embedded
//       workbench does, and the detach names the revision the core listed; its refusal is said
//       under the projects, where it was chosen
#[test]
fn a_lane_threads_menu_reads_its_lane_and_detaches_it_at_the_listed_revision() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.open_lane_menu();
    screen.harness.get_by_label("Reading lane…");
    let (asked, _) = screen.settle_lanes();
    assert!(asked.iter().any(|query| matches!(query,
        Query::Lanes(LanesQuery::ListLanes(list)) if list.lead_thread.as_str() == "thr-1")));
    for label in [
        "Report milestones (attached)",
        "Report completion only (summary)",
        "Detach lane",
        "Close by human decision…",
        "Discard…",
        "Rename…",
        "Copy thread ID",
    ] {
        screen.harness.get_by_label(label);
    }
    assert!(screen.harness.query_by_label("Reading lane…").is_none());

    screen.harness.get_by_label("Detach lane").click();
    let (_, others) = screen.settle_lanes();
    let [LanesCommand::ManageLane(manage)] = lanes_commands(&others)[..] else {
        panic!("one lane command was expected: {}", others.len());
    };
    assert_eq!(manage.request.lane_id.as_str(), "lane-4");
    assert_eq!(manage.request.expected_management_revision, Revision(5));
    assert!(matches!(
        manage.request.action,
        workbench_protocol::lane_management::LaneManagementAction::Detach
    ));

    let refusal = crate::core_client::features::FeatureError::new("the lane changed")
        .with_code(workbench_protocol::errors::ErrorCode::StaleRevision);
    screen.shared.lock().unwrap().features.reply(
        FeatureId::Lanes,
        command_tag(&others),
        FeatureReply::Command(Err(refusal)),
    );
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Detach lane: the lane changed");
}

// Given: thr-2, a thread that is no lane
// When: its row's menu opens
// Then: it offers no lane item and reads nothing of the lanes
#[test]
fn a_thread_that_is_no_lane_has_no_lane_items() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen
        .harness
        .get_by_label("Refactor storage writer")
        .click_secondary();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Rename…");
    let (asked, _) = screen.settle_lanes();
    assert!(asked.is_empty());
    assert!(screen.harness.query_by_label("Reading lane…").is_none());
    assert!(screen.harness.query_by_label("Detach lane").is_none());
}

// Given: thr-4's menu, its lane read
// When: Close by human decision… is chosen
// Then: the Lanes tab comes to the front and draws the confirmation, which reads the lane's
//       preview from the core
#[test]
fn a_close_chosen_from_a_lane_threads_menu_opens_the_confirmation_in_the_lanes_tab() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.open_lane_menu();
    screen.settle_lanes();
    screen
        .harness
        .get_by_label("Close by human decision…")
        .click();
    let (asked, others) = screen.settle_lanes();
    assert!(
        asked.iter().any(|query| matches!(query,
        Query::Lanes(LanesQuery::GetLaneClosePreview { lane_id }) if lane_id.as_str() == "lane-4"))
    );
    assert!(
        lanes_commands(&others).is_empty(),
        "opening records nothing"
    );
    screen.harness.get_by_label("Close lane (unverified)");
    screen.harness.get_by_label("Auth regression tests");
    screen
        .harness
        .get_by_label(crate::panes::lane_widgets::NO_READY_WARNING);
    screen
        .harness
        .get_by_label(crate::panes::lane_widgets::RUNNING_WARNING);
    screen
        .harness
        .get_by_label("1 question from this lane is still unanswered. It stays open.");
    screen.capture("close-dialog");
    let dock = &screen.harness.state().dock;
    let path = dock
        .find_tab(&CoreTab::Feature(FeatureId::Lanes))
        .expect("the Lanes tab");
    let leaf = dock.leaf(path.node_path()).expect("a leaf");
    assert_eq!(
        leaf.tabs[leaf.active.0],
        CoreTab::Feature(FeatureId::Lanes),
        "the Lanes tab is in front"
    );
}

// Given: thr-4, whose lead is stopped with two notices held
// When: thr-4 is selected, and Continue lead is pressed on its card
// Then: the card under the header says the notices are held and, as a stopped lead is
//       continued rather than resumed, offers Continue lead but no Resume notifications;
//       continuing sends the lead's Continue
#[test]
fn a_lane_threads_conversation_shows_its_card_and_continues_a_stopped_lead() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.harness.get_by_label(LANE_TITLE).click();
    screen.settle_lanes();
    screen.harness.get_by_label("lane · attached");
    screen
        .harness
        .get_by_label("The lead was stopped: 2 notice(s) are held, not lost, and it is not woken.");
    screen.harness.get_by_label("working");
    screen
        .harness
        .get_by_label("Report #3: Redirect tests written; running the suite");
    assert!(
        screen
            .harness
            .query_by_label("Resume notifications")
            .is_none()
    );
    screen.capture("conversation");

    screen.harness.get_by_label("Continue lead").click();
    let (_, others) = screen.settle_lanes();
    let continued = others.iter().find_map(|action| match action {
        Action::Command {
            command: Command::Conversation(ConversationCommand::ContinueChat(chat)),
            ..
        } => Some(chat),
        _ => None,
    });
    let chat = continued.expect("the lead is continued");
    assert_eq!(chat.thread.as_str(), "thr-1");
    assert_eq!(chat.project.as_str(), "proj-1");
}

fn command_tag(actions: &[Action]) -> u64 {
    actions
        .iter()
        .find_map(|action| match action {
            Action::Feature(FeatureAction {
                feature: FeatureId::Lanes,
                tag,
                request: FeatureRequest::Command(_),
            }) => Some(*tag),
            _ => None,
        })
        .expect("a lanes command")
}

fn is_close_dialog_open(screen: &Screen, title: &str) -> bool {
    screen
        .harness
        .query_by_role_and_label(egui::accesskit::Role::Label, title)
        .is_some()
}

// Given: thr-4's menu, its lane read
// When: Discard… is chosen and cancelled, then chosen again from the Lanes tab it brought
//       forward, a reason typed and confirmed
// Then: cancelling sends nothing; the discard is sent as a discard naming the revisions the
//       preview read and the reason, and the core's answer is said in the legacy words
#[test]
fn a_discard_from_a_lane_threads_menu_is_cancelled_or_sent_with_its_reason() {
    use egui::accesskit::Role;
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.open_lane_menu();
    screen.settle_lanes();
    screen.harness.get_by_label("Discard…").click();
    screen.settle_lanes();
    assert!(is_close_dialog_open(&screen, "Discard lane"));
    screen
        .harness
        .get_by_label(crate::panes::lane_widgets::MEANING);
    // No claim of completion: the Ready warning belongs to closing as done.
    assert!(
        screen
            .harness
            .query_by_label(crate::panes::lane_widgets::NO_READY_WARNING)
            .is_none()
    );
    screen
        .harness
        .get_by_role_and_label(Role::Button, "Cancel")
        .click();
    let (_, others) = screen.settle_lanes();
    assert!(
        lanes_commands(&others).is_empty(),
        "a cancel records nothing"
    );
    assert!(!is_close_dialog_open(&screen, "Discard lane"));

    // The Lanes tab stays in front: its row offers the same Discard….
    screen.harness.get_by_label("Discard…").click();
    screen.settle_lanes();
    assert!(
        screen
            .harness
            .get_by_role_and_label(Role::Button, "Discard lane")
            .accesskit_node()
            .is_disabled(),
        "a reason is required first"
    );
    screen
        .harness
        .get_by_role_and_label(Role::TextInput, crate::panes::lane_widgets::NOTE_LABEL)
        .focus();
    screen.harness.run_steps(2);
    screen
        .harness
        .get_by_role_and_label(Role::TextInput, crate::panes::lane_widgets::NOTE_LABEL)
        .type_text("superseded by the session work");
    screen.harness.run_steps(2);
    screen
        .harness
        .get_by_role_and_label(Role::Button, "Discard lane")
        .click();
    let (_, others) = screen.settle_lanes();
    let [LanesCommand::CloseLane(close)] = lanes_commands(&others)[..] else {
        panic!("one close was expected: {}", others.len());
    };
    assert_eq!(close.lane_id.as_str(), "lane-4");
    assert_eq!(
        close.kind,
        workbench_protocol::lane_management::HumanLaneCloseKind::Discard
    );
    assert_eq!(close.expected_management_revision, Revision(5));
    assert_eq!(close.expected_request_revision, Revision(6));
    assert_eq!(close.note, "superseded by the session work");

    let reply = serde_json::from_value(json!({"disposition": "accepted", "result": {
        "type": "lanes", "payload": {"type": "closed", "payload": {"lane_id": "lane-4",
        "close_id": "close-1", "kind": "discarded", "management_revision": "6",
        "request_revision": "7", "already_closed": false}}}}))
    .expect("a close reply");
    let tag = command_tag(&others);
    screen.shared.lock().unwrap().features.reply(
        FeatureId::Lanes,
        tag,
        FeatureReply::Command(Ok(reply)),
    );
    screen.settle_lanes();
    screen
        .harness
        .get_by_label("Lane ended: discarded. No run was stopped and nothing was deleted.");
    assert!(!is_close_dialog_open(&screen, "Discard lane"));
}

// Given: thr-1, the lead of lane-4, selected
// When: the Lanes tab is brought forward, the lane is opened, and its policy is changed
// Then: the tab lists the lane in English with its badges and the operations the embedded
//       workbench offers, opens its detail card, and the policy change names the listed
//       revision
#[test]
fn the_lanes_tab_lists_a_leads_lanes_in_english_and_opens_one() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    shell::focus(
        &mut screen.harness.state_mut().dock,
        CoreTab::Feature(FeatureId::Lanes),
    );
    screen.settle_lanes();
    for label in [
        "Cover the redirect with tests",
        "lane · attached",
        "started",
        "working",
        "Report completion only (summary)",
        "Detach lane",
        "Close by human decision…",
        "Discard…",
    ] {
        screen.harness.get_by_label(label);
    }
    assert!(
        screen
            .harness
            .query_by_label("This thread manages no lanes.")
            .is_none()
    );

    screen
        .harness
        .get_by_label("Cover the redirect with tests")
        .click();
    screen.settle_lanes();
    screen.harness.get_by_label("Lane lane-4");
    screen
        .harness
        .get_by_label("attached · management revision 5 · brief revision 2 · generation 1");
    screen.harness.get_by_label("Open lane");
    screen.harness.get_by_label("Resume notifications");
    // The lead's own conversation already continues it: the tab offers no Continue lead.
    assert!(screen.harness.query_by_label("Continue lead").is_none());
    screen.capture("tab");

    screen
        .harness
        .get_by_label("Report completion only (summary)")
        .click();
    let (_, others) = screen.settle_lanes();
    let [LanesCommand::ManageLane(manage)] = lanes_commands(&others)[..] else {
        panic!("one lane command was expected: {}", others.len());
    };
    assert_eq!(manage.request.expected_management_revision, Revision(5));
    assert!(matches!(
        manage.request.action,
        workbench_protocol::lane_management::LaneManagementAction::SetPolicy {
            policy: workbench_protocol::lane_management::ManagedLanePolicy::Summary
        }
    ));
}

// Given: thr-4, whose lane wants attention for a question and a paused wake, its lead
//        running and its latest Report blocked on a person
// When: thr-4 is selected
// Then: the card names each attention item as the Notifications section words it, offers
//       Resume notifications for the paused wake but no Continue lead, and shows the Report
#[test]
fn a_lane_card_names_what_wants_attention_and_offers_the_operation_that_clears_it() {
    let mut summary = lane_summary();
    summary["latest_report_status"] = json!("blocked");
    let detail = json!({
    "summary": summary,
    "latest_report": {"seq": "4", "status": "blocked",
        "summary": "Which fixture user should the tests log in as?", "needs_human": true,
        "management_revision": "5", "brief_revision": "2", "created_at": "1"},
    "delivery": {"held": false, "pending_count": 0},
    "attention": [
        {"tag": "question_pending", "summary": "Which fixture user?", "revision": "1",
            "since": "1", "available_actions": ["answer", "route_to_user"]},
        {"tag": "wake_suppressed", "summary": "3 notices in a row woke no progress",
            "revision": "1", "since": "1", "available_actions": ["resume"]},
    ]});
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.harness.get_by_label(LANE_TITLE).click();
    screen.settle_lanes_with(&detail);
    screen.harness.get_by_label("blocked");
    screen.harness.get_by_label("Question: Which fixture user?");
    screen
        .harness
        .get_by_label("Wake paused: 3 notices in a row woke no progress");
    screen
        .harness
        .get_by_label("Report #4: Which fixture user should the tests log in as? (needs a person)");
    assert!(
        screen
            .harness
            .query_by_label_contains("The lead was stopped")
            .is_none()
    );
    assert!(screen.harness.query_by_label("Continue lead").is_none());

    screen.harness.get_by_label("Resume notifications").click();
    let (_, others) = screen.settle_lanes_with(&detail);
    let [LanesCommand::ResumeLaneDeliveries(resume)] = lanes_commands(&others)[..] else {
        panic!("one resume was expected: {}", others.len());
    };
    assert_eq!(resume.lane_id.as_str(), "lane-4");
    assert_eq!(resume.expected_management_revision, Revision(5));
    assert_eq!(resume.lead_thread_id.as_str(), "thr-1");
    assert_eq!(resume.expected_lead_root_run_id.as_str(), "run-1");
}
