//! The form that adds a provider or edits one, laid out as the legacy Providers tab: what the
//! user types, and the typed `ProviderInput` it becomes. The form holds no secret: a
//! credential is the name of an environment variable or a keychain entry, never the key
//! itself, so nothing to hide reaches the screen, the command or a log.
use crate::model::settings_document::{NewProvider, NewProviderDefaults};
use crate::panes::provider_widgets::{
    ADD_CLAUDE_SUBSCRIPTION, ADD_CODEX, ADD_CURSOR, ADD_KIMI, ADD_OPENAI_COMPATIBLE,
};
use crate::theme::{text::muted, tokens::palette, widgets::primary_button};
use workbench_protocol::namespaces::settings::{
    CredentialRef, ModelInput, OpenaiCompatibleKind, ProviderInput, ProviderKind, ProviderView,
    SettingsRevision, SubscriptionKind,
};

/// The kinds of provider the core's `save_provider` can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormKind {
    OpenaiCompatible,
    Kimi,
    Codex,
    ClaudeSubscription,
    Cursor,
}

const KEYRING_SERVICE: &str = "evorch";

impl FormKind {
    pub(super) const ALL: [Self; 5] = [
        Self::OpenaiCompatible,
        Self::Codex,
        Self::ClaudeSubscription,
        Self::Cursor,
        Self::Kimi,
    ];

    pub(super) const fn add_label(self) -> &'static str {
        match self {
            Self::OpenaiCompatible => ADD_OPENAI_COMPATIBLE,
            Self::Kimi => ADD_KIMI,
            Self::Codex => ADD_CODEX,
            Self::ClaudeSubscription => ADD_CLAUDE_SUBSCRIPTION,
            Self::Cursor => ADD_CURSOR,
        }
    }

    pub(super) const fn new_provider(self) -> NewProvider {
        match self {
            Self::OpenaiCompatible => NewProvider::OpenaiCompatible,
            Self::Kimi => NewProvider::Kimi,
            Self::Codex => NewProvider::Codex,
            Self::ClaudeSubscription => NewProvider::ClaudeSubscription,
            Self::Cursor => NewProvider::Cursor,
        }
    }

    /// Whether the credential is an environment variable or a keychain entry the user names
    /// (otherwise it is the keychain account that holds the login).
    const fn names_credential(self) -> bool {
        matches!(self, Self::OpenaiCompatible | Self::Kimi)
    }

    const fn has_excluded_models(self) -> bool {
        !matches!(self, Self::Codex)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ModelRow {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProviderForm {
    /// The profile being edited (a rename replaces it); `None` adds a new one.
    pub original: Option<String>,
    pub kind: FormKind,
    pub name: String,
    pub base_url: String,
    /// For the kinds that name their credential: an environment variable (`true`) or a
    /// keychain entry.
    pub env_credential: bool,
    pub env_var: String,
    pub keyring_service: String,
    /// The keychain account: the credential's for a named keychain entry, the login's for
    /// the subscription kinds.
    pub account: String,
    pub models: Vec<ModelRow>,
    pub new_model: String,
    /// Model ids to leave out, one per line (commas are read too).
    pub excluded: String,
    pub default_model: String,
    /// The revision of the settings the form was opened from; a save quotes this one, not the
    /// newest, so a change made since is refused instead of overwritten.
    pub based_on: Option<SettingsRevision>,
    /// Tells this form from the ones opened before and after it.
    pub serial: u64,
    pub error: Option<String>,
}

/// What the user did with the form this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormAction {
    None,
    Save,
    Cancel,
}

fn labelled_line(ui: &mut egui::Ui, label: &str, value: &mut String, hint: Option<&str>) {
    let width = ui.available_width();
    let label = ui.label(label);
    let mut edit = egui::TextEdit::singleline(value)
        .desired_width(width)
        .background_color(palette().INPUT);
    if let Some(hint) = hint {
        edit = edit.hint_text(hint);
    }
    ui.add(edit).labelled_by(label.id);
}

impl ProviderForm {
    pub(super) fn new(kind: FormKind, defaults: NewProviderDefaults) -> Self {
        Self {
            original: None,
            kind,
            account: defaults.name.clone(),
            name: defaults.name,
            base_url: defaults.base_url,
            env_credential: false,
            env_var: String::new(),
            keyring_service: KEYRING_SERVICE.to_owned(),
            models: defaults
                .models
                .into_iter()
                .map(|id| ModelRow { id, enabled: true })
                .collect(),
            new_model: String::new(),
            excluded: String::new(),
            default_model: defaults.default_model,
            based_on: None,
            serial: 0,
            error: None,
        }
    }

    /// Whether the user typed anything into the form: it differs from the form its source (the
    /// provider as the snapshot has it, or the defaults of a new one) would open as.
    pub(super) fn edited_from(&self, source: &Self) -> bool {
        (
            self.kind,
            &self.name,
            &self.base_url,
            self.env_credential,
            &self.env_var,
            &self.keyring_service,
            &self.account,
            &self.models,
            &self.new_model,
            &self.excluded,
            &self.default_model,
        ) != (
            source.kind,
            &source.name,
            &source.base_url,
            source.env_credential,
            &source.env_var,
            &source.keyring_service,
            &source.account,
            &source.models,
            &source.new_model,
            &source.excluded,
            &source.default_model,
        )
    }

    /// Draw the fields; [`Self::footer`] draws the buttons.
    pub(super) fn draw(&mut self, ui: &mut egui::Ui) {
        let previous_name = self.name.clone();
        labelled_line(ui, "Name", &mut self.name, None);
        // An edited provider keeps its account: following the rename would leave its stored
        // sign-in behind under the old account.
        if self.original.is_none() && self.account == previous_name {
            self.account.clone_from(&self.name);
        }
        labelled_line(ui, "Base URL", &mut self.base_url, None);
        if self.kind.names_credential() {
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.env_credential, false, "Keyring (recommended)");
                ui.radio_value(&mut self.env_credential, true, "Environment variable");
            });
            if self.env_credential {
                labelled_line(
                    ui,
                    "API key env var",
                    &mut self.env_var,
                    Some("Environment variable NAME, e.g. OPENAI_API_KEY"),
                );
            } else {
                labelled_line(ui, "Keyring service", &mut self.keyring_service, None);
                labelled_line(ui, "Keyring account", &mut self.account, None);
            }
            ui.label(muted(
                "Store the API key itself in Authentication; this form only names where it is kept.",
            ));
        } else {
            let label = if self.kind == FormKind::Codex {
                "Account"
            } else {
                "Credential account"
            };
            labelled_line(ui, label, &mut self.account, None);
            ui.label(muted("Sign in from Authentication after saving."));
        }
        self.models_editor(ui);
        if self.kind.has_excluded_models() {
            let width = ui.available_width();
            let label = ui.label("Excluded models");
            ui.add(
                egui::TextEdit::multiline(&mut self.excluded)
                    .hint_text("one per line")
                    .desired_rows(3)
                    .desired_width(width)
                    .background_color(palette().INPUT),
            )
            .labelled_by(label.id);
        }
        let label = ui.label("Default model");
        let choices: Vec<String> = self
            .models
            .iter()
            .filter(|model| model.enabled)
            .map(|model| model.id.clone())
            .collect();
        egui::ComboBox::from_id_salt("provider-default-model")
            .width(ui.available_width())
            .selected_text(&self.default_model)
            .show_ui(ui, |ui| {
                for id in choices {
                    ui.selectable_value(&mut self.default_model, id.clone(), id);
                }
            })
            .response
            .labelled_by(label.id);
        ui.add(
            egui::Label::new(muted(
                "Used when a route doesn't override the model and when re-resolving a pinned session.",
            ))
            .wrap(),
        );
    }

    /// The error and the Save / Back to list buttons, kept below the scrolled fields as
    /// legacy does. `busy` stops a second save while one runs.
    pub(super) fn footer(&self, ui: &mut egui::Ui, busy: bool) -> FormAction {
        let mut action = FormAction::None;
        if let Some(error) = &self.error {
            crate::theme::widgets::reason_label(ui, palette().ERROR_FG, error);
        }
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy, |ui| {
                if primary_button(ui, "Save").clicked() {
                    action = FormAction::Save;
                }
            });
            if ui.button("Back to list").clicked() {
                action = FormAction::Cancel;
            }
        });
        action
    }

    fn models_editor(&mut self, ui: &mut egui::Ui) {
        ui.label(crate::theme::text::badge("Models"));
        let mut remove = None;
        for (index, model) in self.models.iter_mut().enumerate() {
            ui.push_id(index, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut model.enabled, "Enabled");
                    ui.label(&model.id);
                    let button = ui.button("Remove");
                    let id = model.id.clone();
                    button.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            format!("Remove {id}"),
                        )
                    });
                    if button.clicked() {
                        remove = Some(index);
                    }
                });
            });
        }
        if let Some(index) = remove {
            self.models.remove(index);
        }
        let label = ui.label("Add model");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_model)
                    .desired_width((ui.available_width() - 60.0).max(40.0))
                    .background_color(palette().INPUT),
            )
            .labelled_by(label.id);
            if ui.button("Add").clicked() {
                let id = self.new_model.trim().to_owned();
                if !id.is_empty() && !self.models.iter().any(|model| model.id == id) {
                    self.models.push(ModelRow { id, enabled: true });
                    self.new_model.clear();
                }
            }
        });
    }

    /// The form for editing `view`, or `None` for a provider the core cannot write back from
    /// this form (an API-key provider of another kind is edited in the settings file).
    pub(super) fn from_view(view: &ProviderView) -> Option<Self> {
        let kind = match view.kind {
            ProviderKind::OpenaiCompatible => FormKind::OpenaiCompatible,
            ProviderKind::KimiSubscription => FormKind::Kimi,
            ProviderKind::OpenaiCodex => FormKind::Codex,
            ProviderKind::AnthropicSubscription => FormKind::ClaudeSubscription,
            ProviderKind::Cursor => FormKind::Cursor,
            _ => return None,
        };
        let mut form = Self {
            original: Some(view.name.clone()),
            excluded: view.excluded_models.join("\n"),
            ..Self::new(
                kind,
                NewProviderDefaults {
                    name: view.name.clone(),
                    base_url: view.base_url.clone(),
                    models: Vec::new(),
                    default_model: view.default_model.clone(),
                },
            )
        };
        form.models = view
            .models
            .iter()
            .map(|model| ModelRow {
                id: model.id.clone(),
                enabled: model.enabled,
            })
            .collect();
        match &view.credential {
            CredentialRef::Env { var } => {
                form.env_credential = true;
                form.env_var.clone_from(var);
                form.account.clear();
            }
            CredentialRef::Keyring { service, account } => {
                form.env_credential = false;
                form.keyring_service.clone_from(service);
                form.account.clone_from(account);
            }
        }
        Some(form)
    }

    fn model_inputs(&self) -> Vec<ModelInput> {
        self.models
            .iter()
            .map(|model| ModelInput {
                id: model.id.clone(),
                enabled: model.enabled,
            })
            .collect()
    }

    fn excluded_models(&self) -> Vec<String> {
        self.excluded
            .split([',', '\n'])
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The input the form stands for, or what is missing. The core validates the rest and
    /// says why it refuses.
    pub(super) fn to_input(&self) -> Result<ProviderInput, String> {
        let name = self.name.trim().to_owned();
        let base_url = self.base_url.trim().to_owned();
        if name.is_empty() {
            return Err("Enter a provider name.".into());
        }
        if base_url.is_empty() {
            return Err("Enter a base URL.".into());
        }
        let models = self.model_inputs();
        let default_model = self.default_model.trim().to_owned();
        let excluded_models = if self.kind.has_excluded_models() {
            self.excluded_models()
        } else {
            Vec::new()
        };
        let account = self.account.trim().to_owned();
        if !self.kind.names_credential() && account.is_empty() {
            return Err("Enter the credential account that holds the sign-in.".into());
        }
        Ok(match self.kind {
            FormKind::OpenaiCompatible | FormKind::Kimi => {
                let credential = if self.env_credential {
                    let var = self.env_var.trim().to_owned();
                    if var.is_empty() {
                        return Err("Enter the environment variable name.".into());
                    }
                    CredentialRef::Env { var }
                } else {
                    let service = self.keyring_service.trim().to_owned();
                    if service.is_empty() || account.is_empty() {
                        return Err("Enter the keyring service and account.".into());
                    }
                    CredentialRef::Keyring { service, account }
                };
                ProviderInput::OpenaiCompatible {
                    name,
                    kind: if self.kind == FormKind::Kimi {
                        OpenaiCompatibleKind::KimiSubscription
                    } else {
                        OpenaiCompatibleKind::OpenaiCompatible
                    },
                    base_url,
                    credential,
                    models,
                    excluded_models,
                    default_model,
                }
            }
            FormKind::Codex => ProviderInput::Codex {
                name,
                account,
                base_url,
                models,
                default_model,
            },
            FormKind::ClaudeSubscription | FormKind::Cursor => ProviderInput::Subscription {
                name,
                kind: if self.kind == FormKind::Cursor {
                    SubscriptionKind::Cursor
                } else {
                    SubscriptionKind::AnthropicSubscription
                },
                account,
                base_url,
                models,
                excluded_models,
                default_model,
            },
        })
    }
}
