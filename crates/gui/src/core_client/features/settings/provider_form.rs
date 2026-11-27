//! The form that adds a provider or edits one, laid out as the legacy Providers tab: what the
//! user types, and the typed `ProviderInput` it becomes. The form holds no secret: a
//! credential is the name of an environment variable or a keychain entry, never the key
//! itself, so nothing to hide reaches the screen, the command or a log.
use crate::model::settings_document::{NewProvider, NewProviderDefaults};
use crate::theme::{
    icons,
    text::muted,
    tokens::*,
    widgets::{accessible, ghost, icon_button},
};
use egui::vec2;
use workbench_protocol::namespaces::auth::LoginMethod;
use workbench_protocol::namespaces::settings::{
    CredentialRef, MAX_CONTEXT_OVERRIDE, ModelInput, ModelKind, ModelView, OpenaiCompatibleKind,
    ProviderInput, ProviderKind, ProviderView, SettingsRevision, SubscriptionKind,
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
    /// The name of the kind in the Add menu.
    pub(super) const fn menu_title(self) -> &'static str {
        match self {
            Self::OpenaiCompatible => "OpenAI-compatible",
            Self::Kimi => "Kimi",
            Self::Codex => "Codex",
            Self::ClaudeSubscription => "Claude",
            Self::Cursor => "Cursor",
        }
    }

    /// The line under it.
    pub(super) const fn menu_hint(self) -> &'static str {
        match self {
            Self::OpenaiCompatible => "Any /v1 endpoint with an API key",
            Self::Kimi => "Kimi subscription key",
            Self::Codex => "ChatGPT plan \u{b7} browser sign-in",
            Self::ClaudeSubscription => "Claude plan \u{b7} browser sign-in",
            Self::Cursor => "Browser sign-in or user API key",
        }
    }

    /// The browser sign-in the kind uses; the others keep an API key.
    pub(super) const fn login(self) -> Option<LoginMethod> {
        match self {
            Self::Codex => Some(LoginMethod::Codex),
            Self::ClaudeSubscription => Some(LoginMethod::Claude),
            Self::Cursor => Some(LoginMethod::Cursor),
            Self::OpenaiCompatible | Self::Kimi => None,
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

    /// Whether the kind has a quota the footer can show.
    pub(super) const fn quota_supported(self) -> bool {
        !matches!(self, Self::OpenaiCompatible)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ModelRow {
    pub id: String,
    pub enabled: bool,
    pub kind: ModelKind,
    /// The kind the model had when the form was opened (the one its id implies, for a new
    /// model): only a kind the user changed is sent, so a kind the file leaves implicit stays so.
    original_kind: ModelKind,
    /// The context window override as typed: empty for none, `400000` or `400k`.
    pub context: String,
}

fn inferred_kind(id: &str) -> ModelKind {
    match config::ModelKind::infer(id) {
        config::ModelKind::Chat => ModelKind::Chat,
        config::ModelKind::ImageGeneration => ModelKind::ImageGeneration,
    }
}

/// A context window as the table shows it: `400k` when it is a whole number of thousands.
fn context_text(window: Option<u64>) -> String {
    match window {
        None => String::new(),
        Some(window) if window % 1000 == 0 => format!("{}k", window / 1000),
        Some(window) => window.to_string(),
    }
}

fn parse_context(text: &str) -> Option<Result<u64, ()>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (digits, scale) = match text.chars().last() {
        Some('k' | 'K') => (&text[..text.len() - 1], 1000),
        Some('m' | 'M') => (&text[..text.len() - 1], 1_000_000),
        _ => (text, 1),
    };
    Some(
        digits
            .trim()
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .filter(|n| (1..=MAX_CONTEXT_OVERRIDE).contains(n))
            .ok_or(()),
    )
}

impl ModelRow {
    pub(super) fn new(id: String) -> Self {
        let kind = inferred_kind(&id);
        Self {
            id,
            enabled: true,
            kind,
            original_kind: kind,
            context: String::new(),
        }
    }

    fn from_view(model: &ModelView) -> Self {
        Self {
            id: model.id.clone(),
            enabled: model.enabled,
            kind: model.kind,
            original_kind: model.kind,
            context: context_text(model.context_override),
        }
    }
}

fn kind_label(kind: ModelKind) -> &'static str {
    match kind {
        ModelKind::Chat => "Chat",
        ModelKind::ImageGeneration => "Image",
    }
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
    /// Whether the footer shows the quota of the provider (only a kind with a quota has the switch).
    pub show_quota: bool,
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
            models: defaults.models.into_iter().map(ModelRow::new).collect(),
            new_model: String::new(),
            excluded: String::new(),
            default_model: defaults.default_model,
            show_quota: true,
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
            self.show_quota,
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
            source.show_quota,
        )
    }

    /// Draw the Connection fields: what names the provider and where its credential is kept.
    /// The Account card above them holds the credential itself; nothing secret is typed here.
    pub(super) fn draw_connection(&mut self, ui: &mut egui::Ui, name_locked: bool) {
        let previous_name = self.name.clone();
        ui.add_enabled_ui(!name_locked, |ui| {
            labelled_line(ui, "Name", &mut self.name, None);
        });
        if name_locked {
            ui.label(crate::theme::text::muted(
                "The name cannot change while this provider is signing in.",
            ));
        }
        // An edited provider keeps its account: following the rename would leave its stored
        // sign-in behind under the old account.
        if self.original.is_none() && self.account == previous_name {
            self.account.clone_from(&self.name);
        }
        labelled_line(ui, "Base URL", &mut self.base_url, None);
        if self.kind.names_credential() {
            ui.horizontal(|ui| {
                ui.label("Credential");
                ui.selectable_value(&mut self.env_credential, false, "Keyring");
                ui.selectable_value(&mut self.env_credential, true, "Env var");
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
        } else {
            let label = if self.kind == FormKind::Codex {
                "Account"
            } else {
                "Credential account"
            };
            labelled_line(ui, label, &mut self.account, None);
        }
        if self.kind.has_excluded_models() {
            let width = ui.available_width();
            let label = ui.label("Excluded models");
            ui.add(
                egui::TextEdit::multiline(&mut self.excluded)
                    .hint_text("one per line")
                    .desired_rows(2)
                    .desired_width(width)
                    .background_color(palette().INPUT),
            )
            .labelled_by(label.id);
        }
    }

    /// The models table: On, the id, its kind and context window, the default model (a star)
    /// and removal, with a row to add an id by hand.
    pub(super) fn draw_models(&mut self, ui: &mut egui::Ui) {
        let mut remove = None;
        let mut default = None;
        egui::Grid::new("provider-models")
            .num_columns(6)
            .spacing(vec2(SP_3, SP_1))
            .striped(false)
            .show(ui, |ui| {
                for title in ["On", "Model", "Kind", "Context", "Default", ""] {
                    ui.label(muted(title));
                }
                ui.end_row();
                for (index, model) in self.models.iter_mut().enumerate() {
                    ui.push_id(index, |ui| {
                        switch(ui, &mut model.enabled, &format!("Enable {}", model.id));
                    });
                    ui.label(egui::RichText::new(&model.id).monospace());
                    egui::ComboBox::from_id_salt(("model-kind", index))
                        .width(72.0)
                        .selected_text(kind_label(model.kind))
                        .show_ui(ui, |ui| {
                            for kind in [ModelKind::Chat, ModelKind::ImageGeneration] {
                                ui.selectable_value(&mut model.kind, kind, kind_label(kind));
                            }
                        })
                        .response
                        .widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::ComboBox,
                                true,
                                format!("Kind of {}", model.id),
                            )
                        });
                    let context = ui.add(
                        egui::TextEdit::singleline(&mut model.context)
                            .hint_text("auto")
                            .desired_width(64.0)
                            .background_color(palette().INPUT),
                    );
                    accessible(&context, &format!("Context window of {}", model.id));
                    let is_default = self.default_model == model.id;
                    let chat = model.kind == ModelKind::Chat;
                    let (icon, name) = if is_default {
                        (
                            icons::filled(ui, icons::STAR).color(palette().WARNING),
                            format!("Default model: {}", model.id),
                        )
                    } else if chat {
                        (
                            egui::RichText::new(icons::STAR).color(palette().TEXT_MUTED),
                            format!("Make {} the default model", model.id),
                        )
                    } else {
                        (
                            egui::RichText::new(icons::STAR).color(palette().TEXT_MUTED),
                            format!("{} is an image model and cannot be the default", model.id),
                        )
                    };
                    let star = ui.add_enabled(model.enabled && chat, ghost(icon));
                    accessible(&star, &name);
                    if star.on_hover_text(&name).clicked() {
                        default = Some(model.id.clone());
                    }
                    let trash = icon_button(ui, icons::TRASH, &format!("Remove {}", model.id));
                    if trash.clicked() {
                        remove = Some(index);
                    }
                    ui.end_row();
                }
            });
        if let Some(id) = default {
            self.default_model = id;
        }
        if let Some(index) = remove {
            let removed = self.models.remove(index);
            if self.default_model == removed.id {
                self.default_model.clear();
            }
        }
        let mut add = false;
        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.new_model)
                    .hint_text("Add a model ID manually…")
                    .desired_width((ui.available_width() - 40.0).max(40.0))
                    .background_color(palette().INPUT),
            );
            let enter = edit.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            add = icon_button(ui, icons::PLUS, "Add model").clicked() || enter;
        });
        if add {
            let id = std::mem::take(&mut self.new_model);
            self.add_model(id.trim());
        }
    }

    /// Adds `id` as an enabled model unless the form has it; the first chat model added becomes
    /// the default when there is none. Returns whether it was added.
    pub(super) fn add_model(&mut self, id: &str) -> bool {
        if id.is_empty() || self.models.iter().any(|model| model.id == id) {
            return false;
        }
        let row = ModelRow::new(id.to_owned());
        if self.default_model.is_empty() && row.kind == ModelKind::Chat {
            self.default_model = row.id.clone();
        }
        self.models.push(row);
        true
    }

    /// The footer switch of the provider: shown for a kind that has a quota, with what happens
    /// when it is left alone.
    pub(super) fn draw_quota(&mut self, ui: &mut egui::Ui) {
        if !self.kind.quota_supported() {
            return;
        }
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label("Show quota in the status bar");
                ui.label(muted("Default: on for subscription providers"));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                switch(ui, &mut self.show_quota, "Show quota in the status bar");
            });
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
        form.models = view.models.iter().map(ModelRow::from_view).collect();
        form.show_quota = view.show_quota;
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

    fn model_inputs(&self) -> Result<Vec<ModelInput>, String> {
        self.models
            .iter()
            .map(|model| {
                let context_override = match parse_context(&model.context) {
                    None => None,
                    Some(Ok(window)) => Some(window),
                    Some(Err(())) => {
                        return Err(format!(
                            "The context window of {} must be a number such as 400000 or 400k.",
                            model.id
                        ));
                    }
                };
                Ok(ModelInput {
                    id: model.id.clone(),
                    enabled: model.enabled,
                    kind: (model.kind != model.original_kind).then_some(model.kind),
                    context_override,
                })
            })
            .collect()
    }

    /// What the wire carries of the switch: only an off switch of a kind with a quota, because
    /// on is what no value means.
    fn quota_input(&self) -> Option<bool> {
        (self.kind.quota_supported() && !self.show_quota).then_some(false)
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
        let models = self.model_inputs()?;
        let default_model = self.default_model.trim().to_owned();
        if self
            .models
            .iter()
            .any(|model| model.id == default_model && model.kind == ModelKind::ImageGeneration)
        {
            return Err("An image model cannot be the default chat model.".into());
        }
        let show_quota = self.quota_input();
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
                    show_quota,
                }
            }
            FormKind::Codex => ProviderInput::Codex {
                name,
                account,
                base_url,
                models,
                default_model,
                show_quota,
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
                show_quota,
            },
        })
    }
}

/// A switch painted as a pill with a knob; its accessible name is `label` and it reads as
/// checked while on.
pub(super) fn switch(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(30.0, 16.0), egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let enabled = ui.is_enabled();
    let checked = *on;
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, checked, label)
    });
    let t = ui
        .ctx()
        .animate_bool_with_time(response.id, checked, HOVER_FADE);
    let track = palette().BORDER.lerp_to_gamma(palette().ACCENT, t);
    let painter = ui.painter();
    painter.rect_filled(rect, rect.height() / 2.0, track);
    let knob = egui::lerp(rect.left() + 8.0..=rect.right() - 8.0, t);
    painter.circle_filled(egui::pos2(knob, rect.center().y), 6.0, palette().ACCENT_FG);
    response.on_hover_text(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(kind: FormKind) -> ProviderForm {
        let mut form = ProviderForm::new(
            kind,
            NewProviderDefaults {
                name: "p".into(),
                base_url: "https://x.test".into(),
                models: vec!["chat-a".into(), "gpt-image-2".into()],
                default_model: "chat-a".into(),
            },
        );
        form.env_credential = true;
        form.env_var = "KEY".into();
        form
    }

    #[test]
    fn an_image_model_cannot_be_the_default_chat_model() {
        let mut form = form(FormKind::OpenaiCompatible);
        assert!(form.to_input().is_ok());
        form.default_model = "gpt-image-2".into();
        assert_eq!(
            form.to_input().unwrap_err(),
            "An image model cannot be the default chat model."
        );
        // A model the user turned into an image model is refused the same way.
        form.default_model = "chat-a".into();
        form.models[0].kind = ModelKind::ImageGeneration;
        assert!(form.to_input().is_err());
    }

    #[test]
    fn only_a_changed_kind_and_a_typed_context_are_sent() {
        let mut form = form(FormKind::OpenaiCompatible);
        form.models[0].context = "400k".into();
        form.models[1].kind = ModelKind::Chat;
        let ProviderInput::OpenaiCompatible { models, .. } = form.to_input().unwrap() else {
            panic!("expected an OpenAI-compatible provider");
        };
        assert_eq!(models[0].kind, None);
        assert_eq!(models[0].context_override, Some(400_000));
        assert_eq!(
            models[1].kind,
            Some(ModelKind::Chat),
            "a changed kind is sent"
        );
        form.models[0].context = "0".into();
        assert!(form.to_input().unwrap_err().contains("must be a number"));
        form.models[0].context = "250000".into();
        assert!(form.to_input().is_ok());
        assert_eq!(context_text(Some(400_000)), "400k");
        assert_eq!(context_text(Some(250_500)), "250500");
    }

    #[test]
    fn the_quota_switch_is_sent_only_when_off_and_only_for_a_kind_with_a_quota() {
        for (kind, supported) in [
            (FormKind::Codex, true),
            (FormKind::ClaudeSubscription, true),
            (FormKind::Cursor, true),
            (FormKind::Kimi, true),
            (FormKind::OpenaiCompatible, false),
        ] {
            let mut form = form(kind);
            assert_eq!(form.kind.quota_supported(), supported);
            assert_eq!(
                form.quota_input(),
                None,
                "on is the default: nothing is written"
            );
            form.show_quota = false;
            assert_eq!(form.quota_input(), supported.then_some(false), "{kind:?}");
        }
    }
}
