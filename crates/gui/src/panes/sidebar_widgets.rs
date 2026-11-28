//! Sidebar pieces drawn from plain values, shared by the embedded workbench and the
//! core client: neither the event bus nor the core's protocol reaches them.
//!
//! The project tree's toolbar, project rows and thread-row parts live here; each GUI
//! turns its own records into these views and maps the returned clicks to its actions.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use egui::collapsing_header::CollapsingState;
use egui::{
    Align, Button, Color32, Layout, Rect, Response, RichText, Sense, Stroke, Ui, UiBuilder,
    WidgetInfo, WidgetType,
};
use workspace_ui::ProjectSort;

use crate::theme::icons;
use crate::theme::text::medium;
use crate::theme::text::muted;
use crate::theme::tokens::{FONT_ICON, FONT_SMALL, Palette, ROW_DENSE, SP_1, SP_2, palette};
use crate::theme::widgets::{
    RowTone, accessible, compact_row, ghost, icon_button, icon_text, row_title,
};

/// Side of the square pin/archive action buttons.
pub const ACTION_SIZE: f32 = ROW_DENSE - SP_1;

/// Top-level families a project shows before, and per, "See more".
pub const FAMILY_PAGE: usize = 5;

/// Horizontal step per tree level; also the width of the caret and kind slots.
pub const INDENT: f32 = 16.0;

/// Extra space separating a family with children from its neighbours.
pub const FAMILY_GAP: f32 = SP_1;

/// What a thread asks of the user, by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attention {
    pub question: bool,
    pub approval: bool,
    pub unread: bool,
    /// Another process works on the thread.
    pub external: bool,
}

/// What a row asks of the user, strongest first: an unanswered question, then
/// an approval, then an unread report, then work another process does. The
/// derived order is the priority, so resolving takes the maximum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowAttention {
    #[default]
    Normal,
    ExternalWorking,
    Unread,
    Approval,
    Question,
}

impl Attention {
    pub fn merge(self, other: Self) -> Self {
        Self {
            question: self.question || other.question,
            approval: self.approval || other.approval,
            unread: self.unread || other.unread,
            external: self.external || other.external,
        }
    }

    pub fn resolve(self) -> RowAttention {
        [
            (self.question, RowAttention::Question),
            (self.approval, RowAttention::Approval),
            (self.unread, RowAttention::Unread),
            (self.external, RowAttention::ExternalWorking),
        ]
        .into_iter()
        .find_map(|(set, level)| set.then_some(level))
        .unwrap_or_default()
    }
}

impl RowAttention {
    /// The title color, surface and stripe of a row in this state; none for a
    /// normal row, which keeps the shared row style.
    pub fn colors(self, p: &Palette) -> Option<(Color32, RowTone)> {
        let (fg, surface) = match self {
            Self::Normal => return None,
            Self::ExternalWorking => (p.EXTERNAL_WORKING_FG, p.EXTERNAL_WORKING_SURFACE),
            Self::Unread => (p.UNREAD_FG, p.UNREAD_SURFACE),
            Self::Approval => (p.WARNING_FG, p.WARNING_SURFACE),
            Self::Question => (p.QUESTION_FG, p.QUESTION_SURFACE),
        };
        Some((
            fg,
            RowTone {
                surface,
                stripe: fg,
            },
        ))
    }
}

pub const QUESTION_NOTE: &str = "Answer needed: this thread has an unanswered question";
pub const QUESTION_HIDDEN_NOTE: &str =
    "Answer needed: a collapsed child thread has an unanswered question";
pub const APPROVAL_NOTE: &str = "Approval needed: a tool call is waiting";
pub const APPROVAL_HIDDEN_NOTE: &str =
    "Approval needed: a tool call is waiting in a collapsed child thread";
pub const UNREAD_NOTE: &str = "Unread report";
pub const UNREAD_HIDDEN_NOTE: &str = "Unread report in a collapsed child thread";
pub const EXTERNAL_NOTE: &str = "Working in another window or process";
pub const EXTERNAL_HIDDEN_NOTE: &str =
    "A collapsed child thread is working in another window or process";

/// The words for a row's state, for its tooltip and accessible name: the
/// strongest state, saying whether it is the row's own or a collapsed child's.
pub fn attention_note(own: Attention, hidden: Attention) -> Option<&'static str> {
    [
        (
            own.question,
            hidden.question,
            QUESTION_NOTE,
            QUESTION_HIDDEN_NOTE,
        ),
        (
            own.approval,
            hidden.approval,
            APPROVAL_NOTE,
            APPROVAL_HIDDEN_NOTE,
        ),
        (own.unread, hidden.unread, UNREAD_NOTE, UNREAD_HIDDEN_NOTE),
        (
            own.external,
            hidden.external,
            EXTERNAL_NOTE,
            EXTERNAL_HIDDEN_NOTE,
        ),
    ]
    .into_iter()
    .find_map(|(own, hidden, note, hidden_note)| {
        if own {
            Some(note)
        } else {
            hidden.then_some(hidden_note)
        }
    })
}

/// What a collapsed row hides, so collapsing a family never hides work that
/// needs the user.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hidden {
    pub count: usize,
    pub running: bool,
    pub attention: Attention,
}

/// Rolls up the descendants of `rows[index]`: the following rows deeper than it.
/// `state` says whether a row runs and what it asks of the user.
pub fn roll_up<T>(
    rows: &[(T, usize)],
    index: usize,
    state: impl Fn(&T) -> (bool, Attention),
) -> Hidden {
    let depth = rows[index].1;
    rows[index + 1..]
        .iter()
        .take_while(|(_, next)| *next > depth)
        .fold(Hidden::default(), |hidden, (row, _)| {
            let (running, attention) = state(row);
            Hidden {
                count: hidden.count + 1,
                running: hidden.running || running,
                attention: hidden.attention.merge(attention),
            }
        })
}

/// Orders `items` as a tree, depth first in item order: each root (an item
/// without a parent, or whose parent is not among `items`) is followed by its
/// descendants. Returns `(item index, depth)` per row. Every item appears once,
/// even when parent links are corrupt or cyclic.
pub fn nest<'k, T, K>(
    items: &'k [T],
    id: impl Fn(&'k T) -> &'k K,
    parent: impl Fn(&'k T) -> Option<&'k K>,
) -> Vec<(usize, usize)>
where
    T: 'k,
    K: Hash + Eq + ?Sized + 'k,
{
    let parents: Vec<Option<&K>> = items.iter().map(&parent).collect();
    let shown: HashSet<&K> = items.iter().map(&id).collect();
    let mut children: HashMap<&K, Vec<usize>> = HashMap::new();
    for (row, parent) in parents.iter().enumerate() {
        if let Some(parent) = parent {
            children.entry(parent).or_default().push(row);
        }
    }

    let mut seen = HashSet::new();
    let mut output = Vec::with_capacity(items.len());
    let mut pending = Vec::new();
    // Depth-first in row order; an item already placed is skipped, which also
    // keeps cyclic parent links from looping.
    let mut place = |start: usize| {
        if seen.contains(id(&items[start])) {
            return;
        }
        pending.push((start, 0));
        while let Some((row, depth)) = pending.pop() {
            let key = id(&items[row]);
            if !seen.insert(key) {
                continue;
            }
            output.push((row, depth));
            if let Some(rows) = children.get(key) {
                pending.extend(rows.iter().rev().map(|&child| (child, depth + 1)));
            }
        }
    };
    for (row, parent) in parents.iter().enumerate() {
        if parent.is_none_or(|parent| !shown.contains(parent)) {
            place(row);
        }
    }
    // Corrupt or cyclic parent links still leave every item reachable.
    for row in 0..items.len() {
        place(row);
    }
    output
}

/// Keeps the first `limit` top-level families plus any later family with a
/// member matching `keep`; returns the kept rows and the hidden family count.
pub fn limit_families<T: Copy>(
    rows: &[(T, usize)],
    limit: usize,
    keep: impl Fn(&T) -> bool,
) -> (Vec<(T, usize)>, usize) {
    let mut kept = Vec::with_capacity(rows.len());
    let mut hidden = 0;
    let families = rows.chunk_by(|_, (_, depth)| *depth > 0);
    for (index, family) in families.enumerate() {
        if index < limit || family.iter().any(|(row, _)| keep(row)) {
            kept.extend_from_slice(family);
        } else {
            hidden += 1;
        }
    }
    (kept, hidden)
}

/// Where a project's "See more" limit is kept in egui's temporary data.
pub fn family_limit_id(project: impl Hash + std::fmt::Debug) -> egui::Id {
    egui::Id::new(("sidebar-project-family-limit", project))
}

/// A family with children is set apart from the rows around it; childless
/// roots stay packed together.
pub fn family_gap_before<T>(rows: &[(T, usize)], index: usize) -> bool {
    let starts_family = rows.get(index).is_some_and(|(_, depth)| *depth == 0);
    let has_children = rows.get(index + 1).is_some_and(|(_, depth)| *depth > 0);
    let follows_children = index
        .checked_sub(1)
        .and_then(|previous| rows.get(previous))
        .is_some_and(|(_, depth)| *depth > 0);
    starts_family && index > 0 && (has_children || follows_children)
}

/// Tree guides per row. At `level` (1..=depth) the branch continues below a row
/// when a later row at that level follows before the tree climbs above it.
///
/// Built in O(rows). A row's guides are read sparsely: whether its own branch
/// continues in O(1), and the ancestor levels that continue by walking only
/// those levels, so the work follows the lines drawn rather than the depth,
/// and rows that are not drawn cost nothing.
pub struct TreeRails {
    depths: Vec<usize>,
    /// Next later row at the same depth or shallower.
    next_at_most: Vec<Option<usize>>,
    /// Next later row strictly shallower.
    next_shallower: Vec<Option<usize>>,
}

impl TreeRails {
    pub fn new<T>(rows: &[(T, usize)]) -> Self {
        let depths: Vec<usize> = rows.iter().map(|(_, depth)| *depth).collect();
        // Monotonic stacks from the end: the top is the nearest candidate.
        let next = |pops: fn(usize, usize) -> bool| {
            let mut stack: Vec<usize> = Vec::new();
            let mut next = vec![None; depths.len()];
            for index in (0..depths.len()).rev() {
                while stack
                    .last()
                    .is_some_and(|&top| pops(depths[top], depths[index]))
                {
                    stack.pop();
                }
                next[index] = stack.last().copied();
                stack.push(index);
            }
            next
        };
        Self {
            next_at_most: next(|top, row| top > row),
            next_shallower: next(|top, row| top >= row),
            depths,
        }
    }

    /// Whether the row's own branch (its deepest level) continues below it.
    pub fn own_continues(&self, index: usize) -> bool {
        let depth = self.depths[index];
        depth > 0 && self.next_at_most[index].is_some_and(|next| self.depths[next] == depth)
    }

    /// The ancestor levels (shallower than the row, at least 1) whose branch
    /// continues below the row, deepest first. For a level, the first later row
    /// at or above it lies on the chain of strictly shallower rows, and the
    /// level continues exactly when that row sits on it, so the levels are the
    /// depths on the chain.
    pub fn continuing_ancestors(&self, index: usize) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(self.next_shallower[index], |&row| self.next_shallower[row])
            .map(|row| self.depths[row])
            .take_while(|&level| level >= 1)
    }
}

pub fn rail_stroke() -> Stroke {
    // BORDER nearly vanishes on the black sidebar; a dimmed muted text tone
    // stays visible in both palettes without competing with titles.
    Stroke::new(1.0, palette().TEXT_MUTED.gamma_multiply(0.45))
}

/// Paints the rails left of `slot` (the row's caret slot): pass-through lines
/// for ancestor branches that continue, and an elbow joining this row to its
/// parent's rail.
pub fn paint_rails(
    ui: &Ui,
    slot: Rect,
    depth: usize,
    rails: &TreeRails,
    index: usize,
    row_gap: f32,
) {
    if depth == 0 {
        return;
    }
    let painter = ui.painter();
    let stroke = rail_stroke();
    let top = slot.top() - row_gap / 2.0;
    let bottom = slot.bottom() + row_gap / 2.0;
    let middle = painter.round_to_pixel_center(slot.center().y);
    let rail_x = |level: usize| {
        painter.round_to_pixel_center(slot.left() - (depth - level) as f32 * INDENT - INDENT / 2.0)
    };
    for level in rails.continuing_ancestors(index) {
        painter.vline(rail_x(level), top..=bottom, stroke);
    }
    let x = rail_x(depth);
    painter.vline(
        x,
        top..=if rails.own_continues(index) {
            bottom
        } else {
            middle
        },
        stroke,
    );
    painter.hline(x..=slot.left(), middle, stroke);
}

/// Why a thread hangs under its parent in the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchKind {
    /// A worker run escalated its task to this orchestrator conversation.
    Escalation,
    /// The user forked the parent conversation.
    Fork,
    /// An independent Orchestrator thread its lead manages (ADR 0044).
    Lane,
}

pub fn branch_kind_icon(ui: &Ui, slot: Rect, response: Response, kind: BranchKind) {
    let (icon, label, hint) = match kind {
        BranchKind::Escalation => (
            icons::ARROW_FAT_LINE_UP,
            "Escalated thread",
            "Escalated: a worker run handed its task to this orchestrator thread",
        ),
        BranchKind::Fork => (
            icons::GIT_FORK,
            "Forked thread",
            "Forked from its parent thread",
        ),
        BranchKind::Lane => (
            icons::STACK,
            "Lane thread",
            "Lane: an independent Orchestrator thread managed by its lead. It follows the lead's pin and archive.",
        ),
    };
    ui.painter().text(
        slot.center(),
        egui::Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(FONT_SMALL),
        palette().TEXT_MUTED,
    );
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, label));
    response.on_hover_text(hint);
}

/// What the leading part of a tree row draws: its indent, caret, rails and kind.
pub struct TreeRowLead<'a> {
    pub rails: &'a TreeRails,
    /// The row's index in the rows `rails` was built from.
    pub index: usize,
    pub depth: usize,
    pub has_children: bool,
    /// Shown when the row hangs under a parent.
    pub kind: Option<BranchKind>,
    /// The visible area; rows outside it paint no rails.
    pub clip: Rect,
    /// The vertical gap between rows, which the rails bridge.
    pub row_gap: f32,
}

/// Draws the leading part of a thread row, inside the row's horizontal layout: the
/// indent, the caret slot with its rails, and the branch-kind glyph. The caret toggles
/// `expansion`; its accessible name is "Collapse|Expand children of {name}".
pub fn tree_row_lead(
    ui: &mut Ui,
    lead: &TreeRowLead<'_>,
    expansion: &mut CollapsingState,
    name: impl std::fmt::Display,
) {
    let TreeRowLead {
        rails,
        index,
        depth,
        has_children,
        kind,
        clip,
        row_gap,
    } = *lead;
    ui.spacing_mut().item_spacing.x = SP_1;
    ui.spacing_mut().button_padding.x = SP_1;
    ui.add_space(depth as f32 * INDENT);
    let (rect, toggle) = ui.allocate_exact_size(
        egui::vec2(INDENT, ROW_DENSE),
        if has_children {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    // Rows scrolled out of view need no guides.
    if rect.bottom() + row_gap >= clip.top() && rect.top() - row_gap <= clip.bottom() {
        paint_rails(ui, rect, depth, rails, index, row_gap);
    }
    if has_children {
        if toggle.clicked() {
            expansion.toggle(ui);
        }
        let label = if expansion.is_open() {
            "Collapse"
        } else {
            "Expand"
        };
        toggle.widget_info(|| {
            WidgetInfo::labeled(
                WidgetType::Button,
                true,
                format!("{label} children of {name}"),
            )
        });
        let caret = if expansion.is_open() {
            icons::CARET_DOWN
        } else {
            icons::CARET_RIGHT
        };
        let color = if toggle.hovered() {
            palette().TEXT
        } else {
            palette().TEXT_MUTED
        };
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            caret,
            egui::FontId::proportional(FONT_SMALL),
            color,
        );
        if expansion.is_open() {
            // Starts the children's rail just below the caret.
            ui.painter().vline(
                ui.painter().round_to_pixel_center(rect.center().x),
                rect.center().y + FONT_SMALL / 2.0..=rect.bottom() + row_gap / 2.0,
                rail_stroke(),
            );
        }
    }
    if let Some(kind) = kind {
        // A leaf reuses its empty caret slot; a parent needs its own.
        let (slot, response) = if has_children {
            ui.allocate_exact_size(egui::vec2(INDENT, ROW_DENSE), Sense::hover())
        } else {
            (rect, toggle)
        };
        branch_kind_icon(ui, slot, response, kind);
    }
}

/// The run state a row shows: a spinner while running, a warning once failed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowStatus {
    Running,
    Failed,
    #[default]
    Other,
}

/// The status icon and the question/approval marks at a row's trailing edge.
pub fn thread_status_icon(ui: &mut Ui, status: RowStatus, attention: Attention, hidden: Attention) {
    // Runtime status retains the display aggregation; questions and approvals
    // are independent and must remain visible alongside either the spinner or
    // the error icon. The running spinner lives in the Archive slot instead
    // (see `running_spinner`); an unread report appears here once the work
    // has finished. `hidden` is what collapsed descendants need; the row's own
    // state takes precedence, and the labels say where the rest comes from.
    let unread = if attention.unread {
        Some("Thread status: Unread report")
    } else if hidden.unread {
        Some("Unread report in a collapsed child thread")
    } else {
        None
    };
    let status = if matches!(status, RowStatus::Running) {
        None
    } else if matches!(status, RowStatus::Failed) {
        let response = ui.add(
            egui::Label::new(
                RichText::new(icons::WARNING)
                    .size(FONT_SMALL + 2.0)
                    .color(palette().ERROR_FG),
            )
            .sense(Sense::hover()),
        );
        Some((response, "Thread status: Failed"))
    } else if let Some(label) = unread {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(FONT_SMALL, FONT_SMALL), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), SP_1 + 1.0, palette().ACCENT);
        Some((response, label))
    } else {
        None
    };
    if let Some((response, label)) = status {
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, ui.is_enabled(), label));
        response.on_hover_text(label);
    }
    let question = if attention.question {
        Some(QUESTION_NOTE)
    } else if hidden.question {
        Some(QUESTION_HIDDEN_NOTE)
    } else {
        None
    };
    if let Some(label) = question {
        attention_dot(ui, label);
    }
    let approval = if attention.approval {
        Some(APPROVAL_NOTE)
    } else if hidden.approval {
        Some(APPROVAL_HIDDEN_NOTE)
    } else {
        None
    };
    if let Some(label) = approval {
        if question.is_some() {
            // Two identical dots would be indistinguishable, so an approval
            // beside a question uses its own glyph.
            let response = ui.add(
                egui::Label::new(
                    RichText::new(icons::SHIELD_WARNING)
                        .size(FONT_SMALL + 2.0)
                        .color(palette().WARNING_FG),
                )
                .sense(Sense::hover()),
            );
            response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, ui.is_enabled(), label));
            response.on_hover_text(label);
        } else {
            attention_dot(ui, label);
        }
    }
}

pub fn running_spinner(ui: &mut Ui, label: &str, color: Color32) {
    // add_sized reserves the whole Archive slot and centers the spinner in it.
    let response = ui.add_sized(egui::vec2(ACTION_SIZE, ACTION_SIZE), |ui: &mut Ui| {
        crate::theme::widgets::spinner_with(ui, FONT_SMALL, Some(color))
    });
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, ui.is_enabled(), label));
    response.on_hover_text(label);
}

pub fn attention_dot(ui: &mut Ui, label: &str) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(FONT_SMALL, FONT_SMALL), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), SP_1, palette().WARNING_FG);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, ui.is_enabled(), label));
    response.on_hover_text(label);
}

/// A pill counting a collapsed row's descendants; nothing when none are hidden.
pub fn hidden_count_badge(ui: &mut Ui, count: usize) {
    if count == 0 {
        return;
    }
    let galley = ui.painter().layout_no_wrap(
        count.to_string(),
        egui::FontId::proportional(FONT_SMALL),
        palette().TEXT_MUTED,
    );
    let size = egui::vec2(
        (galley.size().x + SP_2).max(galley.size().y + SP_1),
        galley.size().y + SP_1,
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    ui.add_space(SP_1);
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same((size.y / 2.0) as u8),
        rail_stroke(),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(
        rect.center() - galley.size() / 2.0,
        galley,
        palette().TEXT_MUTED,
    );
    let label = if count == 1 {
        "1 hidden child thread".to_owned()
    } else {
        format!("{count} hidden child threads")
    };
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &label));
    response.on_hover_text(label);
}

/// Brings an archived display root back to the main list.
pub fn restore_button(ui: &mut Ui) -> Response {
    let response = ui.add(
        ghost(icon_text(icons::ARROW_U_UP_LEFT)).min_size(egui::vec2(ACTION_SIZE, ACTION_SIZE)),
    );
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Restore"));
    response.on_hover_text("Restore from archive")
}

/// Archives a display root; a pinned one must be unpinned first.
pub fn archive_button(ui: &mut Ui, can_archive: bool) -> Response {
    let archive = ui
        .add_enabled(
            can_archive,
            ghost(icon_text(icons::ARCHIVE)).min_size(egui::vec2(ACTION_SIZE, ACTION_SIZE)),
        )
        .on_hover_text("Archive")
        .on_disabled_hover_text("Unpin this thread before archiving");
    archive.widget_info(|| WidgetInfo::labeled(WidgetType::Button, can_archive, "Archive"));
    archive
}

/// Pins or unpins a display root: "★" when pinned, "☆" otherwise.
pub fn pin_button(ui: &mut Ui, pinned: bool) -> Response {
    let (pin_icon, pin_label, pin_hint) = if pinned {
        (
            icons::filled(ui, icons::PUSH_PIN)
                .size(FONT_ICON)
                .color(palette().TEXT),
            "★",
            "Unpin",
        )
    } else {
        (
            icon_text(icons::PUSH_PIN).color(palette().TEXT_MUTED),
            "☆",
            "Pin",
        )
    };
    let pin = ui.add(ghost(pin_icon).min_size(egui::vec2(ACTION_SIZE, ACTION_SIZE)));
    pin.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, pin_label));
    pin.on_hover_text(pin_hint)
}

/// What the trailing controls of a live (not archived) thread row show.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrailingView {
    pub status: RowStatus,
    /// The row's own attention; `external` picks the spinner's color.
    pub own: Attention,
    /// What its collapsed descendants hide.
    pub hidden: Hidden,
    /// The row is a root of the displayed tree: only roots pin and archive.
    pub display_root: bool,
    /// A member of the row's family runs, so it cannot be archived.
    pub family_running: bool,
    pub pinned: bool,
}

/// Which trailing control was clicked this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrailingClicks {
    pub archive: bool,
    pub pin: bool,
}

/// Draws a live row's trailing edge, right to left: the status icon, the Archive slot
/// (or the running spinner in its place), the Pin slot and the hidden-count pill.
/// Call it inside a right-to-left layout; it leaves the item spacing at 0 for the
/// title that follows.
pub fn trailing_controls(ui: &mut Ui, view: TrailingView) -> TrailingClicks {
    let TrailingView {
        status,
        own,
        hidden,
        display_root,
        family_running,
        pinned,
    } = view;
    let mut clicks = TrailingClicks::default();
    // Runtime/question indicators stay at the trailing edge.
    ui.spacing_mut().item_spacing.x = SP_1;
    thread_status_icon(ui, status, own, hidden.attention);
    ui.add_space(SP_1);
    ui.spacing_mut().item_spacing.x = 0.0;
    // The slot before Pin holds Archive, or the running spinner in its
    // place, so Pin stays put when a run starts or ends. A running family
    // cannot be archived, so the two never compete for the slot.
    if display_root && !family_running {
        clicks.archive = archive_button(ui, !pinned).clicked();
    } else if matches!(status, RowStatus::Running) {
        if own.external {
            running_spinner(
                ui,
                "Thread status: Running in another window or process",
                palette().EXTERNAL_WORKING_FG,
            );
        } else {
            running_spinner(ui, "Thread status: Running", palette().RUNNING);
        }
    } else if hidden.running {
        if hidden.attention.external {
            running_spinner(
                ui,
                "Running in another window or process in a collapsed child thread",
                palette().EXTERNAL_WORKING_FG,
            );
        } else {
            running_spinner(ui, "Running in a collapsed child thread", palette().RUNNING);
        }
    } else {
        ui.add_space(ACTION_SIZE);
    }
    // A child hangs under its parent, so pinning it would do nothing: only
    // the displayed root has a Pin button. Children keep the slot empty so
    // the trailing columns stay aligned.
    if display_root {
        clicks.pin = pin_button(ui, pinned).clicked();
    } else {
        ui.add_space(ACTION_SIZE);
    }
    ui.add_space(SP_1);
    hidden_count_badge(ui, hidden.count);
    clicks
}

/// A thread row's title: tinted with `color`, named "↳ {title}" for a child row,
/// hovering `hover`. `draggable` lets the caller start a drag from it. The caller
/// adds its context menu and reads the click from the returned response.
pub fn title_label(
    ui: &mut Ui,
    title: &str,
    color: Option<Color32>,
    child: bool,
    draggable: bool,
    hover: &str,
) -> Response {
    // Indentation and the caret already show nesting; "↳" stays in the
    // accessible name only.
    let response = match color {
        Some(color) => row_title(ui, RichText::new(title).color(color)),
        None => row_title(ui, title),
    };
    // Use the title's existing widget ID so selection and right-click menus
    // keep their pointer ownership. A separate full-row drag overlay steals it.
    let response = if draggable {
        response.interact(Sense::click_and_drag())
    } else {
        response
    };
    if child {
        let label = format!("↳ {title}");
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &label));
    }
    response.on_hover_text(hover)
}

/// What a project row shows. The caller scopes the row's ID (one per project).
#[derive(Clone, Copy, Debug)]
pub struct ProjectRowView<'a> {
    pub name: &'a str,
    /// The repository root in the title's hover; "…" while it is unknown.
    pub hover_root: Option<&'a str>,
    pub selected: bool,
    /// The project's threads are listed below it.
    pub open: bool,
    /// Live threads; shown only when greater than 0.
    pub count: usize,
    /// Offer "Search threads in {name}".
    pub search: bool,
    /// Offer "Project settings".
    pub settings: bool,
}

/// Which part of a project row was clicked this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectRowClicks {
    /// The caret: fold or unfold the project's threads.
    pub toggle: bool,
    pub select: bool,
    pub search: bool,
    pub new_thread: bool,
    pub settings: bool,
}

/// A project's row: caret, folder, title, live-thread count and its buttons.
pub fn project_row(ui: &mut Ui, view: ProjectRowView<'_>) -> ProjectRowClicks {
    let ProjectRowView {
        name,
        hover_root,
        selected,
        open,
        count,
        search,
        settings,
    } = view;
    let mut clicks = ProjectRowClicks::default();
    compact_row(ui, selected, |ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        let (rect, toggle) = ui.allocate_exact_size(egui::vec2(16.0, ROW_DENSE), Sense::click());
        clicks.toggle = toggle.clicked();
        toggle.widget_info(|| {
            WidgetInfo::labeled(
                WidgetType::Button,
                true,
                format!(
                    "{} threads of {}",
                    if open { "Collapse" } else { "Expand" },
                    name
                ),
            )
        });
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            if open {
                icons::CARET_DOWN
            } else {
                icons::CARET_RIGHT
            },
            egui::FontId::proportional(FONT_SMALL),
            if toggle.hovered() {
                palette().TEXT
            } else {
                palette().TEXT_MUTED
            },
        );
        let (icon, color) = if selected {
            (icons::FOLDER_OPEN, palette().ACCENT)
        } else {
            (icons::FOLDER_SIMPLE, palette().TEXT_MUTED)
        };
        ui.label(icon_text(icon).color(color));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = SP_1;
            if settings {
                clicks.settings = icon_button(ui, icons::GEAR_SIX, "Project settings").clicked();
            }
            let new_thread = format!("New thread in {name}");
            clicks.new_thread = icon_button(ui, icons::PLUS, &new_thread).clicked();
            if count > 0 {
                ui.label(muted(count.to_string()));
            }
            // Drawn after the count in this right-to-left layout, so it sits to
            // its left on screen. It stays for a project without threads.
            if search {
                let search = format!("Search threads in {name}");
                clicks.search = icon_button(ui, icons::MAGNIFYING_GLASS, &search).clicked();
            }
            let title = if selected {
                medium(name).color(palette().TEXT)
            } else {
                RichText::new(name).color(palette().TEXT)
            };
            clicks.select = row_title(ui, title)
                .on_hover_text(hover_root.unwrap_or("…"))
                .clicked();
        });
    });
    clicks
}

/// Height of the toolbar strip, separator included: one dense row.
pub const TOOLBAR_HEIGHT: f32 = ROW_DENSE;
/// Side of the toolbar's icon-only buttons.
const ICON_BUTTON: f32 = ROW_DENSE - SP_1;

const ADD_LABEL: &str = "Add project";
const SORT_LABEL: &str = "Sort projects";

/// The embedded workbench's project orders, in menu order.
pub const LEGACY_SORT_CHOICES: [(ProjectSort, &str); 3] = [
    (ProjectSort::Registration, "Registration order"),
    (ProjectSort::Name, "Name (A–Z)"),
    (ProjectSort::RecentActivity, "Recent activity"),
];

/// What the toolbar shows.
#[derive(Clone, Copy, Debug)]
pub struct ToolbarView<'a> {
    /// Folding is offered only when there is a project.
    pub has_projects: bool,
    /// Some project is unfolded: the fold button folds them all.
    pub any_open: bool,
    /// The current order, checked in the menu.
    pub sort: ProjectSort,
    /// The orders the menu offers, with their labels.
    pub choices: &'a [(ProjectSort, &'a str)],
}

/// What the operator chose on the toolbar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolbarAction {
    AddProject,
    Sort(ProjectSort),
    /// Fold (`true`) or unfold (`false`) every project.
    SetAllCollapsed(bool),
}

/// Draws the fixed strip above the project list across the full width: add a project,
/// sort, fold everything.
pub fn sidebar_toolbar(ui: &mut Ui, view: ToolbarView<'_>) -> Option<ToolbarAction> {
    let mut action = None;
    let (band, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), TOOLBAR_HEIGHT),
        Sense::hover(),
    );
    ui.painter().hline(
        band.x_range(),
        band.bottom() - 0.5,
        Stroke::new(1.0, palette().BORDER),
    );
    let inner = band
        .shrink2(egui::vec2(SP_2, 0.0))
        .with_max_y(band.bottom() - 1.0);
    let mut strip = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Center)),
    );
    strip.spacing_mut().item_spacing.x = SP_1;

    let add_text = icons::with_icon(icons::PLUS, ADD_LABEL);
    let full_width = text_width(ui, &add_text, egui::TextStyle::Button.resolve(ui.style()))
        + 2.0 * ui.spacing().button_padding.x;
    // The text label only stays while it fits beside the two right-hand buttons, at
    // the widths they actually take (a glyph plus padding can exceed `ICON_BUTTON`).
    let right_width = icon_button_width(ui, icons::ARROWS_DOWN_UP)
        + icon_button_width(ui, icons::ARROWS_IN_LINE_VERTICAL)
            .max(icon_button_width(ui, icons::ARROWS_OUT_LINE_VERTICAL));
    let wide = inner.width() >= full_width + right_width + 2.0 * SP_1;

    if wide {
        let response = strip.add(ghost(add_text).min_size(egui::vec2(0.0, ICON_BUTTON)));
        accessible(&response, ADD_LABEL);
        if response.clicked() {
            action = Some(ToolbarAction::AddProject);
        }
    } else if icon_button(&mut strip, icons::PLUS, ADD_LABEL).clicked() {
        action = Some(ToolbarAction::AddProject);
    }

    strip.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        fold_button(ui, view, &mut action);
        sort_button(ui, view, &mut action);
    });
    action
}

fn text_width(ui: &Ui, text: &str, font: egui::FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, palette().TEXT)
        .size()
        .x
}

/// Width an icon button takes: its glyph and padding, never below `ICON_BUTTON`.
fn icon_button_width(ui: &Ui, icon: &str) -> f32 {
    let glyph = text_width(ui, icon, egui::FontId::proportional(FONT_ICON));
    (glyph + 2.0 * ui.spacing().button_padding.x).max(ICON_BUTTON)
}

/// Folds every project when any is open, otherwise unfolds them all.
fn fold_button(ui: &mut Ui, view: ToolbarView<'_>, action: &mut Option<ToolbarAction>) {
    let fold = view.any_open;
    let (icon, label) = if fold {
        (icons::ARROWS_IN_LINE_VERTICAL, "Collapse all projects")
    } else {
        (icons::ARROWS_OUT_LINE_VERTICAL, "Expand all projects")
    };
    let clicked = ui
        .add_enabled_ui(view.has_projects, |ui| {
            icon_button(ui, icon, label).clicked()
        })
        .inner;
    if clicked {
        *action = Some(ToolbarAction::SetAllCollapsed(fold));
    }
}

/// A menu of the offered orders, the current one checked.
fn sort_button(ui: &mut Ui, view: ToolbarView<'_>, action: &mut Option<ToolbarAction>) {
    let button =
        ghost(icon_text(icons::ARROWS_DOWN_UP)).min_size(egui::vec2(ICON_BUTTON, ICON_BUTTON));
    let (response, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
        for &(sort, name) in view.choices {
            let current = view.sort == sort;
            let mut item = Button::selectable(current, name);
            if current {
                item = item.right_text(icons::CHECK);
            }
            let response = ui.add(item);
            // `right_text` would otherwise become part of the accessible name.
            response.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, current, name));
            if response.clicked() {
                *action = Some(ToolbarAction::Sort(sort));
            }
        }
    });
    accessible(&response, SORT_LABEL);
    response.on_hover_text(SORT_LABEL);
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};
    use workspace_ui::ProjectSort;

    use super::{
        Attention, Hidden, RowAttention, RowStatus, ToolbarAction, ToolbarView, TrailingView,
        TreeRails, attention_note, family_gap_before, nest, sidebar_toolbar, trailing_controls,
    };

    fn rows_at(depths: &[usize]) -> Vec<(usize, usize)> {
        depths.iter().copied().enumerate().collect()
    }

    /// The definition: a level continues when the first later row at or above
    /// it sits exactly at it.
    fn brute_force_rails(depths: &[usize], index: usize) -> Vec<bool> {
        (1..=depths[index])
            .map(|level| {
                depths[index + 1..]
                    .iter()
                    .find(|&&next| next <= level)
                    .is_some_and(|&next| next == level)
            })
            .collect()
    }

    /// The sparse reads as one flag per level, shallowest first.
    fn dense_rails(rails: &TreeRails, depth: usize, index: usize) -> Vec<bool> {
        let ancestors: Vec<usize> = rails.continuing_ancestors(index).collect();
        (1..=depth)
            .map(|level| {
                if level == depth {
                    rails.own_continues(index)
                } else {
                    ancestors.contains(&level)
                }
            })
            .collect()
    }

    #[test]
    fn rails_match_the_definition_on_deep_wide_and_irregular_trees() {
        let mut shapes: Vec<Vec<usize>> = vec![
            // A single chain, and a chain followed by siblings at every level.
            (0..300).collect(),
            (0..150).chain((0..150).rev()).collect(),
            // Wide families with children.
            (0..40).flat_map(|_| [0, 1, 2, 2, 1]).collect(),
        ];
        // Depth jumps no tree walk produces still follow the definition.
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        for _ in 0..40 {
            let shape = (0..30)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    usize::try_from(state % 6).unwrap()
                })
                .collect();
            shapes.push(shape);
        }
        for depths in shapes {
            let rows = rows_at(&depths);
            let rails = TreeRails::new(&rows);
            for index in 0..rows.len() {
                assert_eq!(
                    dense_rails(&rails, depths[index], index),
                    brute_force_rails(&depths, index),
                    "row {index} of {depths:?}"
                );
            }
        }
    }

    #[test]
    fn rails_continue_only_while_a_later_sibling_follows() {
        // root / a / a1 / a2 / b / b1 / other
        let rows = rows_at(&[0, 1, 2, 2, 1, 2, 0]);
        let rails = TreeRails::new(&rows);
        assert_eq!(
            (0..rows.len())
                .map(|index| dense_rails(&rails, rows[index].1, index))
                .collect::<Vec<_>>(),
            [
                vec![],
                // a continues to its sibling b.
                vec![true],
                // a1: level 1 passes through toward b; a2 follows at level 2.
                vec![true, true],
                // a2 is a's last child, while level 1 still runs to b.
                vec![true, false],
                // b is the root's last child.
                vec![false],
                // b1 is b's last child, and nothing continues past it.
                vec![false, false],
                vec![],
            ]
        );
    }

    #[test]
    fn family_gap_sets_apart_only_families_with_children() {
        // solo / solo / family(root, child) / solo / solo
        let rows = rows_at(&[0, 0, 0, 1, 0, 0]);
        let gaps: Vec<_> = (0..rows.len())
            .map(|index| family_gap_before(&rows, index))
            .collect();
        assert_eq!(gaps, [false, false, true, false, true, false]);
        // A leading family needs no gap above the first row.
        let rows = rows_at(&[0, 1, 0]);
        assert!(!family_gap_before(&rows, 0));
        assert!(family_gap_before(&rows, 2));
    }

    fn flags(question: bool, approval: bool, unread: bool, external: bool) -> Attention {
        Attention {
            question,
            approval,
            unread,
            external,
        }
    }

    #[test]
    fn a_row_takes_the_strongest_of_its_states() {
        // Given: every combination of the four states
        for bits in 0..16u8 {
            let has = |bit: u8| bits & (1 << bit) != 0;
            let attention = flags(has(0), has(1), has(2), has(3));

            // When: the row's state is resolved
            let expected = if has(0) {
                RowAttention::Question
            } else if has(1) {
                RowAttention::Approval
            } else if has(2) {
                RowAttention::Unread
            } else if has(3) {
                RowAttention::ExternalWorking
            } else {
                RowAttention::Normal
            };

            // Then: question > approval > unread > external work > normal
            assert_eq!(attention.resolve(), expected, "{attention:?}");
        }
    }

    #[test]
    fn the_note_names_the_strongest_state_and_where_it_comes_from() {
        let none = Attention::default();
        assert_eq!(attention_note(none, none), None);
        assert_eq!(
            attention_note(flags(false, false, false, true), none),
            Some("Working in another window or process")
        );
        assert_eq!(
            attention_note(none, flags(false, false, false, true)),
            Some("A collapsed child thread is working in another window or process")
        );
        // A stronger child state beats a weaker one of the row itself only when
        // it is stronger; the row's own state wins at the same level.
        assert_eq!(
            attention_note(
                flags(false, false, false, true),
                flags(false, false, true, false)
            ),
            Some("Unread report in a collapsed child thread")
        );
        assert_eq!(
            attention_note(
                flags(true, false, false, true),
                flags(true, false, false, false)
            ),
            Some("Answer needed: this thread has an unanswered question")
        );
    }

    #[test]
    fn nest_keeps_orphans_and_cycles_reachable_with_string_ids() {
        // (id, parent): "b" hangs under "a"; "orphan" names a parent that is not listed;
        // "x" and "y" point at each other; "c" sits under "b".
        let items: Vec<(String, Option<String>)> = [
            ("x", Some("y")),
            ("a", None),
            ("c", Some("b")),
            ("orphan", Some("missing")),
            ("b", Some("a")),
            ("y", Some("x")),
        ]
        .into_iter()
        .map(|(id, parent)| (id.to_owned(), parent.map(str::to_owned)))
        .collect();
        let rows = nest(&items, |item| item.0.as_str(), |item| item.1.as_deref());
        let named: Vec<(&str, usize)> = rows
            .iter()
            .map(|&(index, depth)| (items[index].0.as_str(), depth))
            .collect();
        assert_eq!(
            named,
            vec![
                // Roots first, depth first in item order: "a" with its subtree, then
                // the orphan as a root of its own.
                ("a", 0),
                ("b", 1),
                ("c", 2),
                ("orphan", 0),
                // The cycle has no root; it is placed from its first member.
                ("x", 0),
                ("y", 1),
            ]
        );
    }

    #[test]
    fn toolbar_offers_only_the_given_sort_choices() {
        let choices = [
            (ProjectSort::Name, "Name (A–Z)"),
            (ProjectSort::RecentActivity, "Recent activity"),
        ];
        let mut harness = Harness::builder()
            .with_size(egui::vec2(320.0, 240.0))
            .build_ui_state(
                |ui, action: &mut Option<ToolbarAction>| {
                    crate::theme::install(ui.ctx());
                    let view = ToolbarView {
                        has_projects: true,
                        any_open: true,
                        sort: ProjectSort::Name,
                        choices: &choices,
                    };
                    if let Some(chosen) = sidebar_toolbar(ui, view) {
                        *action = Some(chosen);
                    }
                },
                None,
            );
        harness.run();
        assert!(harness.query_by_label("Add project").is_some());
        assert!(harness.query_by_label("Collapse all projects").is_some());

        harness.get_by_label("Sort projects").click();
        harness.run();
        assert!(harness.query_by_label("Registration order").is_none());
        assert!(harness.query_by_label("Name (A–Z)").is_some());
        harness.get_by_label("Recent activity").click();
        harness.run();
        assert_eq!(
            harness.state(),
            &Some(ToolbarAction::Sort(ProjectSort::RecentActivity))
        );
    }

    #[test]
    fn trailing_controls_put_the_spinner_in_the_archive_slot() {
        // A running display root whose family runs cannot be archived: the spinner takes
        // the Archive slot, right of Pin. A running child row shows it the same way.
        for display_root in [true, false] {
            let mut harness = Harness::builder()
                .with_size(egui::vec2(320.0, 60.0))
                .build_ui(move |ui| {
                    crate::theme::install(ui.ctx());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        trailing_controls(
                            ui,
                            TrailingView {
                                status: RowStatus::Running,
                                own: Attention::default(),
                                hidden: Hidden::default(),
                                display_root,
                                family_running: true,
                                pinned: false,
                            },
                        );
                    });
                });
            // A spinner continuously requests repaint; advance bounded frames.
            harness.run_steps(2);

            let spinner = harness.get_by_label("Thread status: Running").rect();
            assert!(harness.query_by_label("Archive").is_none());
            if display_root {
                let pin = harness.get_by_label("☆").rect();
                assert!(
                    pin.right() <= spinner.left(),
                    "spinner {spinner:?} sits right of pin {pin:?}"
                );
            } else {
                assert!(harness.query_by_label("☆").is_none());
            }
        }
    }
}
