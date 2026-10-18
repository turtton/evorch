//! Chromium sessions driven over CDP (ADR 0034), kept free of GUI types so the
//! GUI, runtime and tools can share them.

pub mod agent;
mod cdp;
mod diagnostics;
mod session;
#[cfg(test)]
mod tests;

pub use session::BrowserSession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserAction {
    Navigate(url::Url),
    Click(String),
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("browser command queue is full or the session has stopped")]
    Unavailable,
    #[error("only HTTP and HTTPS navigation is supported")]
    UrlScheme,
    #[error("browser configuration: {0}")]
    Configuration(String),
    #[error(transparent)]
    Cdp(#[from] chromiumoxide::error::CdpError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Image(#[from] image::ImageError),
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
}

#[derive(Debug, Clone)]
pub struct BrowserReport {
    pub action: String,
    pub error: Option<String>,
    pub removed: String,
    pub inserted: String,
}

pub fn validate_action(action: &BrowserAction) -> Result<(), BrowserError> {
    match action {
        BrowserAction::Navigate(url) => match url.scheme() {
            "http" | "https" => Ok(()),
            _ => Err(BrowserError::UrlScheme),
        },
        BrowserAction::Click(_) => Ok(()),
    }
}
