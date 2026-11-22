use workbench_client::restart::intent::{BlockerSelector, TargetSelector};
use workbench_protocol::namespaces::lifecycle::AdminId;

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
    Quarantine(QuarantineOp),
    Graphs(GraphsOp),
}

/// Q1: graphs the core quarantined. A graph is named by the ID the list shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineOp {
    List,
    Show { graph: AdminId },
    Discard { graph: AdminId, yes: bool },
}

/// Q2: graphs a restart left undisposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphsOp {
    List,
    Show { graph: AdminId },
    Release { graph: AdminId, yes: bool },
    Resume { graph: AdminId, yes: bool },
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
        "usage: evorch core restart <targets | status | request [--target ID] [--yes] [--no-wait] | cancel | interrupt (--blocker N | --run ID) [--yes]> | evorch core quarantine <list | show ID | discard ID [--yes]> | evorch core graphs <list | show ID | release ID [--yes] | resume ID [--yes]>  [--user-config DIR]"
    )]
    Usage,
    #[error("{0}")]
    Invalid(String),
}

type Words<'a> =
    std::iter::Peekable<std::iter::Map<std::slice::Iter<'a, String>, fn(&String) -> &str>>;

pub fn parse(argv: &[String]) -> Result<Parsed, ParseError> {
    let mut words: Words<'_> = argv
        .iter()
        .map((|s| s.as_str()) as fn(&String) -> &str)
        .peekable();
    if words.peek() == Some(&"core") {
        words.next();
    }
    match words.next() {
        Some("restart") => {}
        Some(group @ ("quarantine" | "graphs")) => return parse_recovery(group, words),
        _ => return Err(ParseError::Usage),
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

/// `quarantine` and `graphs`: a subcommand, at most one graph ID, `--yes` for
/// the three that change something, and `--user-config`.
fn parse_recovery(group: &str, mut words: Words<'_>) -> Result<Parsed, ParseError> {
    let sub = words.next().ok_or(ParseError::Usage)?;
    let mut user_config = None;
    let mut graph = None;
    let mut yes = false;
    while let Some(word) = words.next() {
        match word {
            "--user-config" => {
                user_config = Some(
                    words
                        .next()
                        .ok_or_else(|| ParseError::Invalid("--user-config needs a value".into()))?
                        .into(),
                );
            }
            "--yes" => yes = true,
            other if other.starts_with('-') => {
                return Err(ParseError::Invalid(format!("unknown argument `{other}`")));
            }
            id if graph.is_none() => {
                graph = Some(
                    AdminId::new(id)
                        .map_err(|e| ParseError::Invalid(format!("graph ID `{id}`: {e}")))?,
                );
            }
            _ => return Err(ParseError::Invalid("one graph ID at a time".into())),
        }
    }
    let changes = matches!(
        (group, sub),
        ("quarantine", "discard") | ("graphs", "release" | "resume")
    );
    if yes && !changes {
        return Err(ParseError::Invalid(format!(
            "--yes is not an option of `{sub}`"
        )));
    }
    let listing = sub == "list";
    if listing && graph.is_some() {
        return Err(ParseError::Invalid("`list` takes no graph ID".into()));
    }
    let need = |graph: Option<AdminId>| {
        graph.ok_or_else(|| ParseError::Invalid(format!("`{sub}` needs a graph ID from `list`")))
    };
    let command = match (group, sub) {
        ("quarantine", "list") => AdminCommand::Quarantine(QuarantineOp::List),
        ("quarantine", "show") => AdminCommand::Quarantine(QuarantineOp::Show {
            graph: need(graph)?,
        }),
        ("quarantine", "discard") => AdminCommand::Quarantine(QuarantineOp::Discard {
            graph: need(graph)?,
            yes,
        }),
        ("graphs", "list") => AdminCommand::Graphs(GraphsOp::List),
        ("graphs", "show") => AdminCommand::Graphs(GraphsOp::Show {
            graph: need(graph)?,
        }),
        ("graphs", "release") => AdminCommand::Graphs(GraphsOp::Release {
            graph: need(graph)?,
            yes,
        }),
        ("graphs", "resume") => AdminCommand::Graphs(GraphsOp::Resume {
            graph: need(graph)?,
            yes,
        }),
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

    #[test]
    fn recovery_commands_name_one_graph_and_only_the_changing_ones_take_yes() {
        let id = |s: &str| AdminId::new(s).unwrap();
        assert_eq!(
            p(&["core", "graphs", "release", "g1", "--yes"])
                .unwrap()
                .command,
            AdminCommand::Graphs(GraphsOp::Release {
                graph: id("g1"),
                yes: true
            })
        );
        assert_eq!(
            p(&["quarantine", "discard", "q1"]).unwrap().command,
            AdminCommand::Quarantine(QuarantineOp::Discard {
                graph: id("q1"),
                yes: false
            })
        );
        assert!(p(&["graphs", "list", "--yes"]).is_err());
        assert!(p(&["graphs", "show", "g1", "--yes"]).is_err());
        assert!(p(&["graphs", "list", "g1"]).is_err());
        assert!(p(&["graphs", "resume"]).is_err());
        assert!(p(&["graphs", "resume", "a", "b"]).is_err());
        assert!(p(&["quarantine", "release", "q1"]).is_err());
        assert!(p(&["graphs", "discard", "g1"]).is_err());
        assert!(p(&["graphs", "show", "/bin/sh"]).is_err());
    }
}
