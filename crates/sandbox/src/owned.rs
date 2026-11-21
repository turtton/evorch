//! Additive non-PTY ownership API (D-3a2 B2a). Existing raw owners remain until B2b.
//! Linux uses an authenticated process guard, regardless of ledger presence.
mod command;
pub use command::OwnedCommand;
#[cfg(target_os = "linux")]
pub use process_guard::GuardRuntime;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{OwnedChild, OwnedGroup, OwnedSession};

#[cfg(not(target_os = "linux"))]
mod unix;
#[cfg(not(target_os = "linux"))]
pub use unix::{OwnedChild, OwnedGroup, OwnedSession};

/// Bind the immutable bundle beside this running application. Call before
/// config, logging or async runtime creation; an inherited env cannot override it.
pub fn bind_installed(role: &str) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    process_guard::GuardRuntime::bind_installed(role).map(drop)?;
    #[cfg(not(target_os = "linux"))]
    let _ = role;
    Ok(())
}
