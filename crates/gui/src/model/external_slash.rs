use super::{ExternalSlashCommand, SLASH_COMMANDS, SlashCommandRegistry};

impl SlashCommandRegistry {
    pub fn discover(executable: std::path::PathBuf) -> Result<Self, String> {
        let output = std::process::Command::new(&executable)
            .arg("--list-commands")
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!("command discovery failed: {}", output.status));
        }
        let stdout = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
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

    pub fn execute(&self, raw: &str, root: &std::path::Path) -> Result<String, String> {
        let (command, args) = self.parse(raw).ok_or("unknown external command")?;
        let executable = self
            .executable
            .as_ref()
            .ok_or("external command executable is not configured")?;
        let output = std::process::Command::new(executable)
            .arg(&command.name)
            .arg(args)
            .current_dir(root)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "external command failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        String::from_utf8(output.stdout).map_err(|error| error.to_string())
    }
}
