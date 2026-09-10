use egui_kittest::{Harness, kittest::Queryable};
use runtime::{
    RunId,
    team::{TaskSpec, TeamBoard},
};

#[test]
fn team_pane_shows_claim_owner_and_ready_tasks() {
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
    harness.get_by_label("Team");
    harness.get_by_label("Ready");
    harness.get_by_label("Claimed");
    harness.get_by_label("worker-2");
    if let Ok(path) = std::env::var("EVORCH_TEAM_CAPTURE") {
        harness.render().unwrap().save(path).unwrap();
    }
}
