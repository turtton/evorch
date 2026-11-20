use workbench_client::restart::intent::{BlockerSelector, TargetSelector};

/// One management command, already checked: a target is an opaque ID, never
/// a path or an argument vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminCommand {
    Targets,
    Status,
    Request {
        target: TargetSelector,
        yes: bool,
        no_wait: bool,
    },
    Cancel,
    Interrupt {
        blocker: BlockerSelector,
        yes: bool,
    },
}

/// The command and where the configuration is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub command: AdminCommand,
    pub user_config: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error(
        "usage: evorch core restart <targets | status | request [--target ID] [--yes] [--no-wait] | cancel | interrupt (--blocker N | --run ID) [--yes]> [--user-config DIR]"
    )]
    Usage,
    #[error("{0}")]
    Invalid(String),
}

pub fn parse(argv: &[String]) -> Result<Parsed, ParseError> {
    let mut words = argv.iter().map(String::as_str).peekable();
    if words.peek() == Some(&"core") {
        words.next();
    }
    if words.next() != Some("restart") {
        return Err(ParseError::Usage);
    }
    let sub = words.next().ok_or(ParseError::Usage)?;
    let mut user_config = None;
    let mut target = None;
    let mut yes = false;
    let mut no_wait = false;
    let mut blocker = None;
    while let Some(word) = words.next() {
        let mut value = |name: &str| {
            words
                .next()
                .map(str::to_owned)
                .ok_or_else(|| ParseError::Invalid(format!("{name} needs a value")))
        };
        match word {
            "--user-config" => user_config = Some(value("--user-config")?.into()),
            "--target" => target = Some(value("--target")?),
            "--yes" => yes = true,
            "--no-wait" => no_wait = true,
            "--blocker" => {
                let text = value("--blocker")?;
                let index: usize = text.parse().map_err(|_| {
                    ParseError::Invalid(format!(
                        "--blocker takes the number shown in the list, not `{text}`"
                    ))
                })?;
                blocker = Some(BlockerSelector::Index(index));
            }
            "--run" => blocker = Some(BlockerSelector::Run(value("--run")?)),
            other => {
                return Err(ParseError::Invalid(format!("unknown argument `{other}`")));
            }
        }
    }
    let only = |allowed: bool, what: &str| {
        if allowed {
            Ok(())
        } else {
            Err(ParseError::Invalid(format!(
                "{what} is not an option of `{sub}`"
            )))
        }
    };
    let command = match sub {
        "targets" | "status" | "cancel" => {
            only(target.is_none(), "--target")?;
            only(!yes, "--yes")?;
            only(!no_wait, "--no-wait")?;
            only(blocker.is_none(), "--blocker/--run")?;
            match sub {
                "targets" => AdminCommand::Targets,
                "status" => AdminCommand::Status,
                _ => AdminCommand::Cancel,
            }
        }
        "request" => {
            only(blocker.is_none(), "--blocker/--run")?;
            let target = match target {
                None => TargetSelector::Current,
                Some(text) => TargetSelector::parse(&text)
                    .map_err(|e| ParseError::Invalid(format!("--target: {e}")))?,
            };
            AdminCommand::Request {
                target,
                yes,
                no_wait,
            }
        }
        "interrupt" => {
            only(target.is_none(), "--target")?;
            only(!no_wait, "--no-wait")?;
            AdminCommand::Interrupt {
                blocker: blocker.ok_or_else(|| {
                    ParseError::Invalid("interrupt needs --blocker N or --run ID".into())
                })?,
                yes,
            }
        }
        _ => return Err(ParseError::Usage),
    };
    Ok(Parsed {
        command,
        user_config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(words: &[&str]) -> Result<Parsed, ParseError> {
        parse(&words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn the_target_defaults_to_current_and_nothing_is_confirmed_by_default() {
        let parsed = p(&["core", "restart", "request"]).unwrap();
        assert_eq!(
            parsed.command,
            AdminCommand::Request {
                target: TargetSelector::Current,
                yes: false,
                no_wait: false
            }
        );
        assert_eq!(p(&["restart", "request"]).unwrap(), parsed);
    }

    #[test]
    fn a_target_is_an_id_and_never_a_path() {
        assert!(p(&["restart", "request", "--target", "/bin/sh"]).is_err());
        assert!(p(&["restart", "request", "--target", "a b"]).is_err());
        assert!(p(&["restart", "request", "--target", "next-build"]).is_ok());
    }

    #[test]
    fn options_belong_to_their_commands() {
        assert!(p(&["restart", "status", "--yes"]).is_err());
        assert!(p(&["restart", "cancel", "--target", "x"]).is_err());
        assert!(p(&["restart", "interrupt"]).is_err());
        assert!(p(&["restart", "interrupt", "--blocker", "x"]).is_err());
        assert_eq!(
            p(&["restart", "interrupt", "--blocker", "1", "--yes"])
                .unwrap()
                .command,
            AdminCommand::Interrupt {
                blocker: BlockerSelector::Index(1),
                yes: true
            }
        );
        assert!(p(&["restart"]).is_err());
        assert!(p(&["maintenance"]).is_err());
        assert!(p(&["restart", "reboot"]).is_err());
    }
}
