//! The form that adds a provider or edits one: what the user types, and the typed
//! `ProviderInput` it becomes. The form holds no secret: a credential is the name of an
//! environment variable or a keychain entry, never the key itself, so nothing to hide
//! reaches the screen, the command or a log.
use workbench_protocol::namespaces::settings::{
    CredentialRef, ModelInput, OpenaiCompatibleKind, ProviderInput, ProviderKind, ProviderView,
    SubscriptionKind,
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

impl FormKind {
    pub(super) const ALL: [Self; 5] = [
        Self::OpenaiCompatible,
        Self::Kimi,
        Self::Codex,
        Self::ClaudeSubscription,
        Self::Cursor,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::OpenaiCompatible => "OpenAI 互換",
            Self::Kimi => "Kimi",
            Self::Codex => "OpenAI Codex",
            Self::ClaudeSubscription => "Claude サブスクリプション",
            Self::Cursor => "Cursor",
        }
    }

    /// Whether the credential is an environment variable or a keychain entry the user names
    /// (otherwise it is the keychain account that holds the login).
    fn names_credential(self) -> bool {
        matches!(self, Self::OpenaiCompatible | Self::Kimi)
    }

    fn has_excluded_models(self) -> bool {
        !matches!(self, Self::Codex)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProviderForm {
    /// The profile being edited (a rename replaces it); `None` adds a new one.
    pub original: Option<String>,
    pub kind: FormKind,
    pub name: String,
    pub base_url: String,
    /// For `OpenAI 互換` and `Kimi`: an environment variable (`true`) or a keychain entry.
    pub env_credential: bool,
    pub env_var: String,
    pub keyring_service: String,
    /// The keychain account: the credential's for a named keychain entry, the login's for
    /// the subscription kinds.
    pub account: String,
    /// One model id per line; a line starting with `#` is listed but disabled.
    pub models: String,
    /// Model ids to leave out, separated by commas or lines.
    pub excluded: String,
    pub default_model: String,
}

impl Default for ProviderForm {
    fn default() -> Self {
        Self {
            original: None,
            kind: FormKind::OpenaiCompatible,
            name: String::new(),
            base_url: String::new(),
            env_credential: true,
            env_var: String::new(),
            keyring_service: String::new(),
            account: String::new(),
            models: String::new(),
            excluded: String::new(),
            default_model: String::new(),
        }
    }
}

/// What the user did with the form this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormAction {
    None,
    Save,
    Cancel,
}

impl ProviderForm {
    /// Draw the form. `busy` stops a second save while one runs.
    pub(super) fn draw(&mut self, ui: &mut egui::Ui, busy: bool) -> FormAction {
        let mut action = FormAction::None;
        ui.strong(if self.original.is_some() {
            "プロバイダーを編集"
        } else {
            "プロバイダーを追加"
        });
        ui.label("API キーそのものはここに入力しません。環境変数名またはキーチェーンの項目を指定します。");
        ui.horizontal_wrapped(|ui| {
            ui.label("種類");
            for kind in FormKind::ALL {
                ui.selectable_value(&mut self.kind, kind, kind.label());
            }
        });
        let line = |ui: &mut egui::Ui, label: &str, value: &mut String| {
            ui.horizontal(|ui| {
                ui.label(label);
                ui.text_edit_singleline(value);
            });
        };
        line(ui, "名前", &mut self.name);
        line(ui, "ベース URL", &mut self.base_url);
        if self.kind.names_credential() {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.env_credential, true, "環境変数");
                ui.selectable_value(&mut self.env_credential, false, "キーチェーン");
            });
            if self.env_credential {
                line(ui, "環境変数名", &mut self.env_var);
            } else {
                line(ui, "キーチェーンのサービス名", &mut self.keyring_service);
                line(ui, "キーチェーンのアカウント", &mut self.account);
            }
        } else {
            line(ui, "ログインのキーチェーンアカウント", &mut self.account);
        }
        ui.label("モデル（1 行に 1 つ。先頭に # を付けると無効）");
        ui.add(egui::TextEdit::multiline(&mut self.models).desired_rows(3));
        if self.kind.has_excluded_models() {
            line(ui, "除外するモデル（カンマ区切り）", &mut self.excluded);
        }
        line(ui, "既定のモデル", &mut self.default_model);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new("プロバイダーを保存"))
                .clicked()
            {
                action = FormAction::Save;
            }
            if ui.button("編集をやめる").clicked() {
                action = FormAction::Cancel;
            }
        });
        action
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
            kind,
            name: view.name.clone(),
            base_url: view.base_url.clone(),
            default_model: view.default_model.clone(),
            models: view
                .models
                .iter()
                .map(|model| {
                    if model.enabled {
                        model.id.clone()
                    } else {
                        format!("#{}", model.id)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            excluded: view.excluded_models.join(", "),
            ..Self::default()
        };
        match &view.credential {
            CredentialRef::Env { var } => {
                form.env_credential = true;
                form.env_var.clone_from(var);
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
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| match line.strip_prefix('#') {
                Some(id) => ModelInput {
                    id: id.trim().to_owned(),
                    enabled: false,
                },
                None => ModelInput {
                    id: line.to_owned(),
                    enabled: true,
                },
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
            return Err("プロバイダー名を入力してください。".into());
        }
        if base_url.is_empty() {
            return Err("ベース URL を入力してください。".into());
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
            return Err("ログインを保持するキーチェーンのアカウントを入力してください。".into());
        }
        Ok(match self.kind {
            FormKind::OpenaiCompatible | FormKind::Kimi => {
                let credential = if self.env_credential {
                    let var = self.env_var.trim().to_owned();
                    if var.is_empty() {
                        return Err("環境変数名を入力してください。".into());
                    }
                    CredentialRef::Env { var }
                } else {
                    let service = self.keyring_service.trim().to_owned();
                    if service.is_empty() || account.is_empty() {
                        return Err(
                            "キーチェーンのサービス名とアカウントを入力してください。".into()
                        );
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
