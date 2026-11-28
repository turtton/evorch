//! The built-in slash commands, how a draft parses into one, and the
//! completion and help text they offer. Shared by the embedded workbench
//! (`model::composer`, which offers them all) and the core client (which offers
//! the ones it can carry out).

use std::ops::Range;

use super::mentions::{CompletionItem, CompletionKind};

pub struct SlashCommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

const GOAL_COMMAND: SlashCommandSpec = SlashCommandSpec {
    name: "goal",
    description: "Track an objective in this conversation",
    argument_hint: Some("<text>"),
};

pub const SLASH_COMMANDS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        name: "team",
        description: "Submit a team goal with explicit delegation value",
        argument_hint: Some("<delegation value> | <goal>"),
    },
    GOAL_COMMAND,
    SlashCommandSpec {
        name: "help",
        description: "Show available commands",
        argument_hint: None,
    },
    SlashCommandSpec {
        name: "new",
        description: "Start a new thread",
        argument_hint: None,
    },
    SlashCommandSpec {
        name: "compaction",
        description: "Request context compaction at the next turn boundary",
        argument_hint: None,
    },
    SlashCommandSpec {
        name: "continue",
        description: "Resume the current conversation after a stop or error",
        argument_hint: None,
    },
    SlashCommandSpec {
        name: "run",
        description: "Start a background run without opening a pane",
        argument_hint: Some("<text>"),
    },
    SlashCommandSpec {
        name: "btw",
        description: "Ask a side question without adding to the conversation",
        argument_hint: Some("<question>"),
    },
];

pub enum ComposerInput<'a> {
    Empty,
    Chat,
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
        return ComposerInput::Chat;
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

/// [`completions`] limited to the commands named in `offered`, when given,
/// still in declaration order.
pub fn completions_among(raw: &str, offered: Option<&[&str]>) -> Vec<&'static SlashCommandSpec> {
    let Some(prefix) = raw.strip_prefix('/') else {
        return Vec::new();
    };
    if prefix.contains(char::is_whitespace) {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(prefix) && is_offered(spec, offered))
        .collect()
}

/// [`help_text`] limited to the commands named in `offered`, when given.
pub fn help_text_among(offered: Option<&[&str]>) -> String {
    SLASH_COMMANDS
        .iter()
        .filter(|spec| is_offered(spec, offered))
        .map(|spec| match spec.argument_hint {
            Some(hint) => format!("/{} {hint} — {}", spec.name, spec.description),
            None => format!("/{} — {}", spec.name, spec.description),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The completion row of `spec`: its name and argument hint, replacing `range`
/// with `/name `.
pub fn slash_item(spec: &SlashCommandSpec, range: Range<usize>) -> CompletionItem {
    CompletionItem {
        kind: CompletionKind::Command,
        label: match spec.argument_hint {
            Some(hint) => format!("/{} {hint}", spec.name),
            None => format!("/{}", spec.name),
        },
        detail: spec.description.into(),
        range,
        replacement: format!("/{} ", spec.name),
    }
}

fn is_offered(spec: &SlashCommandSpec, offered: Option<&[&str]>) -> bool {
    offered.is_none_or(|offered| offered.contains(&spec.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFERED: &[&str] = &["team", "goal", "help", "new", "compaction"];

    fn names(specs: Vec<&SlashCommandSpec>) -> Vec<&'static str> {
        specs.iter().map(|spec| spec.name).collect()
    }

    #[test]
    fn completions_among_keeps_declaration_order_and_drops_what_is_not_offered() {
        let all = names(completions_among("/", Some(OFFERED)));
        assert_eq!(all, ["team", "goal", "help", "new", "compaction"]);
        assert_eq!(
            names(completions_among("/c", Some(OFFERED))),
            ["compaction"]
        );
        assert!(completions_among("/r", Some(OFFERED)).is_empty());
        assert!(completions_among("/goal x", Some(OFFERED)).is_empty());
        assert_eq!(
            completions_among("/c", None).len(),
            2,
            "every command without a filter"
        );
    }

    #[test]
    fn help_text_among_lists_only_the_offered_commands_in_the_legacy_format() {
        let help = help_text_among(Some(&["goal", "new"]));
        assert_eq!(
            help,
            "/goal <text> — Track an objective in this conversation\n/new — Start a new thread"
        );
    }

    #[test]
    fn slash_item_shows_the_hint_and_replaces_with_the_bare_name() {
        let goal = SLASH_COMMANDS.iter().find(|s| s.name == "goal").unwrap();
        let item = slash_item(goal, 0..3);
        assert_eq!(item.label, "/goal <text>");
        assert_eq!(item.replacement, "/goal ");
        assert_eq!(item.range, 0..3);
        assert_eq!(item.kind, CompletionKind::Command);
        let new = SLASH_COMMANDS.iter().find(|s| s.name == "new").unwrap();
        assert_eq!(slash_item(new, 0..1).label, "/new");
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
        for raw in [" hi there \n", "日本語 /help"] {
            // When
            let parsed = parse_input(raw);
            // Then
            assert!(matches!(parsed, ComposerInput::Chat));
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
            (" /compaction \n", "compaction", ""),
            ("/compaction extra", "compaction", "extra"),
        ] {
            // When
            let parsed = parse_input(raw);
            // Then
            assert!(matches!(parsed, ComposerInput::Command { spec, args }
                if spec.name == name && args == expected_args));
        }
    }

    #[test]
    fn parse_new_without_args() {
        // Given
        let raw = "/new";
        // When
        let parsed = parse_input(raw);
        // Then
        assert!(matches!(parsed, ComposerInput::Command { spec, args }
            if spec.name == "new" && args.is_empty()));
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
}
