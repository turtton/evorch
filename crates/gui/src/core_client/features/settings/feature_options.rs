//! Features: the modal for the four features of the Features screen (PR view, Agent web
//! tools, Agent browser, Thread cleanup).
//!
//! The switches and numbers edit a draft. Nothing is sent until Save, and one Save sends
//! every changed section in one `save_feature_settings`. The core writes the file, and
//! follows the PR view and Agent web tools at once; what it cannot follow (the browser and
//! thread cleanup) is listed by the view as waiting for a restart, which the band at the top
//! shows with the way to the Restart screen. The screen keeps no state of its own about
//! that: the band is the view's `restart_pending`, so it goes away when a value is put back.
use super::*;
use crate::core_client::features::{FeatureError, pull_requests::PullRequestIntent};
use crate::theme::{
    text::h3,
    widgets::{badge, icon_button, surface_frame},
};
use workbench_protocol::namespaces::settings::{
    BrowserFeature, FeatureSection, FeatureSettingsPatch, FeatureSettingsView, FeatureValues,
    PullRequestFeature, SaveFeatureSettings, ThreadCleanupFeature,
};

/// Days a step starts with when it is turned on after being off.
const DEFAULT_DAYS: u32 = 7;
/// Room kept at the right of a card for its switch.
const SWITCH_GUTTER: f32 = 56.0;
const MODAL_WIDTH: f32 = 720.0;
const MODAL_MAX_HEIGHT: f32 = 760.0;
const MODAL_MARGIN: f32 = SP_4 * 1.5;
const HEADER_HEIGHT: f32 = TOPBAR;
const FOOTER_HEIGHT: f32 = TOPBAR - SP_2;

/// A cleanup step as the screen edits it: a switch and a number of days.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Step {
    on: bool,
    days: u32,
}

impl Step {
    fn of(days: u32) -> Self {
        Self {
            on: days > 0,
            days: if days > 0 { days } else { DEFAULT_DAYS },
        }
    }

    /// What the file holds: `0` is off.
    fn wire(self) -> u32 {
        if self.on { self.days.max(1) } else { 0 }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct BrowserDraft {
    enabled: bool,
    executable: String,
    max_pages: u32,
    max_tabs: u32,
    coordinate_actions: bool,
}

impl BrowserDraft {
    fn of(browser: &BrowserFeature) -> Self {
        Self {
            enabled: browser.enabled,
            executable: browser.executable.clone().unwrap_or_default(),
            max_pages: browser.max_pages,
            max_tabs: browser.max_tabs,
            coordinate_actions: browser.coordinate_actions,
        }
    }

    fn wire(&self) -> BrowserFeature {
        let executable = self.executable.trim();
        BrowserFeature {
            enabled: self.enabled,
            executable: (!executable.is_empty()).then(|| executable.to_owned()),
            max_pages: self.max_pages.max(1),
            max_tabs: self.max_tabs.max(1),
            coordinate_actions: self.coordinate_actions,
        }
    }
}

/// What is being edited, and the revision of the file it was made from.
#[derive(Clone, PartialEq, Eq)]
struct Draft {
    based_on: SettingsRevision,
    pull_requests: PullRequestFeature,
    web_tools: bool,
    browser: BrowserDraft,
    archive: Step,
    delete: Step,
}

impl Draft {
    fn of(view: &FeatureSettingsView) -> Self {
        let saved = &view.saved;
        Self {
            based_on: view.revision.clone(),
            pull_requests: saved.pull_requests,
            web_tools: saved.agent_web_tools,
            browser: BrowserDraft::of(&saved.browser),
            archive: Step::of(saved.thread_cleanup.auto_archive_after_days),
            delete: Step::of(saved.thread_cleanup.archived_delete_after_days),
        }
    }

    fn cleanup(&self) -> ThreadCleanupFeature {
        ThreadCleanupFeature {
            auto_archive_after_days: self.archive.wire(),
            archived_delete_after_days: self.delete.wire(),
        }
    }

    /// The sections that differ from `saved`, as one patch.
    fn patch(&self, saved: &FeatureValues) -> FeatureSettingsPatch {
        FeatureSettingsPatch {
            pull_requests: (self.pull_requests != saved.pull_requests)
                .then_some(self.pull_requests),
            agent_web_tools: (self.web_tools != saved.agent_web_tools).then_some(self.web_tools),
            browser: (self.browser.wire() != saved.browser).then(|| self.browser.wire()),
            thread_cleanup: (self.cleanup() != saved.thread_cleanup).then(|| self.cleanup()),
        }
    }

    /// How many values differ from `saved`.
    fn changes(&self, saved: &FeatureValues) -> usize {
        let (pr, was) = (&self.pull_requests, &saved.pull_requests);
        let (browser, was_browser) = (self.browser.wire(), &saved.browser);
        let cleanup = self.cleanup();
        [
            pr.enabled != was.enabled,
            pr.auto_fix_enabled != was.auto_fix_enabled,
            pr.include_in_review != was.include_in_review,
            pr.pending_poll_seconds != was.pending_poll_seconds,
            pr.stable_poll_seconds != was.stable_poll_seconds,
            pr.timeout_seconds != was.timeout_seconds,
            self.web_tools != saved.agent_web_tools,
            browser.enabled != was_browser.enabled,
            browser.executable != was_browser.executable,
            browser.max_pages != was_browser.max_pages,
            browser.max_tabs != was_browser.max_tabs,
            browser.coordinate_actions != was_browser.coordinate_actions,
            cleanup.auto_archive_after_days != saved.thread_cleanup.auto_archive_after_days,
            cleanup.archived_delete_after_days != saved.thread_cleanup.archived_delete_after_days,
        ]
        .into_iter()
        .filter(|changed| *changed)
        .count()
    }
}

/// The Features modal's state.
pub(super) struct FeatureOptions {
    open: bool,
    stale: bool,
    in_flight: bool,
    view: Option<FeatureSettingsView>,
    draft: Option<Draft>,
    /// The running save: the draft is not edited and not saved again meanwhile.
    saving: Option<OperationId>,
    status: String,
    polling_open: bool,
    confirm_close: bool,
    /// The PR tab is to read its threads again (what tracking is on changed).
    refresh_pull_requests: bool,
}

impl Default for FeatureOptions {
    fn default() -> Self {
        Self {
            open: false,
            stale: true,
            in_flight: false,
            view: None,
            draft: None,
            saving: None,
            status: String::new(),
            polling_open: false,
            confirm_close: false,
            refresh_pull_requests: false,
        }
    }
}

impl FeatureOptions {
    fn dirty(&self) -> bool {
        match (&self.draft, &self.view) {
            (Some(draft), Some(view)) => !draft.patch(&view.saved).is_empty(),
            _ => false,
        }
    }

    /// A reconnect: what was read is read again; a save in flight is asked for by the
    /// feature's operation check, not sent again.
    pub(super) fn reset(&mut self) {
        self.stale = true;
        self.in_flight = false;
    }

    pub(super) fn changed_elsewhere(&mut self) {
        self.stale = true;
    }

    fn view_arrived(&mut self, result: Result<QueryReply, FeatureError>) {
        self.in_flight = false;
        let view = match result {
            Ok(QueryReply::Settings(SettingsReply::FeatureSettings(view))) => *view,
            Ok(_) => return,
            Err(error) => {
                self.status = error.message;
                return;
            }
        };
        // What tracking is now on in the running core decides what the PR tab shows.
        if self.view.as_ref().is_some_and(|old| {
            old.running.pull_requests.enabled != view.running.pull_requests.enabled
        }) {
            self.refresh_pull_requests = true;
        }
        let reseed = match &self.draft {
            None => true,
            Some(draft) => {
                self.saving.is_none()
                    && draft.based_on != view.revision
                    && self
                        .view
                        .as_ref()
                        .is_some_and(|old| draft.patch(&old.saved).is_empty())
            }
        };
        if reseed {
            self.draft = Some(Draft::of(&view));
        } else if self
            .draft
            .as_ref()
            .is_some_and(|d| d.based_on != view.revision)
        {
            self.status =
                "The settings were changed elsewhere. Discard your edits to see them; saving them is refused."
                    .into();
        }
        self.view = Some(view);
    }
}

impl SettingsFeature {
    /// Opens the Features modal, with what the core holds read again.
    pub fn open_features(&mut self) {
        let features = &mut self.features;
        features.open = true;
        features.stale = true;
        features.confirm_close = false;
        features.status.clear();
        features.draft = None;
    }

    pub fn features_open(&self) -> bool {
        self.features.open
    }

    fn close_features(&mut self) {
        self.features.open = false;
        self.features.confirm_close = false;
        self.features.draft = None;
    }

    /// The ends of a save that did not reach the core, or whose outcome was lost.
    pub(super) fn feature_save_dropped(&mut self, id: &OperationId, note: Option<String>) {
        if self.features.saving.as_ref() == Some(id) {
            self.features.saving = None;
            self.features.stale = true;
            if let Some(note) = note {
                self.features.status = note;
            }
        }
    }

    /// A save of the Features sections ended.
    pub(super) fn feature_save_ended(
        &mut self,
        id: &OperationId,
        state: OperationState,
        message: Option<String>,
    ) {
        if self.features.saving.as_ref() != Some(id) {
            return;
        }
        self.features.saving = None;
        self.features.stale = true;
        match state {
            OperationState::Ready => {
                self.features.draft = None;
                self.features.refresh_pull_requests = true;
                self.features.status = message.unwrap_or_default();
            }
            OperationState::Failed { code } => {
                self.features.status = match message {
                    Some(message) if code != ErrorCode::StaleRevision => {
                        format!("{} {message}", failure_label(code))
                    }
                    _ => failure_label(code).to_owned(),
                };
            }
            OperationState::Cancelled { .. } => {
                self.features.status =
                    "The save did not run because of a restart or shutdown.".into();
            }
            OperationState::Pending => {}
        }
    }

    pub(super) fn features_reply(&mut self, result: Result<QueryReply, FeatureError>) {
        self.features.view_arrived(result);
    }

    pub(super) fn features_maintain(&mut self, ctx: &mut FeatureContext<'_>) {
        if std::mem::take(&mut self.features.refresh_pull_requests) {
            ctx.pull_request(PullRequestIntent::Refresh);
        }
        if self.features.open && self.features.stale && !self.features.in_flight {
            self.features.stale = false;
            self.features.in_flight = true;
            let tag = self.tag(Pending::FeatureView);
            ctx.query(tag, Query::Settings(SettingsQuery::GetFeatureSettings {}));
        }
    }

    pub(super) fn render_features(
        &mut self,
        egui_ctx: &egui::Context,
        ctx: &mut FeatureContext<'_>,
    ) {
        if !self.features.open {
            return;
        }
        let busy = self.features.saving.is_some();
        let action = draw_modal(egui_ctx, &mut self.features, busy);
        match action {
            Some(Action::Close) => {
                if self.features.confirm_close {
                    self.features.confirm_close = false;
                } else if self.features.dirty() {
                    self.features.confirm_close = true;
                } else {
                    self.close_features();
                }
            }
            Some(Action::DiscardAndClose) => self.close_features(),
            Some(Action::KeepEditing) => self.features.confirm_close = false,
            Some(Action::Discard) => {
                self.features.draft = self.features.view.as_ref().map(Draft::of);
                self.features.status.clear();
            }
            Some(Action::Restart) => {
                self.close_features();
                ctx.settings(SettingsIntent::RestartCore);
            }
            Some(Action::Save) => self.save_features(ctx),
            None => {}
        }
    }

    fn save_features(&mut self, ctx: &mut FeatureContext<'_>) {
        let (Some(draft), Some(view)) = (&self.features.draft, &self.features.view) else {
            return;
        };
        let changes = draft.patch(&view.saved);
        if changes.is_empty() || self.features.saving.is_some() {
            return;
        }
        let operation_id = new_operation();
        let expected_revision = draft.based_on.clone();
        self.features.saving = Some(operation_id.clone());
        self.features.status.clear();
        self.send(
            ctx,
            SettingsCommand::SaveFeatureSettings(SaveFeatureSettings {
                operation_id,
                expected_revision,
                changes,
            }),
        );
    }
}

enum Action {
    Close,
    DiscardAndClose,
    KeepEditing,
    Discard,
    Save,
    Restart,
}

/// The sections the band names, in the words of the cards.
fn section_name(section: FeatureSection) -> &'static str {
    match section {
        FeatureSection::PullRequests => "PR view",
        FeatureSection::AgentWebTools => "Agent web tools",
        FeatureSection::Browser => "Agent browser",
        FeatureSection::ThreadCleanup => "Thread cleanup",
    }
}

fn joined(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// A switch: `on` is flipped by a click. Its accessible name is `label`.
fn switch(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(32.0, 18.0), egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let enabled = ui.is_enabled();
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, *on, label)
    });
    if ui.is_rect_visible(rect) {
        let p = palette();
        let track = match (*on, enabled) {
            (true, true) => p.ACCENT,
            (true, false) => p.ACCENT.gamma_multiply(0.5),
            (false, _) => p.INPUT,
        };
        let knob = if *on { p.ACCENT_FG } else { p.TEXT_MUTED };
        ui.painter().rect_filled(rect, 9.0, track);
        let x = if *on {
            rect.right() - 9.0
        } else {
            rect.left() + 9.0
        };
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), 7.0, knob);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn chip(ui: &mut egui::Ui, restart: bool) {
    let p = palette();
    if restart {
        badge(
            ui,
            icons::with_icon(icons::ARROWS_CLOCKWISE, "Restart to apply"),
            p.WARNING_FG,
            p.WARNING_SURFACE,
        );
    } else {
        badge(
            ui,
            icons::with_icon(icons::CHECK, "Applies now"),
            p.SUCCESS,
            p.SUCCESS.gamma_multiply(0.18),
        );
    }
}

/// One card: the icon, the title with its chip, the switch at the right, a description and
/// what the card shows under them.
fn card(
    ui: &mut egui::Ui,
    (icon, title): (&str, &str),
    restart: bool,
    description: &str,
    on: Option<&mut bool>,
    editable: bool,
    body: impl FnOnce(&mut egui::Ui),
) {
    soft_frame(palette().SURFACE).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(icon)
                    .size(FONT_ICON + 2.0)
                    .color(palette().INFO),
            );
            ui.vertical(|ui| {
                ui.set_max_width(ui.available_width() - SWITCH_GUTTER);
                ui.horizontal(|ui| {
                    ui.label(medium(title).size(FONT_H4));
                    chip(ui, restart);
                });
                ui.label(muted(description));
            });
            if let Some(on) = on {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(editable, |ui| switch(ui, on, title));
                });
            }
        });
        ui.add_enabled_ui(editable, body);
    });
}

/// A labelled switch with a line of help under it, inside a card.
fn option_row(ui: &mut egui::Ui, label: &str, help: &str, on: &mut bool) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_max_width(ui.available_width() - SWITCH_GUTTER);
            ui.label(label);
            ui.label(muted(help));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            switch(ui, on, label);
        });
    });
}

fn number<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    value: &mut T,
    range: std::ops::RangeInclusive<T>,
    label: &str,
    suffix: &str,
) {
    let response = ui.add(egui::DragValue::new(value).range(range).suffix(suffix));
    accessible(&response, label);
}

fn step_row(ui: &mut egui::Ui, step: &mut Step, before: &str, after: &str, label: &str) {
    ui.horizontal(|ui| {
        switch(ui, &mut step.on, label);
        ui.label(before);
        ui.add_enabled_ui(step.on, |ui| {
            number(ui, &mut step.days, 1..=36_500, label, "")
        });
        ui.label(after);
    });
}

fn pull_request_card(
    ui: &mut egui::Ui,
    options: &mut FeatureOptions,
    restart: bool,
    editable: bool,
) {
    let Some(draft) = &mut options.draft else {
        return;
    };
    let polling_open = &mut options.polling_open;
    let pr = &mut draft.pull_requests;
    let mut enabled = pr.enabled;
    card(
        ui,
        (icons::LINK, "Pull request view"),
        restart,
        "Track the CI and review state of the pull request linked to a thread and show it above the conversation. Uses the GitHub CLI (gh).",
        Some(&mut enabled),
        editable,
        |ui| {
            if !pr.enabled {
                return;
            }
            ui.add_space(SP_1);
            option_row(
                ui,
                "Auto-fix notifications",
                "The global switch for automatic fix notifications. Each pull request still needs its own opt-in.",
                &mut pr.auto_fix_enabled,
            );
            option_row(
                ui,
                "Use PR state in escalation review",
                "Include the fetched pull request state when a permission escalation is reviewed.",
                &mut pr.include_in_review,
            );
            let summary = format!(
                "pending {}s \u{b7} stable {}s \u{b7} timeout {}s",
                pr.pending_poll_seconds, pr.stable_poll_seconds, pr.timeout_seconds
            );
            let arrow = if *polling_open {
                icons::CARET_DOWN
            } else {
                icons::CARET_RIGHT
            };
            let header =
                ui.add(egui::Button::new(format!("{arrow} Polling   {summary}")).frame(false));
            accessible(&header, "Polling");
            if header.clicked() {
                *polling_open = !*polling_open;
            }
            if *polling_open {
                ui.horizontal(|ui| {
                    ui.label("Check pending pull requests every");
                    number(
                        ui,
                        &mut pr.pending_poll_seconds,
                        5..=3_600,
                        "Pending poll seconds",
                        " s",
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Check stable pull requests every");
                    number(
                        ui,
                        &mut pr.stable_poll_seconds,
                        30..=86_400,
                        "Stable poll seconds",
                        " s",
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Give up one fetch after");
                    number(
                        ui,
                        &mut pr.timeout_seconds,
                        1..=300,
                        "Fetch timeout seconds",
                        " s",
                    );
                });
            }
        },
    );
    pr.enabled = enabled;
}

fn web_tools_card(ui: &mut egui::Ui, options: &mut FeatureOptions, restart: bool, editable: bool) {
    let running = options
        .view
        .as_ref()
        .map(|view| view.running.agent_web_tools);
    let Some(draft) = &mut options.draft else {
        return;
    };
    card(
        ui,
        (icons::GLOBE, "Agent web tools"),
        restart,
        "Lets roles that are allowed use web search and fetch. Each call still follows the tool policy. A run that already started keeps the tools it was sent.",
        Some(&mut draft.web_tools),
        editable,
        |ui| {
            if let Some(running) = running {
                ui.label(muted(format!(
                    "Running core: {}",
                    if running { "on" } else { "off" }
                )));
            }
        },
    );
}

fn browser_card(ui: &mut egui::Ui, options: &mut FeatureOptions, restart: bool, editable: bool) {
    let Some(draft) = &mut options.draft else {
        return;
    };
    let browser = &mut draft.browser;
    let mut enabled = browser.enabled;
    card(
        ui,
        (icons::EYE, "Agent browser"),
        restart,
        "Tools that let the agent operate a browser. They are registered when a Chromium or Chrome executable is found.",
        Some(&mut enabled),
        editable,
        |ui| {
            ui.add_enabled_ui(browser.enabled, |ui| {
                ui.label(muted("Browser executable"));
                let executable = ui.add(
                    egui::TextEdit::singleline(&mut browser.executable)
                        .hint_text("Auto-detect (CHROME \u{2192} PATH)")
                        .desired_width(f32::INFINITY),
                );
                accessible(&executable, "Browser executable");
                ui.horizontal(|ui| {
                    ui.label("Open tabs, all runs");
                    number(ui, &mut browser.max_pages, 1..=1024, "Open tabs, all runs", "");
                    ui.label("Tabs per run");
                    number(ui, &mut browser.max_tabs, 1..=1024, "Tabs per run", "");
                });
                option_row(
                    ui,
                    "Coordinate clicks",
                    "Also register browser_click_at, which clicks at coordinates read from a screenshot.",
                    &mut browser.coordinate_actions,
                );
            });
        },
    );
    browser.enabled = enabled;
}

fn cleanup_card(ui: &mut egui::Ui, options: &mut FeatureOptions, restart: bool, editable: bool) {
    let Some(draft) = &mut options.draft else {
        return;
    };
    let (archive, delete) = (&mut draft.archive, &mut draft.delete);
    card(
        ui,
        (icons::ARCHIVE, "Thread cleanup"),
        restart,
        "Archive threads that are no longer used, and delete archived threads after a while. Pinned threads, evaluation samples and running threads are left alone.",
        None,
        editable,
        |ui| {
            step_row(
                ui,
                archive,
                "Archive threads inactive for",
                "days",
                "Archive inactive threads",
            );
            step_row(
                ui,
                delete,
                "Delete archived threads after",
                "days",
                "Delete archived threads",
            );
            ui.label(muted(icons::with_icon(
                icons::INFO,
                "Deletion is confirmed when the core starts, and while it runs at a moment when nothing else is working on the thread. Turning a step off writes 0 to the settings file.",
            )));
        },
    );
}

fn band(ui: &mut egui::Ui, view: &FeatureSettingsView, dirty: bool) -> bool {
    if view.restart_pending.is_empty() {
        return false;
    }
    let names: Vec<_> = view
        .restart_pending
        .iter()
        .map(|s| section_name(*s))
        .collect();
    let mut restart = false;
    soft_frame(palette().WARNING_SURFACE).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(icons::ARROWS_CLOCKWISE).color(palette().WARNING_FG));
            ui.colored_label(
                palette().WARNING_FG,
                format!(
                    "{} saved. A core restart applies the saved values; work in progress stops at a safe point and continues in the new core.",
                    joined(&names)
                ),
            );
            let button = ui.add_enabled(
                !dirty,
                egui::Button::new(icons::with_icon(icons::ARROWS_CLOCKWISE, "Restart core\u{2026}")),
            );
            accessible(&button, "Restart core");
            let button = if dirty {
                button.on_disabled_hover_text("Save or discard your changes first")
            } else {
                button
            };
            restart = button.clicked();
        });
    });
    restart
}

fn draw_modal(
    egui_ctx: &egui::Context,
    options: &mut FeatureOptions,
    busy: bool,
) -> Option<Action> {
    let mut action = None;
    let window = egui_ctx.viewport_rect().size();
    let size = egui::vec2(
        (window.x - MODAL_MARGIN * 2.0).clamp(0.0, MODAL_WIDTH),
        (window.y - MODAL_MARGIN * 2.0).clamp(0.0, MODAL_MAX_HEIGHT),
    );
    let modal = egui::Modal::new(egui::Id::new("feature-options"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED).inner_margin(egui::Margin::ZERO))
        .show(egui_ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            let (frame, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            let header = egui::Rect::from_min_size(frame.min, egui::vec2(size.x, HEADER_HEIGHT));
            let footer = egui::Rect::from_min_max(
                egui::pos2(frame.min.x, (frame.max.y - FOOTER_HEIGHT).max(header.max.y)),
                frame.max,
            );
            let body = egui::Rect::from_min_max(
                egui::pos2(frame.min.x, header.max.y),
                egui::pos2(frame.max.x, footer.min.y),
            );
            let line = egui::Stroke::new(1.0, palette().BORDER);
            ui.painter().hline(frame.x_range(), header.max.y, line);
            ui.painter().hline(frame.x_range(), footer.min.y, line);
            draw_header(ui, header, busy, &mut action);
            draw_body(ui, body, options, busy, &mut action);
            draw_footer(ui, footer, options, busy, &mut action);
        });
    if !busy && modal.should_close() && action.is_none() {
        action = Some(Action::Close);
    }
    action
}

fn region(ui: &mut egui::Ui, rect: egui::Rect, layout: egui::Layout) -> egui::Ui {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
    child
}

fn draw_header(ui: &mut egui::Ui, rect: egui::Rect, busy: bool, action: &mut Option<Action>) {
    let mut header = region(
        ui,
        rect.shrink2(egui::vec2(SP_4, 0.0)),
        egui::Layout::right_to_left(egui::Align::Center),
    );
    header.add_enabled_ui(!busy, |ui| {
        if icon_button(ui, icons::X, "Close").clicked() {
            *action = Some(Action::Close);
        }
    });
    header.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.label(egui::RichText::new(icons::SLIDERS_HORIZONTAL).size(FONT_ICON));
        ui.label(h3("Features"));
        ui.add(
            egui::Label::new(muted(
                "On or off per feature. Saved to the settings file (config.toml).",
            ))
            .truncate(),
        );
    });
}

fn draw_body(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    options: &mut FeatureOptions,
    busy: bool,
    action: &mut Option<Action>,
) {
    let mut body = region(
        ui,
        rect.shrink2(egui::vec2(SP_4, SP_3)),
        egui::Layout::top_down(egui::Align::Min),
    );
    let Some(view) = options.view.clone() else {
        body.label("Loading settings\u{2026}");
        if !options.status.is_empty() {
            body.label(&options.status);
        }
        return;
    };
    if let Some(problem) = view.problem {
        body.colored_label(palette().WARNING_FG, problem_label(problem));
        body.label(muted(
            "Fix the settings file in the Config file tab, then open Features again.",
        ));
        return;
    }
    let dirty = options.dirty();
    egui::ScrollArea::vertical()
        .id_salt("feature-options-body")
        .auto_shrink([false, false])
        .show(&mut body, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                if band(ui, &view, dirty) {
                    *action = Some(Action::Restart);
                }
                let pending = |section| view.restart_pending.contains(&section);
                pull_request_card(ui, options, pending(FeatureSection::PullRequests), !busy);
                web_tools_card(ui, options, pending(FeatureSection::AgentWebTools), !busy);
                browser_card(ui, options, true, !busy);
                cleanup_card(ui, options, true, !busy);
            });
        });
}

fn draw_footer(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    options: &FeatureOptions,
    busy: bool,
    action: &mut Option<Action>,
) {
    let mut footer = region(
        ui,
        rect.shrink2(egui::vec2(SP_4, 0.0)),
        egui::Layout::right_to_left(egui::Align::Center),
    );
    if options.confirm_close {
        if footer.button("Keep editing").clicked() {
            *action = Some(Action::KeepEditing);
        }
        if footer.button("Discard and close").clicked() {
            *action = Some(Action::DiscardAndClose);
        }
        footer.colored_label(palette().WARNING_FG, "Discard unsaved changes and close?");
        return;
    }
    let changes = match (&options.draft, &options.view) {
        (Some(draft), Some(view)) => draft.changes(&view.saved),
        _ => 0,
    };
    footer.add_enabled_ui(changes > 0 && !busy, |ui| {
        let save = primary_button(ui, icons::with_icon(icons::FLOPPY_DISK, "Save"));
        accessible(&save, "Save");
        if save.clicked() {
            *action = Some(Action::Save);
        }
        let discard = ui.button(icons::with_icon(icons::ARROW_COUNTER_CLOCKWISE, "Discard"));
        accessible(&discard, "Discard");
        if discard.clicked() {
            *action = Some(Action::Discard);
        }
    });
    footer.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        if busy {
            ui.label(muted("Saving\u{2026}"));
        } else if !options.status.is_empty() {
            ui.add(egui::Label::new(options.status.as_str()).truncate())
                .on_hover_text(&options.status);
        } else if changes > 0 {
            ui.label(
                egui::RichText::new(icons::with_icon(
                    icons::INFO,
                    format!(
                        "{changes} unsaved change{}",
                        if changes == 1 { "" } else { "s" }
                    ),
                ))
                .color(palette().WARNING_FG),
            );
        }
    });
}
