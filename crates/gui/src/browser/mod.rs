mod cdp;
mod diagnostics;
mod pane;
mod session;
#[cfg(test)]
mod tests;

pub use pane::{BrowserPane, BrowserWindow};
pub use session::ChromiumSource;

use std::collections::VecDeque;

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

pub trait FrameSource {
    fn poll_frame(&mut self) -> Option<egui::ColorImage>;
    fn submit(&mut self, action: BrowserAction) -> Result<(), BrowserError>;
    fn error(&self) -> Option<String>;
}

#[derive(Default)]
pub struct FakeFrameSource {
    pub frames: VecDeque<egui::ColorImage>,
    pub actions: Vec<BrowserAction>,
    pub failure: Option<String>,
}

impl FrameSource for FakeFrameSource {
    fn poll_frame(&mut self) -> Option<egui::ColorImage> {
        self.frames.pop_front()
    }

    fn submit(&mut self, action: BrowserAction) -> Result<(), BrowserError> {
        validate_action(&action)?;
        self.actions.push(action);
        Ok(())
    }

    fn error(&self) -> Option<String> {
        self.failure.clone()
    }
}

fn validate_action(action: &BrowserAction) -> Result<(), BrowserError> {
    match action {
        BrowserAction::Navigate(url) => match url.scheme() {
            "http" | "https" => Ok(()),
            _ => Err(BrowserError::UrlScheme),
        },
        BrowserAction::Click(_) => Ok(()),
    }
}
