use egui::{Id, Label, Sense, Ui, collapsing_header::CollapsingState};

pub(super) fn show(ui: &mut Ui, id: Id, text: &str, run_id: Option<&str>, call_id: Option<&str>) {
    // CollapsingHeader forces Extend, widening the shared transcript and all
    // later cards. A custom header keeps the full verdict visible and wrapped.
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, false);
    let header = ui.horizontal(|ui| {
        state.show_toggle_button(ui, egui::collapsing_header::paint_default_icon);
        ui.add(Label::new(text).wrap().sense(Sense::click()))
    });
    if header.inner.clicked() {
        state.toggle(ui);
    }
    state.show_body_indented(&header.response, ui, |ui| {
        if let Some(run_id) = run_id {
            ui.add(Label::new(format!("run_id: {run_id}")).wrap());
        }
        if let Some(call_id) = call_id {
            ui.add(Label::new(format!("call_id: {call_id}")).wrap());
        }
    });
}
