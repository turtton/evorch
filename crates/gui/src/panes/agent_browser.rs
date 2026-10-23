//! The agent browser tab: a read-only view of the pages agents open with the
//! `browser_*` tools (ADR 0034). The workbench decides which run the tab
//! shows; builds with the `browser` feature supply the drawing surface.

const OPEN_REQUEST: &str = "agent-browser-open-request";
const AVAILABLE: &str = "agent-browser-available";

/// A run the tab can switch to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRun {
    pub run_id: String,
    pub label: String,
}

/// What the tab shows this frame.
#[derive(Debug, Clone, Copy)]
pub struct BrowserTabView<'a> {
    /// The thread's runs that opened a page since the GUI started, oldest first.
    pub runs: &'a [BrowserRun],
    /// The run to show. Its page may have closed since.
    pub selected: Option<&'a str>,
}

/// Draws the agent browser tab over the runtime's agent pages.
pub trait AgentBrowserSurface {
    /// Runs with an open agent page.
    fn pages(&mut self, ctx: &egui::Context) -> Vec<String>;
    /// Draws `view` and returns the run the user picked, if any.
    fn render(&mut self, ui: &mut egui::Ui, view: BrowserTabView<'_>) -> Option<String>;
    /// The tab went out of sight: stop the screencast.
    fn stop(&mut self, ctx: &egui::Context);
}

/// Asks the workbench to show `run_id`'s page in the agent browser tab.
pub fn request_open(ctx: &egui::Context, run_id: &str) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::new(OPEN_REQUEST), run_id.to_owned()));
    ctx.request_repaint();
}

pub fn take_open_request(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|data| data.remove_temp::<String>(egui::Id::new(OPEN_REQUEST)))
}

/// Records whether this frame has an agent browser to show, so tool cards
/// offer the View button only when it can open something.
pub fn set_available(ctx: &egui::Context, available: bool) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::new(AVAILABLE), available));
}

pub fn available(ctx: &egui::Context) -> bool {
    ctx.data(|data| data.get_temp::<bool>(egui::Id::new(AVAILABLE)))
        .unwrap_or(false)
}

/// Whether `tool_name` drives the agent browser.
pub fn is_browser_tool(tool_name: &str) -> bool {
    tool_name.starts_with("browser_")
}
