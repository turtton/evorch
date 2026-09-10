use super::{ExternalSlashCommand, SLASH_COMMANDS, SlashCommandRegistry};
#[path = "slash_process.rs"]
mod process;

impl SlashCommandRegistry {
    pub async fn discover(executable: std::path::PathBuf) -> Result<Self, String> {
        let stdout = process::run(
            tokio::process::Command::new(&executable).arg("--list-commands"),
            std::time::Duration::from_secs(10),
        )
        .await?;
        let mut registry = Self {
            executable: Some(executable),
            ..Self::default()
        };
        registry.load_external(&stdout);
        Ok(registry)
    }
    pub fn completions(&self, raw: &str) -> Vec<&ExternalSlashCommand> {
        let Some(prefix) = raw.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.contains(char::is_whitespace) {
            return Vec::new();
        }
        self.external
            .iter()
            .filter(|command| {
                command.name.starts_with(prefix)
                    && !SLASH_COMMANDS
                        .iter()
                        .any(|builtin| builtin.name == command.name)
            })
            .collect()
    }

    pub fn parse<'a>(&'a self, raw: &'a str) -> Option<(&'a ExternalSlashCommand, &'a str)> {
        let command = raw.trim().strip_prefix('/')?;
        let (name, args) = command
            .split_once(char::is_whitespace)
            .unwrap_or((command, ""));
        if SLASH_COMMANDS.iter().any(|builtin| builtin.name == name) {
            return None;
        }
        self.external
            .iter()
            .find(|command| command.name == name)
            .map(|command| (command, args.trim()))
    }

    pub fn help_text(&self) -> String {
        let mut text = super::help_text();
        for command in self.completions("/") {
            text.push_str(&format!(
                "\n/{}{} — {}",
                command.name,
                command
                    .argument_hint
                    .as_ref()
                    .map(|hint| format!(" {hint}"))
                    .unwrap_or_default(),
                command.description
            ));
        }
        text
    }

    pub async fn execute(&self, raw: &str, root: &std::path::Path) -> Result<String, String> {
        let (command, args) = self.parse(raw).ok_or("unknown external command")?;
        let executable = self
            .executable
            .as_ref()
            .ok_or("external command executable is not configured")?;
        process::run(
            tokio::process::Command::new(executable)
                .arg(&command.name)
                .arg(args)
                .current_dir(root),
            std::time::Duration::from_secs(30),
        )
        .await
    }
}
