//! Entry classification has no filesystem, executor or database side effects.
use super::HandoffEntry;
use std::{ffi::OsString, io};

/// Mode is determined by argv, never inferred from absence of a registered proof.
pub enum BootMode {
    Cold,
    PlannedHandoff(PlannedHandoff),
}
pub struct PlannedHandoff {
    lifetime: i32,
    ticket: i32,
}
pub struct EntryOptions {
    pub mode: BootMode,
    pub describe: bool,
    /// Remaining effective arguments for the ordinary option parser.
    pub args: Vec<OsString>,
}
impl EntryOptions {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> io::Result<Self> {
        let mut args = args.into_iter();
        let mut remaining = Vec::new();
        let (mut lifetime, mut ticket) = (None, None);
        let mut launch = false;
        let mut describe = false;
        while let Some(arg) = args.next() {
            let slot = if arg == "--handoff-lifetime-fd" {
                Some(&mut lifetime)
            } else if arg == "--handoff-ticket-fd" {
                Some(&mut ticket)
            } else {
                None
            };
            if let Some(slot) = slot {
                if slot.is_some() {
                    return Err(invalid());
                }
                let fd = args
                    .next()
                    .and_then(|s| s.to_str().and_then(|s| s.parse::<i32>().ok()))
                    .filter(|fd| *fd >= 3)
                    .ok_or_else(invalid)?;
                *slot = Some(fd);
            } else if arg.as_encoded_bytes().starts_with(b"--handoff-") {
                return Err(invalid());
            } else if arg == "--describe-handoff" {
                if describe {
                    return Err(invalid());
                }
                describe = true;
            } else {
                if arg == "--launch-fd" || arg.as_encoded_bytes().starts_with(b"--launch-fd=") {
                    launch = true;
                }
                // Values cannot be interpreted as handoff options. Preserve them
                // for the final parser, but still reject incomplete options.
                if arg == "--config-dir" || arg == "--launch-fd" {
                    remaining.push(arg);
                    remaining.push(
                        args.next()
                            .filter(|v| !v.as_encoded_bytes().starts_with(b"--"))
                            .ok_or_else(invalid)?,
                    );
                } else {
                    remaining.push(arg);
                }
            }
        }
        let mode = match (lifetime, ticket) {
            (None, None) => BootMode::Cold,
            (Some(lifetime), Some(ticket)) if lifetime != ticket && !launch && !describe => {
                BootMode::PlannedHandoff(PlannedHandoff { lifetime, ticket })
            }
            _ => return Err(invalid()),
        };
        Ok(Self {
            mode,
            describe,
            args: remaining,
        })
    }
}
impl PlannedHandoff {
    /// # Safety
    /// Must be called at single-threaded process entry, with exclusive ownership
    /// of the OS-provided FDs. Errors are fatal, never permission for Cold.
    pub unsafe fn inspect(self) -> io::Result<HandoffEntry> {
        unsafe { HandoffEntry::inspect(self.lifetime, self.ticket) }
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid handoff entry arguments",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> io::Result<EntryOptions> {
        EntryOptions::parse(args.iter().map(OsString::from))
    }
    #[test]
    fn argv_fixes_mode_without_fallback() {
        assert!(matches!(
            parse(&["--foreground"]).unwrap().mode,
            BootMode::Cold
        ));
        assert!(parse(&["--describe-handoff"]).unwrap().describe);
        assert!(matches!(
            parse(&["--handoff-lifetime-fd", "3", "--handoff-ticket-fd", "4"])
                .unwrap()
                .mode,
            BootMode::PlannedHandoff(_)
        ));
        for args in [
            vec!["--handoff-lifetime-fd", "3"],
            vec!["--handoff-ticket-fd", "3"],
            vec!["--handoff-lifetime-fd", "3", "--handoff-ticket-fd", "3"],
            vec!["--handoff-lifetime-fd", "2", "--handoff-ticket-fd", "4"],
            vec!["--handoff-lifetime-fd=3"],
            vec!["--handoff-unknown"],
            vec![
                "--handoff-lifetime-fd",
                "3",
                "--handoff-lifetime-fd",
                "4",
                "--handoff-ticket-fd",
                "5",
            ],
            vec![
                "--launch-fd",
                "7",
                "--handoff-lifetime-fd",
                "3",
                "--handoff-ticket-fd",
                "4",
            ],
            vec![
                "--describe-handoff",
                "--handoff-lifetime-fd",
                "3",
                "--handoff-ticket-fd",
                "4",
            ],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }
}
