use egui_kittest::{Harness, kittest::Queryable};
use runtime::{
    RunId,
    team::{TaskSpec, TeamBoard},
};

#[test]
fn team_pane_shows_claim_owner_and_ready_tasks() {
    // Given: one ready task and one task claimed by worker-2.
    let board = TeamBoard::default();
    for id in ["ready-task", "claimed-task"] {
        board
            .enqueue(TaskSpec {
                id: id.into(),
                paths: vec![],
            })
            .unwrap();
    }
    board.claim("claimed-task", "worker-2", 0).unwrap();
    let teams = vec![(RunId::new(1), board.snapshot().unwrap())];
    let mut harness = Harness::builder()
        .with_size(egui::vec2(700.0, 400.0))
        .build_ui(move |ui| gui::panes::team::team_pane(ui, &teams));
    harness.run();

    // When: the real Team grid is rendered.
    harness.get_by_label("Team");
    let ready_task = harness.get_by_label("ready-task").rect().center().y;
    let ready_state = harness.get_by_label("Ready").rect().center().y;
    let claimed_task = harness.get_by_label("claimed-task").rect().center().y;
    let claimed_state = harness.get_by_label("Claimed").rect().center().y;
    let owner = harness.get_by_label("worker-2").rect().center().y;

    // Then: each task aligns to its state row, and the claim owner aligns to that claimed row.
    assert!((ready_task - ready_state).abs() < 1.0);
    assert!((claimed_task - claimed_state).abs() < 1.0);
    assert!((claimed_task - owner).abs() < 1.0);
    assert!((ready_task - claimed_task).abs() >= 1.0);
    if let Ok(path) = std::env::var("EVORCH_TEAM_CAPTURE") {
        harness.render().unwrap().save(path).unwrap();
    }
}
