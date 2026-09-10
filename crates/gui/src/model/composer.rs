pub struct SlashCommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalSlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlashCommandRegistry {
    pub external: Vec<ExternalSlashCommand>,
}

impl SlashCommandRegistry {
    pub fn builtin() -> &'static [SlashCommandSpec] {
        SLASH_COMMANDS
    }

    pub fn load_external(&mut self, stdout: &str) {
        self.external = stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(3, '\t');
                let name = fields.next()?.trim();
                let description = fields.next()?.trim();
                if name.is_empty()
                    || !name
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphanumeric())
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return None;
                }
                Some(ExternalSlashCommand {
                    name: name.to_owned(),
                    description: description.to_owned(),
                    argument_hint: fields
                        .next()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                })
            })
            .collect();
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        SLASH_COMMANDS
            .iter()
            .map(|command| command.name)
            .chain(self.external.iter().map(|command| command.name.as_str()))
    }
}

pub const SLASH_COMMANDS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        name: "undo",
        description: "Restore the previous workspace snapshot",
        argument_hint: None,
    },
    SlashCommandSpec {
        name: "redo",
        description: "Restore the next workspace snapshot",
        argument_hint: None,
    },
    SlashCommandSpec {
        name: "goal",
        description: "Submit a goal to the orchestrator loop",
        argument_hint: Some("<text>"),
    },
    SlashCommandSpec {
        name: "help",
        description: "Show available commands",
        argument_hint: None,
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageAttachment {
    pub media_type: String,
    pub data: String,
}

impl ImageAttachment {
    pub fn from_data_url(value: &str) -> Option<Self> {
        let (header, data) = value.strip_prefix("data:")?.split_once(',')?;
        let (media_type, encoding) = header.split_once(';').unwrap_or((header, ""));
        if !media_type.starts_with("image/") || encoding != "base64" || data.is_empty() {
            return None;
        }
        Some(Self {
            media_type: media_type.to_owned(),
            data: data.to_owned(),
        })
    }

    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

pub enum ComposerInput<'a> {
    Empty,
    Chat(&'a str),
    Command {
        spec: &'static SlashCommandSpec,
        args: &'a str,
    },
    UnknownCommand {
        name: &'a str,
    },
}

pub fn parse_input(raw: &str) -> ComposerInput<'_> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ComposerInput::Empty;
    }
    let Some(command) = trimmed.strip_prefix('/') else {
        return ComposerInput::Chat(trimmed);
    };
    let (name, rest) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    match SLASH_COMMANDS.iter().find(|spec| spec.name == name) {
        Some(spec) => ComposerInput::Command {
            spec,
            args: rest.trim(),
        },
        None => ComposerInput::UnknownCommand { name },
    }
}

pub fn completions(raw: &str) -> Vec<&'static SlashCommandSpec> {
    let Some(prefix) = raw.strip_prefix('/') else {
        return Vec::new();
    };
    if prefix.contains(char::is_whitespace) {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(prefix))
        .collect()
}

/// Lists commands in declaration order, without a trailing newline.
pub fn help_text() -> String {
    SLASH_COMMANDS
        .iter()
        .map(|spec| match spec.argument_hint {
            Some(hint) => format!("/{} {hint} — {}", spec.name, spec.description),
            None => format!("/{} — {}", spec.name, spec.description),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub const PROVIDER_MISSING_GUIDANCE: &str = "No provider configured yet — open Settings to add an openai-compatible provider. /goal and /help still work.";

/// Goal ペイン専用の案内。ペイン間でラベルが重複しない文面にする。
pub const GOAL_PROVIDER_GUIDANCE: &str = "No provider configured — goals can be submitted, but agents cannot run until a provider is set up in Settings.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderStatus {
    Configured,
    NotConfigured { guidance: String },
}

impl Default for ProviderStatus {
    fn default() -> Self {
        Self::NotConfigured {
            guidance: PROVIDER_MISSING_GUIDANCE.into(),
        }
    }
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct ComposerModel {
    pub input: String,
    pub completions_dismissed_for: Option<String>,
    pub attachments: Vec<ImageAttachment>,
    pub image_input_supported: bool,
}

impl ComposerModel {
    pub fn completions_visible(&self) -> bool {
        self.completions_dismissed_for.as_ref() != Some(&self.input)
            && !completions(&self.input).is_empty()
    }

    pub fn dismiss_completions(&mut self) {
        self.completions_dismissed_for = Some(self.input.clone());
    }

    pub fn add_pasted_image(&mut self, value: &str) -> bool {
        let Some(image) = ImageAttachment::from_data_url(value) else {
            return false;
        };
        self.attachments.push(image);
        true
    }

    pub fn remove_attachment(&mut self, index: usize) -> bool {
        if index >= self.attachments.len() {
            return false;
        }
        self.attachments.remove(index);
        true
    }

    pub fn image_warning(&self) -> Option<&'static str> {
        (!self.attachments.is_empty() && !self.image_input_supported)
            .then_some("このモデルは画像入力に対応していないため、画像は送信できません")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guidance_points_to_settings_with_distinct_pane_labels() {
        // Given
        let composer = PROVIDER_MISSING_GUIDANCE;
        let goal = GOAL_PROVIDER_GUIDANCE;
        // When
        let settings_available =
            composer.contains("Settings") && !composer.contains("coming in v0.3");
        // Then
        assert!(
            settings_available,
            "composer guidance must point to available Settings"
        );
        assert_ne!(goal, composer);
    }

    #[test]
    fn parse_empty_and_whitespace_is_empty() {
        // Given
        for raw in ["", "  \t\n", "\u{3000}"] {
            // When
            let parsed = parse_input(raw);
            // Then
            assert!(matches!(parsed, ComposerInput::Empty));
        }
    }

    #[test]
    fn parse_plain_text_is_chat() {
        // Given
        for (raw, expected) in [
            (" hi there \n", "hi there"),
            ("日本語 /help", "日本語 /help"),
        ] {
            // When
            let parsed = parse_input(raw);
            // Then
            assert!(matches!(parsed, ComposerInput::Chat(text) if text == expected));
        }
    }

    #[test]
    fn parse_goal_with_args_and_help() {
        // Given
        for (raw, name, expected_args) in [
            (" /goal  ship the feature \n", "goal", "ship the feature"),
            ("/goal\t日本語\u{3000}", "goal", "日本語"),
            ("/goal", "goal", ""),
            ("/help", "help", ""),
            ("/help extra", "help", "extra"),
        ] {
            // When
            let parsed = parse_input(raw);
            // Then
            assert!(matches!(parsed, ComposerInput::Command { spec, args }
                if spec.name == name && args == expected_args));
        }
    }

    #[test]
    fn parse_unknown_slash_is_unknown_not_chat() {
        // Given
        for (raw, expected) in [
            ("/unknown args", "unknown"),
            ("/Goal x", "Goal"),
            ("//help", "/help"),
        ] {
            // When
            let parsed = parse_input(raw);
            // Then
            assert!(matches!(parsed, ComposerInput::UnknownCommand { name } if name == expected));
        }
    }

    #[test]
    fn parse_slash_alone_is_unknown() {
        // Given
        let raw = "/";
        // When
        let parsed = parse_input(raw);
        // Then
        assert!(matches!(parsed, ComposerInput::UnknownCommand { name: "" }));
    }

    #[test]
    fn completions_prefix_match_and_stop_after_space() {
        // Given
        let cases: &[(&str, &[&str])] = &[
            ("/", &["undo", "redo", "goal", "help"]),
            ("/g", &["goal"]),
            ("/goal", &["goal"]),
            ("/h", &["help"]),
            ("/goal x", &[]),
            ("hi", &[]),
            ("", &[]),
            (" /g", &[]),
            ("/G", &[]),
            ("/unknown", &[]),
            ("/goal ", &[]),
            ("/goal\t", &[]),
            ("/\n", &[]),
            ("/g\u{3000}", &[]),
        ];
        for (raw, expected) in cases {
            // When
            let names: Vec<_> = completions(raw).iter().map(|spec| spec.name).collect();
            // Then
            assert_eq!(&names, expected, "{raw:?}");
        }
    }

    #[test]
    fn help_text_lists_every_command() {
        // Given
        let expected = [
            "/undo — Restore the previous workspace snapshot",
            "/redo — Restore the next workspace snapshot",
            "/goal <text> — Submit a goal to the orchestrator loop",
            "/help — Show available commands",
        ];
        // When
        let text = help_text();
        // Then
        assert_eq!(text, expected.join("\n"));
        assert_eq!(text.lines().count(), SLASH_COMMANDS.len());
    }

    #[test]
    fn provider_status_default_is_not_configured_with_guidance() {
        // Given
        let expected = ProviderStatus::NotConfigured {
            guidance: PROVIDER_MISSING_GUIDANCE.into(),
        };
        // When
        let status = ProviderStatus::default();
        // Then
        assert_eq!(status, expected);
    }

    #[test]
    fn composer_model_default_has_empty_input() {
        // Given / When
        let model = ComposerModel::default();
        // Then
        assert_eq!(model.input, "");
    }

    #[test]
    fn pasted_image_can_be_added_and_removed() {
        let mut model = ComposerModel::default();
        assert!(model.add_pasted_image("data:image/png;base64,aGVsbG8="));
        assert_eq!(
            model.attachments[0].data_url(),
            "data:image/png;base64,aGVsbG8="
        );
        assert!(model.image_warning().is_some());
        assert!(model.remove_attachment(0));
        assert!(!model.remove_attachment(0));
    }

    #[test]
    fn external_slash_registry_accepts_only_safe_tabular_entries() {
        let mut registry = SlashCommandRegistry::default();
        registry.load_external("review\tRun review\t<path>\n-bad\tignored\n");
        assert_eq!(registry.external.len(), 1);
        assert_eq!(registry.names().collect::<Vec<_>>().last(), Some(&"review"));
    }
}
