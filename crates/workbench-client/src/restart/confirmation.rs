//! Restart and Interrupt are irreversible. A command for either can only be
//! built from a [`Confirmed`], and a [`Confirmed`] only comes from
//! [`confirm`]: an explicit flag, or an approval a person gave at a terminal.
//! Nothing else (a default, an environment variable, a pipe) produces one.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    ExplicitFlag,
    InteractiveApproval,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("explicit confirmation is required")]
pub struct ConfirmationRequired;
pub fn require_confirmation(
    terminal: bool,
    explicit_flag: bool,
    interactive_approved: bool,
) -> Result<Confirmation, ConfirmationRequired> {
    if explicit_flag {
        Ok(Confirmation::ExplicitFlag)
    } else if terminal && interactive_approved {
        Ok(Confirmation::InteractiveApproval)
    } else {
        Err(ConfirmationRequired)
    }
}

/// Proof that the person confirmed. The field is private: it cannot be built
/// outside [`confirm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Confirmed {
    method: Confirmation,
}
impl Confirmed {
    pub fn method(self) -> Confirmation {
        self.method
    }
}
/// [`require_confirmation`], keeping the proof.
pub fn confirm(
    terminal: bool,
    explicit_flag: bool,
    interactive_approved: bool,
) -> Result<Confirmed, ConfirmationRequired> {
    require_confirmation(terminal, explicit_flag, interactive_approved)
        .map(|method| Confirmed { method })
}
