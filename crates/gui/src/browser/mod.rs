mod agent;
mod pane;

pub use ::browser::agent::Activity;
pub use ::browser::{BrowserAction, BrowserError, BrowserReport, BrowserSession};
pub use agent::{AgentBrowserPane, AgentBrowserWindow, AgentView, FakeAgentView, LiveAgentView};
pub use pane::{BrowserPane, BrowserWindow};

use std::collections::VecDeque;

pub trait FrameSource {
    fn poll_frame(&mut self) -> Option<egui::ColorImage>;
    fn submit(&mut self, action: BrowserAction) -> Result<(), BrowserError>;
    fn error(&self) -> Option<String>;
    fn poll_report(&mut self) -> Option<BrowserReport>;
}

impl FrameSource for BrowserSession {
    fn poll_frame(&mut self) -> Option<egui::ColorImage> {
        BrowserSession::poll_frame(self).map(|frame| {
            egui::ColorImage::from_rgba_unmultiplied(
                [frame.width() as usize, frame.height() as usize],
                frame.as_raw(),
            )
        })
    }

    fn submit(&mut self, action: BrowserAction) -> Result<(), BrowserError> {
        BrowserSession::submit(self, action)
    }

    fn error(&self) -> Option<String> {
        BrowserSession::error(self)
    }

    fn poll_report(&mut self) -> Option<BrowserReport> {
        BrowserSession::poll_report(self)
    }
}

#[derive(Default)]
pub struct FakeFrameSource {
    pub frames: VecDeque<egui::ColorImage>,
    pub actions: Vec<BrowserAction>,
    pub failure: Option<String>,
    pub reports: VecDeque<BrowserReport>,
}

impl FrameSource for FakeFrameSource {
    fn poll_frame(&mut self) -> Option<egui::ColorImage> {
        self.frames.pop_front()
    }

    fn submit(&mut self, action: BrowserAction) -> Result<(), BrowserError> {
        ::browser::validate_action(&action)?;
        self.actions.push(action);
        Ok(())
    }

    fn error(&self) -> Option<String> {
        self.failure.clone()
    }

    fn poll_report(&mut self) -> Option<BrowserReport> {
        if self.actions.is_empty() {
            None
        } else {
            self.reports.pop_front()
        }
    }
}
