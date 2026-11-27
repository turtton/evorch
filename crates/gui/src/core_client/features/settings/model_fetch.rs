//! Fetch models: the panel above the models table that lists what the provider offers, and the
//! state of the fetch behind it. A fetch asks the core, by provider name, for the models of the
//! provider as it is saved; nothing is changed by it. The user picks ids and adds them to the
//! form, and they are saved with the provider like any other edit.
use super::*;
use crate::theme::text::{medium, muted};
use crate::theme::widgets::{accessible, spinner};
use egui::{Align, Layout};
use std::collections::BTreeSet;
use workbench_protocol::namespaces::settings::{FetchProviderModels, ProviderModelCandidates};

pub(super) enum FetchPhase {
    /// Asked; the core is waiting for the provider, or the result is being read.
    Running,
    /// This kind of provider has no model list: ids are typed by hand.
    Unsupported,
    Failed(String),
    Found {
        found: ProviderModelCandidates,
        picked: BTreeSet<String>,
        filter: String,
    },
}

pub(super) struct Fetch {
    pub provider: String,
    pub operation: OperationId,
    pub phase: FetchPhase,
}

impl SettingsFeature {
    pub(super) fn start_fetch(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        snapshot: &SettingsSnapshot,
        provider: String,
    ) {
        if matches!(
            self.fetch.as_ref().map(|fetch| &fetch.phase),
            Some(FetchPhase::Running)
        ) {
            return;
        }
        let operation_id = new_operation();
        self.fetch = Some(Fetch {
            provider: provider.clone(),
            operation: operation_id.clone(),
            phase: FetchPhase::Running,
        });
        self.send(
            ctx,
            SettingsCommand::FetchProviderModels(FetchProviderModels {
                operation_id,
                expected_revision: snapshot.revision.clone(),
                provider,
            }),
        );
    }

    fn fetch_of(&mut self, id: &OperationId) -> Option<&mut Fetch> {
        self.fetch.as_mut().filter(|fetch| &fetch.operation == id)
    }

    /// The operation of the fetch `id` ended: its result is read, or the reason is shown.
    pub(super) fn fetch_ended(
        &mut self,
        id: &OperationId,
        state: OperationState,
        message: Option<String>,
    ) {
        let ready = matches!(state, OperationState::Ready);
        let phase = match state {
            OperationState::Pending => return,
            OperationState::Ready => None,
            OperationState::Failed {
                code: ErrorCode::UnsupportedCapability,
            } => Some(FetchPhase::Unsupported),
            OperationState::Failed {
                code: ErrorCode::StaleRevision,
            } => Some(FetchPhase::Failed(
                "The settings changed while fetching. Fetch again.".into(),
            )),
            OperationState::Failed { .. } => {
                Some(FetchPhase::Failed(message.unwrap_or_else(|| {
                    "The core could not fetch the models.".into()
                })))
            }
            OperationState::Cancelled { .. } => Some(FetchPhase::Failed(
                "The fetch did not run (the core is restarting).".into(),
            )),
        };
        let Some(fetch) = self.fetch_of(id) else {
            return;
        };
        if let Some(phase) = phase {
            fetch.phase = phase;
        }
        if ready {
            self.read_models.push(id.clone());
        }
    }

    /// The reply to the read of what fetch `id` found.
    pub(super) fn fetch_read(
        &mut self,
        id: &OperationId,
        found: Result<ProviderModelCandidates, String>,
    ) {
        let Some(fetch) = self.fetch_of(id) else {
            return;
        };
        fetch.phase = match found {
            Ok(found) => FetchPhase::Found {
                found,
                picked: BTreeSet::new(),
                filter: String::new(),
            },
            Err(reason) => FetchPhase::Failed(reason),
        };
    }

    /// The fetch of operation `id` has no result to show (it was lost, or never reached the core).
    pub(super) fn fetch_lost(&mut self, id: &OperationId, reason: &str) {
        if let Some(fetch) = self.fetch_of(id) {
            fetch.phase = FetchPhase::Failed(reason.to_owned());
        }
    }

    /// What the models header says on the right: the fetch button and how the last one went.
    pub(super) fn fetch_header(
        &self,
        ui: &mut egui::Ui,
        name: Option<&str>,
        busy: bool,
    ) -> Option<ProviderAction> {
        let mut action = None;
        let running = matches!(
            self.fetch.as_ref().map(|fetch| &fetch.phase),
            Some(FetchPhase::Running)
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let button = ui.add_enabled(
                name.is_some() && !busy && !running,
                egui::Button::new(icons::with_icon(icons::CLOUD_ARROW_DOWN, "Fetch models")),
            );
            accessible(&button, "Fetch models");
            let button = if name.is_none() {
                button.on_disabled_hover_text(
                    "Save the provider first: models are fetched with its saved settings.",
                )
            } else {
                button
            };
            if button.clicked()
                && let Some(name) = name
            {
                action = Some(ProviderAction::FetchModels(name.to_owned()));
            }
            if let Some(Fetch {
                provider,
                phase: FetchPhase::Found { found, .. },
                ..
            }) = &self.fetch
                && Some(provider.as_str()) == name
            {
                ui.label(muted(format!(
                    "Fetched \u{b7} {} available",
                    found.models.len()
                )));
            }
        });
        action
    }

    /// The panel of what the fetch of `provider` found, over the models table. With a form the
    /// checked ids can be added to it; without one (a kind the form cannot write back) it only
    /// lists them, and `has` says which the provider already has.
    pub(super) fn fetch_panel(
        &mut self,
        ui: &mut egui::Ui,
        provider: Option<&str>,
        form: Option<&mut ProviderForm>,
        has: &dyn Fn(&str) -> bool,
    ) {
        let Some(fetch) = self
            .fetch
            .as_mut()
            .filter(|fetch| Some(fetch.provider.as_str()) == provider)
        else {
            return;
        };
        let mut close = false;
        egui::Frame::new()
            .fill(palette().UNREAD_SURFACE)
            .corner_radius(egui::CornerRadius::same(R_MD))
            .inner_margin(egui::Margin::same(SP_2 as i8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                match &mut fetch.phase {
                    FetchPhase::Running => {
                        ui.horizontal(|ui| {
                            spinner(ui);
                            ui.label("Fetching models\u{2026}");
                        });
                    }
                    FetchPhase::Unsupported => {
                        ui.add(
                            egui::Label::new(
                                "This kind of provider does not list its models. Add model IDs by hand below.",
                            )
                            .wrap(),
                        );
                        close = ui.button("Close").clicked();
                    }
                    FetchPhase::Failed(reason) => {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!(
                                    "Fetching models failed: {reason} You can still add model IDs by hand below."
                                ))
                                .color(palette().WARNING_FG),
                            )
                            .wrap(),
                        );
                        close = ui.button("Close").clicked();
                    }
                    FetchPhase::Found {
                        found,
                        picked,
                        filter,
                    } => close = found_panel(ui, form, has, found, picked, filter),
                }
            });
        if close {
            self.fetch = None;
        }
    }
}

/// The list of what was found with a filter, a check per id and the button that adds the checked
/// ones to the form. Returns whether to close the panel.
fn found_panel(
    ui: &mut egui::Ui,
    form: Option<&mut ProviderForm>,
    has: &dyn Fn(&str) -> bool,
    found: &ProviderModelCandidates,
    picked: &mut BTreeSet<String>,
    filter: &mut String,
) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(medium(format!("{} models offered", found.models.len())));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            close = ui.button("Close").clicked();
            let edit = ui.add(
                egui::TextEdit::singleline(filter)
                    .hint_text("Filter")
                    .desired_width(140.0)
                    .background_color(palette().INPUT),
            );
            accessible(&edit, "Filter fetched models");
        });
    });
    if found.truncated {
        ui.label(muted(
            "The provider offers more models than are listed here; add the others by ID.",
        ));
    }
    let needle = filter.trim().to_lowercase();
    egui::ScrollArea::vertical()
        .id_salt("fetched-models")
        .max_height(180.0)
        .show(ui, |ui| {
            for id in found
                .models
                .iter()
                .filter(|id| needle.is_empty() || id.to_lowercase().contains(&needle))
            {
                let added = match form.as_deref() {
                    Some(form) => form.models.iter().any(|model| &model.id == id),
                    None => has(id),
                };
                ui.horizontal(|ui| {
                    let Some(_) = form else {
                        ui.label(egui::RichText::new(id).monospace());
                        if added {
                            ui.label(muted("Added"));
                        }
                        return;
                    };
                    let mut checked = picked.contains(id);
                    let check = ui.add_enabled(
                        !added,
                        egui::Checkbox::new(&mut checked, egui::RichText::new(id).monospace()),
                    );
                    if check.changed() {
                        if checked {
                            picked.insert(id.clone());
                        } else {
                            picked.remove(id);
                        }
                    }
                    if added {
                        ui.label(muted("Added"));
                    }
                });
            }
        });
    let Some(form) = form else {
        ui.label(muted(
            "This type is edited in the Config file: add the model IDs there.",
        ));
        return close;
    };
    let count = picked.len();
    let add = ui.add_enabled(
        count > 0,
        egui::Button::new(format!("Add selected ({count})")),
    );
    if add.clicked() {
        for id in std::mem::take(picked) {
            form.add_model(&id);
        }
    }
    close
}
