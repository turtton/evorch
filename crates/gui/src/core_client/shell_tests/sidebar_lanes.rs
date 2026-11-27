//! The sidebar's lane list of a thread: collapsed by default, newest first with the closed
//! lanes after the others, and limited to a page with "See more".
use super::*;
use workbench_protocol::namespaces::LanesQuery;

/// Lanes thr-1 has besides thr-4, "Lane 1" to "Lane 7", each newer than the one before and
/// all newer than thr-4. Lanes 4 to 7 are closed.
const NEW_LANES: [(&str, u64, bool); 7] = [
    ("Lane 1", 1_793_800_001_000, false),
    ("Lane 2", 1_793_800_002_000, false),
    ("Lane 3", 1_793_800_003_000, false),
    ("Lane 4", 1_793_800_004_000, true),
    ("Lane 5", 1_793_800_005_000, true),
    ("Lane 6", 1_793_800_006_000, true),
    ("Lane 7", 1_793_800_007_000, true),
];

fn thread_id(title: &str) -> String {
    format!("thr-{}", title.to_lowercase().replace(' ', "-"))
}

fn lane_row(title: &str) -> String {
    format!("↳ {title}")
}

/// The window with `NEW_LANES` under thr-1, whose lane list is open or collapsed as asked.
fn lanes_screen(open: bool) -> Screen {
    screen_from(
        egui::vec2(1440.0, 900.0),
        super::super::local::LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            None,
        ),
        |shared| {
            for (title, updated, _) in NEW_LANES {
                let id = thread_id(title);
                let thread: ThreadSummary = view(json!({
                    "id": id, "project": "proj-1", "title": title, "state": "idle",
                    "lead_thread": "thr-1", "revision": "3", "updated_at": updated.to_string(),
                }));
                shared.cache.threads.insert(id, thread);
            }
            if let Some(workspace) = shared.features.workspace_mut() {
                let opened = open.then(|| "thr-1".to_owned());
                workspace.restore_opened_lanes(opened);
            }
        },
    )
}

fn lane_summary(lane: &str, thread: &str, updated: u64, close: bool) -> serde_json::Value {
    let mut summary = json!({"lane_id": lane, "thread_id": thread, "lead_thread_id": "thr-1",
        "policy": "attached", "management_revision": "5", "brief_revision": "2",
        "objective_preview": "An objective", "generation": "1", "start": "started",
        "last_activity_at": updated.to_string(), "revision": "1"});
    if close {
        summary["current_close"] =
            json!({"close_id": format!("close-{lane}"), "kind": "lead_closed"});
    }
    summary
}

/// What `ListLanes` of thr-1 answers: thr-4's lane, open, and `NEW_LANES`.
fn listed_lanes() -> Vec<serde_json::Value> {
    let mut items = vec![lane_summary("lane-4", "thr-4", 1_793_799_110_000, false)];
    for (title, updated, closed) in NEW_LANES {
        let id = thread_id(title);
        items.push(lane_summary(&format!("lane-{id}"), &id, updated, closed));
    }
    items
}

impl Screen {
    /// Settles, answering the lane lookups the rows ask with `listed_lanes`; returns how many
    /// lanes queries were asked.
    fn settle_with_listed_lanes(&mut self) -> usize {
        let mut asked = 0;
        self.settle();
        loop {
            let held = HELD_LANE_QUERIES.take();
            if held.is_empty() {
                return asked;
            }
            for (tag, query) in held {
                let Query::Lanes(LanesQuery::ListLanes(list)) = query else {
                    panic!("only the lane list is asked for");
                };
                asked += 1;
                let items = if list.lead_thread.as_str() == "thr-1" {
                    listed_lanes()
                } else {
                    Vec::new()
                };
                let reply = view(json!({"type": "lanes", "payload": {"type": "lanes",
                    "payload": {"items": items, "snapshot_revision": "1"}}}));
                let reply = FeatureReply::Query(Ok(reply));
                self.shared
                    .lock()
                    .unwrap()
                    .features
                    .reply(FeatureId::Lanes, tag, reply);
            }
            self.settle();
        }
    }

    fn lane_titles(&self) -> Vec<String> {
        let mut shown: Vec<(f32, String)> = ["Auth regression tests"]
            .into_iter()
            .chain(NEW_LANES.iter().map(|(title, ..)| *title))
            .filter_map(|title| {
                let label = lane_row(title);
                let row = self.harness.query_by_label(&label)?;
                Some((row.rect().min.y, title.to_owned()))
            })
            .collect();
        shown.sort_by(|left, right| left.0.total_cmp(&right.0));
        shown.into_iter().map(|(_, title)| title).collect()
    }
}

// Given: thr-1 with eight lanes and no lane list the person opened
// When: the sidebar is drawn
// Then: no lane row shows and none is looked up, thr-1's row offers "Show lanes (8)"; pressing
//       it lists the lanes
#[test]
fn a_threads_lanes_are_collapsed_by_default() {
    let mut screen = lanes_screen(false);
    assert_eq!(screen.settle_with_listed_lanes(), 0);
    assert!(screen.lane_titles().is_empty());
    screen.harness.get_by_label("Show lanes (8)");
    // The fork under it stays, the lanes are not what a fold hides.
    screen
        .harness
        .get_by_label("↳ Try a session-scoped redirect");
    screen.capture("lanes-collapsed");

    screen.harness.get_by_label("Show lanes (8)").click();
    screen.harness.run_steps(2);
    screen.settle_with_listed_lanes();
    assert!(!screen.lane_titles().is_empty());
    screen.harness.get_by_label("Hide lanes");
}

// Given: thr-1 with four open lanes (thr-4, Lane 1 to 3) and four closed ones (Lane 4 to 7),
//        every closed lane newer than every open one, and its lane list open
// When: the lanes are read, then "See more" is pressed
// Then: the open lanes are listed newest first and the closed ones after them, newest first;
//       five rows show, the closed ones going first, with "See more (3)"; "See more" shows all
#[test]
fn lanes_run_newest_first_with_the_closed_ones_last_and_the_oldest_left_out() {
    let mut screen = lanes_screen(true);
    screen.settle_with_listed_lanes();
    assert_eq!(
        screen.lane_titles(),
        [
            "Lane 3",
            "Lane 2",
            "Lane 1",
            "Auth regression tests",
            "Lane 7"
        ]
    );
    screen.harness.get_by_label("See more (3)");
    screen.capture("lanes-open");

    screen.harness.get_by_label("See more (3)").click();
    screen.harness.run_steps(2);
    screen.settle_with_listed_lanes();
    assert_eq!(
        screen.lane_titles(),
        [
            "Lane 3",
            "Lane 2",
            "Lane 1",
            "Auth regression tests",
            "Lane 7",
            "Lane 6",
            "Lane 5",
            "Lane 4"
        ]
    );
    assert!(
        screen
            .harness
            .query_by_label_contains("See more (")
            .is_none()
    );
    assert_eq!(
        screen
            .harness
            .query_all_by_label_contains("Lane close:")
            .count(),
        4
    );
}

/// Adds a lane thread `id` titled `title` that hangs under `lead`.
fn add_lane(shared: &mut Shared, id: &str, title: &str, lead: &str, updated: u64) {
    let thread: ThreadSummary = view(json!({
        "id": id, "project": "proj-1", "title": title, "state": "idle",
        "lead_thread": lead, "revision": "3", "updated_at": updated.to_string(),
    }));
    shared.cache.threads.insert(id.into(), thread);
}

fn screen_patched(patch: impl FnOnce(&mut Shared)) -> Screen {
    screen_from(
        egui::vec2(1440.0, 900.0),
        super::super::local::LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            None,
        ),
        patch,
    )
}

// Given: thr-7, whose lane list is open and whose children are folded, with lane "Lane 1" and,
//        under that lane, "Lane 2" that waits for an approval; Lane 2's list is collapsed
// When: the sidebar is drawn
// Then: neither lane shows, and thr-7's row still says an approval waits below it, though the
//       approval is on a lane the limit left out under a lane that is itself folded away
#[test]
fn a_folded_thread_still_shows_what_a_limited_lane_below_its_child_asks() {
    let mut screen = screen_patched(|shared| {
        add_lane(shared, "thr-lane-1", "Lane 1", "thr-7", 1_793_800_001_000);
        add_lane(
            shared,
            "thr-lane-2",
            "Lane 2",
            "thr-lane-1",
            1_793_800_002_000,
        );
        let approval: ApprovalView = view(json!({
            "id": "ap-9", "core_epoch": "7", "revision": "2", "thread": "thr-lane-2",
            "run": "run-9", "tool_name": "shell", "summary": "cargo test", "summary_truncated": false,
            "input_bytes": "10", "state": "pending",
        }));
        shared.cache.approvals.insert("ap-9".into(), approval);
        let workspace = shared.features.workspace_mut().unwrap();
        workspace.restore_opened_lanes(["thr-7".to_owned()]);
        workspace.restore_folded_threads(["thr-7".to_owned()]);
    });
    screen.settle_with_listed_lanes();
    assert!(screen.harness.query_by_label("↳ Lane 1").is_none());
    assert!(screen.harness.query_by_label("↳ Lane 2").is_none());
    screen.harness.get_by_label(&format!(
        "Thread attention: {}",
        crate::panes::sidebar_widgets::APPROVAL_HIDDEN_NOTE
    ));
}

// Given: the archived thr-5 with seven lanes and the lane list the person left collapsed
// When: the Archived section is opened, "Show lanes" is pressed, and "See more" after it
// Then: the section shows no lane at first; then the five newest lanes with "See more (2)",
//       then all seven; the Archived section follows the same lane list as the live threads
#[test]
fn the_archived_section_collapses_and_limits_its_lanes_too() {
    let mut screen = screen_patched(|shared| {
        for number in 1..=7 {
            let id = format!("thr-alane-{number}");
            let title = format!("Archived lane {number}");
            add_lane(
                shared,
                &id,
                &title,
                "thr-5",
                1_793_700_000_000 + number * 1000,
            );
        }
        shared
            .features
            .workspace_mut()
            .unwrap()
            .restore_opened_lanes(Vec::new());
    });
    screen.settle_with_listed_lanes();
    screen.harness.get_by_label("Archived (8)").click();
    screen.harness.run_steps(2);
    screen.settle_with_listed_lanes();
    let shown = |screen: &Screen| -> Vec<u64> {
        let mut rows: Vec<(f32, u64)> = (1..=7)
            .filter_map(|number| {
                let label = format!("↳ Archived lane {number}");
                let row = screen.harness.query_by_label(&label)?;
                Some((row.rect().min.y, number))
            })
            .collect();
        rows.sort_by(|left, right| left.0.total_cmp(&right.0));
        rows.into_iter().map(|(_, number)| number).collect()
    };
    assert!(shown(&screen).is_empty());
    screen.harness.get_by_label("Show lanes (7)").click();
    screen.harness.run_steps(2);
    screen.settle_with_listed_lanes();
    assert_eq!(shown(&screen), [7, 6, 5, 4, 3]);
    screen.harness.get_by_label("See more (2)").click();
    screen.harness.run_steps(2);
    screen.settle_with_listed_lanes();
    assert_eq!(shown(&screen), [7, 6, 5, 4, 3, 2, 1]);
    screen.capture("archived-lanes");
}
