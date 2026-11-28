//! The Providers section of Model Settings: the providers in a list on the left and the one
//! selected on the right, with its Account card (sign-in or API key), its connection and its
//! models. What is typed is the draft of the selected provider; it is saved, or dropped, with
//! the buttons of the footer, one provider at a time.
use super::*;
use crate::core_client::features::auth::{Account, AccountAction};
use crate::theme::text::{h3, section};
use crate::theme::widgets::{accessible, icon_button, primary_button};
use egui::{Align, Layout, Sense, vec2};
use workbench_protocol::namespaces::settings::CredentialRef;

pub(super) enum ProviderAction {
    /// A row of the list was chosen.
    Select(String),
    Delete(String),
    Add(FormKind),
    /// A new provider that starts as a copy of this one (it shares the same credential).
    Duplicate(ProviderView),
    /// Open the selected provider again from the newest settings.
    Reread,
    Form(FormAction),
    /// Save the form, then sign in to the provider it names once that save worked.
    SaveAndSignIn,
    OpenConfigFile,
    RefreshCredentials,
}

/// A login to start once the save of the provider it is for has worked. It is dropped by
/// anything else that happens to that save (a refusal, a lost reply, a reconnect) and by
/// choosing another provider: only a save known to have worked starts a sign-in.
pub(super) struct LoginAfterSave {
    operation: OperationId,
    name: String,
    saved: bool,
}

impl LoginAfterSave {
    pub(super) fn saved(&self) -> bool {
        self.saved
    }
}

/// Whether the form differs from what its source would open as: the provider as the snapshot
/// has it, or the defaults of a new one.
pub(super) fn form_edited(snapshot: &SettingsSnapshot, form: &ProviderForm) -> bool {
    let source = match &form.original {
        Some(name) => snapshot
            .providers
            .iter()
            .find(|provider| &provider.name == name)
            .and_then(ProviderForm::from_view),
        None => Some(ProviderForm::new(
            form.kind,
            new_provider_defaults(form.kind.new_provider(), |name| {
                snapshot.providers.iter().any(|p| p.name == name)
            }),
        )),
    };
    source.is_none_or(|source| form.edited_from(&source))
}

fn one_line(
    ui: &egui::Ui,
    text: &str,
    size: f32,
    color: egui::Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(
        text.to_owned(),
        egui::FontId::proportional(size),
        color,
        width,
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.painter().layout_job(job)
}

/// One row of the list: a status dot, the name and a line under it.
fn provider_row(
    ui: &mut egui::Ui,
    selected: bool,
    locked: bool,
    name: &str,
    line: &str,
    line_color: egui::Color32,
    dot: egui::Color32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
    let hovered = response.hovered() && !locked;
    if selected || hovered {
        let fill = if selected {
            palette().ACTIVE_ROW
        } else {
            palette().HOVER_ROW
        };
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_MD), fill);
    }
    ui.painter()
        .circle_filled(egui::pos2(rect.left() + SP_3, rect.top() + 15.0), 4.0, dot);
    let left = rect.left() + SP_3 + SP_3;
    let width = (rect.right() - left - SP_2).max(10.0);
    let title = one_line(ui, name, FONT_BODY, palette().TEXT, width);
    ui.painter()
        .galley(egui::pos2(left, rect.top() + 6.0), title, palette().TEXT);
    let sub = one_line(ui, line, FONT_SMALL, line_color, width);
    ui.painter()
        .galley(egui::pos2(left, rect.top() + 26.0), sub, line_color);
    let enabled = !locked || selected;
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, name));
    response
}

fn add_item(ui: &mut egui::Ui, kind: FormKind) -> bool {
    let button = ui.add(
        egui::Button::new(kind.menu_title())
            .right_text(crate::theme::text::muted(kind.menu_hint())),
    );
    accessible(&button, kind.menu_title());
    button.clicked()
}

fn unique_copy_name(snapshot: &SettingsSnapshot, name: &str) -> String {
    let taken = |candidate: &str| snapshot.providers.iter().any(|p| p.name == candidate);
    let mut candidate = format!("{name}-copy");
    let mut n = 2;
    while taken(&candidate) {
        candidate = format!("{name}-copy-{n}");
        n += 1;
    }
    candidate
}

impl SettingsFeature {
    /// Whether the form holds edits of a provider the snapshot has (or a copy of one): the list
    /// and the Add menu then wait for a Save or a Discard instead of dropping them.
    fn form_dirty(&self) -> bool {
        self.form
            .as_ref()
            .zip(self.snapshot.as_ref())
            .is_some_and(|(form, snapshot)| form_edited(snapshot, form))
    }

    /// Keeps the selection and the form of the selected provider in step with the snapshot: a
    /// provider is always open as a form (what is not edited follows a newer snapshot), and the
    /// first one is selected when nothing is.
    fn ensure_selection(&mut self, snapshot: &SettingsSnapshot) {
        if self.provider_saving.is_some() {
            return;
        }
        let Some(name) = self.selected.clone() else {
            if self.form.is_none()
                && let Some(first) = snapshot.providers.first()
            {
                self.selected = Some(first.name.clone());
            }
            return;
        };
        let Some(view) = snapshot.providers.iter().find(|p| p.name == name) else {
            // A save that renamed it is waiting for the snapshot that has the new name.
            if !(self.stale || self.snapshot_in_flight) {
                self.selected = None;
                self.form = None;
            }
            return;
        };
        let reopen = match &self.form {
            None => true,
            Some(form) => {
                form.original.as_deref() != Some(name.as_str())
                    || (form.based_on.as_ref() != Some(&snapshot.revision)
                        && !form_edited(snapshot, form))
            }
        };
        if reopen {
            self.form_serial += 1;
            self.form = ProviderForm::from_view(view).map(|mut form| {
                form.based_on = Some(snapshot.revision.clone());
                form.serial = self.form_serial;
                form
            });
        }
    }

    pub(super) fn providers_body(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        snapshot: &SettingsSnapshot,
        busy: bool,
        height: f32,
    ) -> Option<ProviderAction> {
        self.ensure_selection(snapshot);
        let locked = busy || self.form_dirty();
        let mut action = None;
        let total = ui.available_width();
        let list_width = (total * 0.3).clamp(170.0, 250.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                vec2(list_width, height),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(list_width);
                    action = self.provider_list(ui, snapshot, busy, locked, height);
                },
            );
            ui.separator();
            let rest = ui.available_width().max(100.0);
            ui.allocate_ui_with_layout(vec2(rest, height), Layout::top_down(Align::Min), |ui| {
                ui.set_width(rest);
                egui::ScrollArea::vertical()
                    .id_salt("provider-detail")
                    .auto_shrink([false, false])
                    .max_height(height)
                    .show(ui, |ui| {
                        action = self
                            .provider_detail(ui, ctx, snapshot, busy)
                            .or(action.take());
                    });
            });
        });
        action
    }

    fn provider_list(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &SettingsSnapshot,
        busy: bool,
        locked: bool,
        height: f32,
    ) -> Option<ProviderAction> {
        let mut action = None;
        ui.horizontal(|ui| {
            ui.label(section(format!("{} PROVIDERS", snapshot.providers.len())));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(!locked, |ui| {
                    let menu = ui.menu_button(icons::with_icon(icons::PLUS, "Add"), |ui| {
                        ui.set_min_width(280.0);
                        ui.label(section("SUBSCRIPTION"));
                        for kind in [
                            FormKind::Codex,
                            FormKind::ClaudeSubscription,
                            FormKind::Cursor,
                            FormKind::Kimi,
                        ] {
                            if add_item(ui, kind) {
                                action = Some(ProviderAction::Add(kind));
                                ui.close();
                            }
                        }
                        ui.separator();
                        ui.label(section("API"));
                        if add_item(ui, FormKind::OpenaiCompatible) {
                            action = Some(ProviderAction::Add(FormKind::OpenaiCompatible));
                            ui.close();
                        }
                    });
                    accessible(&menu.response, "Add");
                });
                ui.add_enabled_ui(!self.auth.busy(), |ui| {
                    if icon_button(ui, icons::ARROWS_CLOCKWISE, "Refresh credentials").clicked() {
                        action = Some(ProviderAction::RefreshCredentials);
                    }
                });
            });
        });
        egui::ScrollArea::vertical()
            .id_salt("provider-list")
            .auto_shrink([false, false])
            .max_height((height - 40.0).max(60.0))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if let Some(form) = self.form.as_ref().filter(|form| form.original.is_none()) {
                    let name = if form.name.trim().is_empty() {
                        "New provider"
                    } else {
                        form.name.trim()
                    };
                    provider_row(
                        ui,
                        true,
                        false,
                        name,
                        "New \u{b7} not saved",
                        palette().TEXT_MUTED,
                        palette().TEXT_MUTED,
                    );
                }
                if snapshot.providers.is_empty() && self.form.is_none() {
                    ui.label(crate::theme::text::muted("No provider profiles."));
                }
                for provider in &snapshot.providers {
                    let selected = self.selected.as_deref() == Some(provider.name.as_str())
                        && self
                            .form
                            .as_ref()
                            .is_none_or(|form| form.original.is_some());
                    let (dot, state, ok) = self.auth.state_of(&provider.name);
                    let kind = provider_kind_label(provider.kind);
                    let (line, color) = if ProviderForm::from_view(provider).is_none() {
                        (
                            format!("{kind} \u{b7} config file only"),
                            palette().TEXT_MUTED,
                        )
                    } else if ok {
                        let model = if provider.default_model.is_empty() {
                            kind.to_owned()
                        } else {
                            format!("{kind} \u{b7} {}", provider.default_model)
                        };
                        (model, palette().TEXT_MUTED)
                    } else {
                        (state, dot)
                    };
                    let response =
                        provider_row(ui, selected, locked, &provider.name, &line, color, dot);
                    if locked && !selected {
                        response.clone().on_hover_text(if busy {
                            "A save is running."
                        } else {
                            "Save or discard the changes first."
                        });
                    } else if response.clicked() && !selected {
                        action = Some(ProviderAction::Select(provider.name.clone()));
                    }
                }
            });
        action
    }

    fn provider_detail(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        snapshot: &SettingsSnapshot,
        busy: bool,
    ) -> Option<ProviderAction> {
        if let Some(form) = self.form.take() {
            return self.form_detail(ui, ctx, snapshot, busy, form);
        }
        let selected = self.selected.clone()?;
        let view = snapshot.providers.iter().find(|p| p.name == selected)?;
        self.read_only_detail(ui, ctx, view, busy)
    }

    /// The name of the provider and its icons: copy and delete.
    fn detail_header(
        &mut self,
        ui: &mut egui::Ui,
        title: &str,
        kind: &str,
        saved: Option<&ProviderView>,
        (busy, duplicable): (bool, bool),
        action: &mut Option<ProviderAction>,
    ) {
        let signing_in = saved.is_some_and(|view| self.auth.login_of(&view.name));
        ui.horizontal(|ui| {
            ui.label(h3(title));
            crate::panes::provider_widgets::provider_type_badge(ui, kind);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let Some(view) = saved else {
                    return;
                };
                // Not while it is signing in (its sign-in could no longer be cancelled from here),
                // and a copy is made of what is saved, so not over unsaved edits.
                ui.add_enabled_ui(!busy && !signing_in, |ui| {
                    if icon_button(ui, icons::TRASH, "Delete provider").clicked() {
                        self.confirm_delete = Some(view.name.clone());
                    }
                });
                ui.add_enabled_ui(!busy && duplicable, |ui| {
                    if ProviderForm::from_view(view).is_some()
                        && icon_button(ui, icons::COPY, "Duplicate provider").clicked()
                    {
                        *action = Some(ProviderAction::Duplicate(view.clone()));
                    }
                });
            });
        });
        let Some(view) = saved.filter(|view| self.confirm_delete.as_deref() == Some(&view.name))
        else {
            return;
        };
        egui::Frame::new()
            .fill(palette().WARNING_SURFACE)
            .corner_radius(egui::CornerRadius::same(R_MD))
            .inner_margin(egui::Margin::same(SP_2 as i8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(format!("Delete {}?", view.name)).color(palette().WARNING_FG));
                ui.add(
                    egui::Label::new(crate::theme::text::muted(
                        "The provider is removed from the settings. Its stored sign-in or API key is not deleted: sign out first to remove it.",
                    ))
                    .wrap(),
                );
                ui.horizontal(|ui| {
                    if ui.add_enabled(!busy, egui::Button::new("Confirm delete")).clicked() {
                        *action = Some(ProviderAction::Delete(view.name.clone()));
                    }
                    if ui.button("Keep profile").clicked() {
                        self.confirm_delete = None;
                    }
                });
            });
    }

    fn form_detail(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        snapshot: &SettingsSnapshot,
        busy: bool,
        mut form: ProviderForm,
    ) -> Option<ProviderAction> {
        let mut action = None;
        let saved = form
            .original
            .as_ref()
            .and_then(|name| snapshot.providers.iter().find(|p| &p.name == name));
        let title = match &form.original {
            Some(name) => name.clone(),
            None if form.name.trim().is_empty() => "New provider".to_owned(),
            None => form.name.trim().to_owned(),
        };
        let kind_label = saved.map_or_else(
            || form.kind.menu_title().to_owned(),
            |view| provider_kind_label(view.kind).to_owned(),
        );
        let duplicable = !form_edited(snapshot, &form);
        self.detail_header(
            ui,
            &title,
            &kind_label,
            saved,
            (busy, duplicable),
            &mut action,
        );
        if form
            .based_on
            .as_ref()
            .is_some_and(|based| based != &snapshot.revision)
        {
            ui.colored_label(
                palette().WARNING_FG,
                "The settings file changed after this form was opened; saving will be refused.",
            );
            if ui.button("Reopen with the latest settings").clicked() {
                action = Some(ProviderAction::Reread);
            }
        }
        ui.add_space(SP_2);
        ui.label(section("ACCOUNT"));
        let name = saved.map_or_else(|| form.name.trim().to_owned(), |view| view.name.clone());
        let account = Account {
            name: &name,
            login: form.kind.login(),
            saved: saved.is_some(),
            savable: form.to_input().is_ok(),
            saving: self.provider_saving.is_some(),
        };
        if let Some(AccountAction::SaveAndSignIn) = self.auth.account_card(ui, ctx, &account) {
            action = Some(ProviderAction::SaveAndSignIn);
        }
        ui.add_space(SP_2);
        ui.label(section("CONNECTION"));
        let name_locked = form
            .original
            .as_ref()
            .is_some_and(|name| self.auth.login_of(name));
        ui.add_enabled_ui(!busy, |ui| form.draw_connection(ui, name_locked));
        ui.add_space(SP_2);
        ui.label(section(format!("MODELS ({})", form.models.len())));
        ui.add_enabled_ui(!busy, |ui| form.draw_models(ui));
        if let Some(error) = &form.error {
            crate::theme::widgets::reason_label(ui, palette().ERROR_FG, error);
        }
        self.form = Some(form);
        action
    }

    /// A provider of a kind the form cannot write back: what it holds, read-only, with the way
    /// to the Config file; its Account card works as for any other.
    fn read_only_detail(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        view: &ProviderView,
        busy: bool,
    ) -> Option<ProviderAction> {
        let mut action = None;
        self.detail_header(
            ui,
            &view.name,
            provider_kind_label(view.kind),
            Some(view),
            (busy, true),
            &mut action,
        );
        egui::Frame::new()
            .fill(palette().UNREAD_SURFACE)
            .corner_radius(egui::CornerRadius::same(R_MD))
            .inner_margin(egui::Margin::same(SP_2 as i8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(
                    egui::Label::new(
                        "This type is edited in the Config file; what is shown here is read-only.",
                    )
                    .wrap(),
                );
                let open = ui.button(icons::with_icon(icons::FILE_TEXT, "Open Config file"));
                accessible(&open, "Open Config file");
                if open.clicked() {
                    action = Some(ProviderAction::OpenConfigFile);
                }
            });
        ui.add_space(SP_2);
        ui.label(section("ACCOUNT"));
        let account = Account {
            name: &view.name,
            login: None,
            saved: true,
            savable: false,
            saving: false,
        };
        self.auth.account_card(ui, ctx, &account);
        ui.add_space(SP_2);
        ui.label(section("CONNECTION"));
        ui.label(format!("Base URL: {}", view.base_url));
        let credential = match &view.credential {
            CredentialRef::Env { var } => format!("Environment variable {var}"),
            CredentialRef::Keyring { service, account } => {
                format!("Keyring \"{service}\", account {account}")
            }
        };
        ui.label(format!("Credential: {credential}"));
        ui.add_space(SP_2);
        ui.label(section(format!("MODELS ({})", view.models.len())));
        for model in &view.models {
            ui.horizontal(|ui| {
                let (icon, color) = if model.enabled {
                    (icons::CHECK_CIRCLE, palette().SUCCESS)
                } else {
                    (icons::CIRCLE, palette().TEXT_MUTED)
                };
                ui.label(egui::RichText::new(icon).color(color));
                ui.label(egui::RichText::new(&model.id).monospace());
                if model.id == view.default_model {
                    ui.label(crate::theme::text::muted("default"));
                }
            });
        }
        action
    }

    /// Save and Discard of the selected provider, drawn right to left in the footer. They stand for
    /// this provider only; nothing else of the modal is saved or dropped with them.
    pub(super) fn provider_buttons(&self, ui: &mut egui::Ui, busy: bool) -> Option<ProviderAction> {
        let form = self.form.as_ref()?;
        let dirty = form.original.is_none() || self.form_dirty();
        let mut action = None;
        ui.add_enabled_ui(!busy && dirty, |ui| {
            let save = primary_button(ui, icons::with_icon(icons::FLOPPY_DISK, "Save"));
            accessible(&save, "Save");
            if save.clicked() {
                action = Some(ProviderAction::Form(FormAction::Save));
            }
        });
        ui.add_enabled_ui(!busy && dirty, |ui| {
            let discard = ui.button(icons::with_icon(icons::ARROW_COUNTER_CLOCKWISE, "Discard"));
            accessible(&discard, "Discard");
            if discard.clicked() {
                action = Some(ProviderAction::Form(FormAction::Cancel));
            }
        });
        action
    }

    pub(super) fn apply_provider_action(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        snapshot: &SettingsSnapshot,
        action: ProviderAction,
    ) {
        match action {
            ProviderAction::Select(name) => {
                if self.provider_saving.is_none() {
                    self.leave_provider();
                    self.selected = Some(name);
                    self.ensure_selection(snapshot);
                }
            }
            ProviderAction::Delete(name) => {
                self.confirm_delete = None;
                if self.auth.login_of(&name) {
                    return;
                }
                self.auth.forget(&name);
                if self.selected.as_deref() == Some(name.as_str()) {
                    self.selected = None;
                    self.form = None;
                }
                let operation_id = new_operation();
                self.send(
                    ctx,
                    SettingsCommand::DeleteProvider(DeleteProvider {
                        operation_id,
                        expected_revision: snapshot.revision.clone(),
                        name,
                    }),
                );
            }
            ProviderAction::Add(kind) => {
                if self.provider_saving.is_none() {
                    self.leave_provider();
                    self.form_serial += 1;
                    let defaults = new_provider_defaults(kind.new_provider(), |name| {
                        snapshot.providers.iter().any(|p| p.name == name)
                    });
                    self.form = Some(ProviderForm {
                        based_on: Some(snapshot.revision.clone()),
                        serial: self.form_serial,
                        ..ProviderForm::new(kind, defaults)
                    });
                }
            }
            ProviderAction::Duplicate(view) => {
                if self.provider_saving.is_none()
                    && let Some(mut form) = ProviderForm::from_view(&view)
                {
                    self.leave_provider();
                    self.form_serial += 1;
                    form.original = None;
                    form.name = unique_copy_name(snapshot, &view.name);
                    form.based_on = Some(snapshot.revision.clone());
                    form.serial = self.form_serial;
                    self.form = Some(form);
                }
            }
            ProviderAction::Reread => {
                if self.provider_saving.is_none() {
                    match self.form.take() {
                        Some(old) if old.original.is_none() => {
                            self.form = Some(ProviderForm {
                                based_on: Some(snapshot.revision.clone()),
                                ..old
                            });
                        }
                        _ => {
                            self.form = None;
                            self.ensure_selection(snapshot);
                        }
                    }
                }
            }
            ProviderAction::Form(FormAction::Cancel) => {
                if self.provider_saving.is_none() {
                    self.login_after_save = None;
                    self.form = None;
                    self.ensure_selection(snapshot);
                }
            }
            ProviderAction::Form(FormAction::Save) => {
                self.save_form(ctx, snapshot, false);
            }
            ProviderAction::SaveAndSignIn => {
                self.save_form(ctx, snapshot, true);
            }
            ProviderAction::OpenConfigFile => {
                if self.modal_dirty() {
                    self.request_close();
                } else {
                    self.close_modal();
                    ctx.settings(SettingsIntent::ConfigFile);
                }
            }
            ProviderAction::RefreshCredentials => self.auth.refresh(ctx),
        }
    }

    /// Choosing another provider (or adding one) drops what was waiting on the one left.
    fn leave_provider(&mut self) {
        self.selected = None;
        self.form = None;
        self.confirm_delete = None;
        self.login_after_save = None;
        self.auth.cancel_confirm();
        self.auth.clear_message();
    }

    fn save_form(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        snapshot: &SettingsSnapshot,
        then_sign_in: bool,
    ) {
        if self.provider_saving.is_some() {
            return;
        }
        let Some(form) = &mut self.form else {
            return;
        };
        if let Some(original) = &form.original
            && form.name.trim() != original
            && self.auth.login_of(original)
        {
            form.error = Some("Finish or cancel the sign-in before renaming this provider.".into());
            return;
        }
        match form.to_input() {
            Ok(provider) => {
                form.error = None;
                let operation_id = new_operation();
                self.provider_saving = Some(operation_id.clone());
                self.saving_form = Some(form.serial);
                self.saving_name = Some(provider.name().to_owned());
                self.login_after_save =
                    (then_sign_in && form.kind.login().is_some()).then(|| LoginAfterSave {
                        operation: operation_id.clone(),
                        name: provider.name().to_owned(),
                        saved: false,
                    });
                let expected_revision = form
                    .based_on
                    .clone()
                    .unwrap_or_else(|| snapshot.revision.clone());
                let original_name = form.original.clone();
                self.send(
                    ctx,
                    SettingsCommand::SaveProvider(Box::new(SaveProvider {
                        operation_id,
                        expected_revision,
                        original_name,
                        provider,
                    })),
                );
            }
            Err(missing) => form.error = Some(missing),
        }
    }

    /// The save `id` ended. A provider save that worked selects the provider it wrote; the
    /// login that waited for it is allowed to start (by [`Self::start_login_after_save`], once
    /// the snapshot shows the provider). Any other end drops that login.
    pub(super) fn provider_save_ended(&mut self, id: &OperationId, worked: bool) {
        if self
            .login_after_save
            .as_ref()
            .is_some_and(|l| &l.operation == id)
        {
            if worked {
                if let Some(login) = &mut self.login_after_save {
                    login.saved = true;
                }
            } else {
                self.login_after_save = None;
            }
        }
    }

    /// A reply or an operation of `id` was lost: a sign-in that waited for it does not start.
    pub(super) fn drop_login_after(&mut self, id: &OperationId) {
        if self
            .login_after_save
            .as_ref()
            .is_some_and(|l| &l.operation == id)
        {
            self.login_after_save = None;
        }
    }

    /// Starts the sign-in that waited for a save, once the save is known to have worked, the
    /// snapshot of it is in and the provider is still the one selected.
    pub(super) fn start_login_after_save(&mut self, ctx: &mut FeatureContext<'_>) {
        let Some(login) = &self.login_after_save else {
            return;
        };
        if !login.saved {
            return;
        }
        if self.selected.as_deref() != Some(login.name.as_str()) {
            self.login_after_save = None;
            return;
        }
        let known = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.providers.iter().any(|p| p.name == login.name));
        if !known || self.stale || self.snapshot_in_flight || !self.auth.can_start_login() {
            return;
        }
        if let Some(login) = self.login_after_save.take() {
            self.auth.start_login(ctx, &login.name);
        }
    }
}
