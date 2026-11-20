//! Noninteractive management requires an explicit opt-in.
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
