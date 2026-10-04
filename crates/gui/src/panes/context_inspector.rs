//! Context tab: the base context orchestrator and worker runs receive.
//!
//! Preview composes a role's context from the live runtime without starting a run, Run reads a
//! run's last persisted checkpoint, and Compare lines two previews up section by section.
//! Runtime work happens in `update`; `render` only records what the next frame should load.

use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use runtime::base_context::{BaseContextReport, RunContextView};

use crate::model::commands::{CommandSink, ContextPreviewReceiver};
use crate::model::context_inspector::{BudgetSegment, PreviewSpec, compact_tokens};
use crate::theme::text::muted;
use crate::theme::tokens::palette;

#[path = "context_inspector/body.rs"]
mod body;
#[path = "context_inspector/controls.rs"]
mod controls;

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const UNAVAILABLE: &str = "Context preview needs a live runtime.";

/// The views offered by the tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InspectorMode {
    #[default]
    Preview,
    Run,
    Compare,
}

impl InspectorMode {
    const ALL: [Self; 3] = [Self::Preview, Self::Run, Self::Compare];

    const fn label(self) -> &'static str {
        match self {
            Self::Preview => "Preview",
            Self::Run => "Run",
            Self::Compare => "Compare roles",
        }
    }
}

/// What the detail column shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Selection {
    #[default]
    None,
    Section(usize),
    Tool(usize),
    Message(usize),
}

/// One preview the tab wants, with the request in flight and the last answer.
#[derive(Default)]
struct PreviewSlot {
    spec: Option<PreviewSpec>,
    pending: Option<(PreviewSpec, ContextPreviewReceiver)>,
    result: Option<(PreviewSpec, Result<BaseContextReport, String>)>,
}

impl PreviewSlot {
    fn report(&self) -> Option<&Result<BaseContextReport, String>> {
        self.result.as_ref().map(|(_, result)| result)
    }

    fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Collect a finished preview and start the wanted one when it changed.
    fn update(&mut self, ctx: &egui::Context, sink: &dyn CommandSink, project: Option<&str>) {
        if let Some((spec, receiver)) = &self.pending {
            match receiver.try_recv() {
                Ok(result) => {
                    self.result = Some((spec.clone(), result));
                    self.pending = None;
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(POLL_INTERVAL),
                Err(TryRecvError::Disconnected) => {
                    self.result = Some((spec.clone(), Err("Context preview was dropped".into())));
                    self.pending = None;
                }
            }
        }
        let Some(wanted) = &self.spec else {
            return;
        };
        let current = self
            .pending
            .as_ref()
            .map(|(spec, _)| spec)
            .or(self.result.as_ref().map(|(spec, _)| spec));
        if current == Some(wanted) {
            return;
        }
        match sink.preview_base_context(wanted.request(), project) {
            Some(receiver) => {
                self.pending = Some((wanted.clone(), receiver));
                ctx.request_repaint_after(POLL_INTERVAL);
            }
            None => self.result = Some((wanted.clone(), Err(UNAVAILABLE.into()))),
        }
    }

    fn invalidate(&mut self) {
        self.result = None;
    }
}

/// A run the Run view can open, with a short label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunChoice {
    pub run_id: String,
    pub label: String,
}

/// Provider-reported numbers for the selected run, shown beside the estimates.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunActuals {
    pub latest_input: Option<u64>,
    pub latest_cache_read: Option<u64>,
    pub estimate: Option<event_bus::ContextComposition>,
}

/// State of the Context tab.
pub struct ContextInspectorPane {
    mode: InspectorMode,
    spec: PreviewSpec,
    left: PreviewSpec,
    right: PreviewSpec,
    preview: PreviewSlot,
    compare_left: PreviewSlot,
    compare_right: PreviewSlot,
    run: String,
    wanted_run: Option<String>,
    run_view: Option<(String, Result<Option<RunContextView>, String>)>,
    selection: Selection,
    raw: bool,
}

impl Default for ContextInspectorPane {
    fn default() -> Self {
        Self {
            mode: InspectorMode::Preview,
            spec: PreviewSpec::new(runtime::Role::Orchestrator),
            left: PreviewSpec::new(runtime::Role::Orchestrator),
            right: PreviewSpec::new(runtime::Role::Worker),
            preview: PreviewSlot::default(),
            compare_left: PreviewSlot::default(),
            compare_right: PreviewSlot::default(),
            run: String::new(),
            wanted_run: None,
            run_view: None,
            selection: Selection::None,
            raw: false,
        }
    }
}

impl ContextInspectorPane {
    /// Show `mode`, focusing `run` in the Run view when given.
    pub fn show(&mut self, mode: InspectorMode, run: Option<String>) {
        self.mode = mode;
        if let Some(run) = run {
            self.run = run;
            self.run_view = None;
        }
        self.selection = Selection::None;
    }

    /// Per-frame bookkeeping: collect previews and load what the last render asked for.
    pub fn update(&mut self, ctx: &egui::Context, sink: &dyn CommandSink, project: Option<&str>) {
        for slot in [
            &mut self.preview,
            &mut self.compare_left,
            &mut self.compare_right,
        ] {
            slot.update(ctx, sink, project);
        }
        if let Some(run) = self.wanted_run.take() {
            self.run_view = Some((run.clone(), sink.run_context_view(&run)));
        }
    }

    /// Renders the tab. `runs` lists runs for the Run view, newest relevant first.
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        runs: &[RunChoice],
        actuals: impl Fn(&str) -> RunActuals,
    ) {
        let refresh = self.toolbar(ui, runs);
        ui.separator();
        match self.mode {
            InspectorMode::Preview => {
                if refresh {
                    self.preview.invalidate();
                }
                self.preview.spec = Some(self.spec.clone());
                self.preview_body(ui);
            }
            InspectorMode::Run => {
                if self.run.is_empty()
                    && let Some(first) = runs.first()
                {
                    self.run = first.run_id.clone();
                }
                let stale = self
                    .run_view
                    .as_ref()
                    .is_none_or(|(run, _)| *run != self.run);
                if !self.run.is_empty() && (refresh || stale) {
                    self.wanted_run = Some(self.run.clone());
                }
                let actuals = actuals(&self.run);
                self.run_body(ui, &actuals);
            }
            InspectorMode::Compare => {
                if refresh {
                    self.compare_left.invalidate();
                    self.compare_right.invalidate();
                }
                self.compare_left.spec = Some(self.left.clone());
                self.compare_right.spec = Some(self.right.clone());
                self.compare_body(ui);
            }
        }
    }
}

/// Stacked budget bar over the whole window; the unfilled tail is free space.
fn budget_bar(ui: &mut egui::Ui, segments: &[BudgetSegment], window: u64) {
    let used: u64 = segments.iter().map(|segment| segment.tokens).sum();
    let scale = window.max(used).max(1) as f32;
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 14.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, palette().BORDER);
    let mut x = rect.left();
    for (index, segment) in segments.iter().enumerate() {
        let w = rect.width() * segment.tokens as f32 / scale;
        let part = egui::Rect::from_min_max(
            egui::pos2(x, rect.top()),
            egui::pos2((x + w).min(rect.right()), rect.bottom()),
        );
        painter.rect_filled(part, 0.0, segment_color(index));
        x += w;
    }
    response.on_hover_text(
        segments
            .iter()
            .map(|segment| format!("{}: {}", segment.label, segment.tokens))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    ui.horizontal_wrapped(|ui| {
        for (index, segment) in segments.iter().enumerate() {
            ui.label(egui::RichText::new("■").color(segment_color(index)));
            ui.label(muted(format!(
                "{} {}",
                segment.label,
                compact_tokens(segment.tokens)
            )));
        }
        ui.label(egui::RichText::new("■").color(palette().BORDER));
        ui.label(muted(format!(
            "Free {}",
            compact_tokens(window.saturating_sub(used))
        )));
    });
}

fn segment_color(index: usize) -> egui::Color32 {
    let series = crate::panes::usage::charts::SERIES;
    series[index % series.len()]
}

/// `18.4K / 200K (9%)`, never a bare percentage.
fn usage_line(used: u64, window: u64) -> String {
    let percent = if window == 0 {
        0.0
    } else {
        used as f64 * 100.0 / window as f64
    };
    format!(
        "{} / {} ({percent:.1}%)",
        compact_tokens(used),
        compact_tokens(window)
    )
}

/// Dense selectable list row; returns whether it was clicked.
fn list_row(ui: &mut egui::Ui, selected: bool, text: String, hover: Option<String>) -> bool {
    let mut clicked = false;
    crate::theme::widgets::compact_row(ui, selected, |ui| {
        let response = crate::theme::widgets::row_title(ui, text);
        let response = match hover {
            Some(hover) => response.on_hover_text(hover),
            None => response,
        };
        clicked = response.clicked();
    });
    clicked
}
