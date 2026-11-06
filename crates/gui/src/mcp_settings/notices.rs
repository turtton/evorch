//! Non-blocking notices for servers a new run skipped because they are not
//! approved. Each project's status watch (the same one runs publish to) is
//! followed in the background; the render thread only compares snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use egui::Ui;

use super::{McpWaker, StatusReceiver};
use crate::mcp::{McpGuiService, McpServerStatus};
use crate::theme::text::{muted, semibold};
use crate::theme::tokens::*;
use crate::theme::widgets::{accessible, primary_button, surface_frame};

/// A run started without this server because it is not approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpApprovalNotice {
    /// The project root as the sidebar records it.
    pub project_root: PathBuf,
    pub server_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpNoticeAction {
    /// Open the project's MCP servers and review this server's approval.
    Review(McpApprovalNotice),
    Dismiss(McpApprovalNotice),
}

struct Watch {
    status: StatusReceiver,
    primed: bool,
    /// Servers last seen waiting for approval.
    waiting: BTreeSet<String>,
}

pub(super) struct ProjectWatches {
    tx: Sender<(PathBuf, StatusReceiver)>,
    rx: Receiver<(PathBuf, StatusReceiver)>,
    /// `None` while the subscription is being set up off-thread.
    watches: BTreeMap<PathBuf, Option<Watch>>,
    notices: Vec<McpApprovalNotice>,
}

impl Default for ProjectWatches {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            watches: BTreeMap::new(),
            notices: Vec::new(),
        }
    }
}

impl ProjectWatches {
    pub(super) fn notices(&self) -> &[McpApprovalNotice] {
        &self.notices
    }

    pub(super) fn dismiss(&mut self, notice: &McpApprovalNotice) {
        self.notices.retain(|candidate| candidate != notice);
    }

    /// The project's MCP dialog is open: its rows show the same state.
    pub(super) fn dismiss_project(&mut self, root: &Path) {
        self.notices.retain(|notice| notice.project_root != root);
    }

    /// The background subscription for `root`, unless it is already watched.
    /// The task resolves the canonical root (the key runs publish under),
    /// hands the receiver over and then only wakes the UI on changes.
    pub(super) fn start(
        &mut self,
        root: &Path,
        service: &Arc<dyn McpGuiService>,
        waker: &Arc<Mutex<Option<McpWaker>>>,
    ) -> Option<impl Future<Output = ()> + Send + 'static> {
        if self.watches.contains_key(root) {
            return None;
        }
        self.watches.insert(root.to_path_buf(), None);
        let root = root.to_path_buf();
        let service = Arc::clone(service);
        let tx = self.tx.clone();
        let waker = Arc::downgrade(waker);
        Some(async move {
            let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            let mut status = service.subscribe_status(canonical);
            drop(service);
            if tx.send((root, status.clone())).is_err() {
                return;
            }
            loop {
                let Some(slot) = waker.upgrade() else {
                    return;
                };
                super::wake(&slot);
                drop(slot);
                if status.changed().await.is_err() {
                    return;
                }
            }
        })
    }

    /// Blocks until every started subscription has been handed over.
    pub(super) fn wait_subscribed(&mut self) {
        while self.watches.values().any(Option::is_none) {
            let Ok((root, status)) = self.rx.recv() else {
                return;
            };
            self.insert(root, status);
        }
    }

    fn insert(&mut self, root: PathBuf, status: StatusReceiver) {
        self.watches.insert(
            root,
            Some(Watch {
                status,
                primed: false,
                waiting: BTreeSet::new(),
            }),
        );
    }

    /// Raises a notice when a server of a watched project starts waiting for
    /// approval, and drops notices of servers that no longer wait.
    pub(super) fn poll(&mut self, open: Option<&Path>) {
        while let Ok((root, status)) = self.rx.try_recv() {
            self.insert(root, status);
        }
        let Self {
            watches, notices, ..
        } = self;
        for (root, watch) in watches.iter_mut() {
            let Some(watch) = watch else {
                continue;
            };
            if watch.primed && !watch.status.has_changed().unwrap_or(false) {
                continue;
            }
            watch.primed = true;
            let waiting: BTreeSet<String> = watch
                .status
                .borrow_and_update()
                .iter()
                .filter(|event| event.status == McpServerStatus::NeedsApproval)
                .map(|event| event.scope.server_id.clone())
                .collect();
            notices.retain(|notice| {
                &notice.project_root != root || waiting.contains(&notice.server_id)
            });
            if open != Some(root.as_path()) {
                for server_id in waiting.difference(&watch.waiting) {
                    notices.push(McpApprovalNotice {
                        project_root: root.clone(),
                        server_id: server_id.clone(),
                    });
                }
            }
            watch.waiting = waiting;
        }
    }
}

/// How many notices are stacked at once; older ones wait below.
const SHOWN: usize = 3;

pub const NOTICE_REVIEW_LABEL: &str = "Review approval";

/// Draws the notices in the bottom-right corner without blocking the window.
/// `project_name` names a project root for the text.
pub fn mcp_approval_notices(
    ctx: &egui::Context,
    notices: &[McpApprovalNotice],
    project_name: impl Fn(&Path) -> Option<String>,
) -> Option<McpNoticeAction> {
    if notices.is_empty() {
        return None;
    }
    let mut action = None;
    egui::Area::new(egui::Id::new("mcp-approval-notices"))
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-SP_4, -SP_4))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.set_max_width(360.0_f32.min(ctx.viewport_rect().width() - SP_4 * 2.0));
            for notice in notices.iter().take(SHOWN) {
                ui.push_id(
                    ("mcp-notice", &notice.project_root, &notice.server_id),
                    |ui| {
                        notice_card(ui, notice, &project_name, &mut action);
                    },
                );
                ui.add_space(SP_2);
            }
            if notices.len() > SHOWN {
                ui.label(muted(format!("{} more MCP notices", notices.len() - SHOWN)));
            }
        });
    action
}

fn notice_card(
    ui: &mut Ui,
    notice: &McpApprovalNotice,
    project_name: &impl Fn(&Path) -> Option<String>,
    action: &mut Option<McpNoticeAction>,
) {
    surface_frame(palette().SURFACE_RAISED)
        .stroke(egui::Stroke::new(1.0, palette().WARNING))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            let project = project_name(&notice.project_root)
                .unwrap_or_else(|| notice.project_root.display().to_string());
            ui.label(semibold(format!("MCP server {} was skipped", notice.server_id)));
            ui.label(muted(format!(
                "A new run in {project} started without it because it is not approved. The run continued. After approval it is used from the next new run; conversations already running keep their tools."
            )));
            ui.horizontal(|ui| {
                let review = primary_button(ui, NOTICE_REVIEW_LABEL);
                accessible(
                    &review,
                    &format!("{NOTICE_REVIEW_LABEL} {}", notice.server_id),
                );
                if review.clicked() {
                    *action = Some(McpNoticeAction::Review(notice.clone()));
                }
                let dismiss = ui.button("Dismiss");
                accessible(&dismiss, &format!("Dismiss notice {}", notice.server_id));
                if dismiss.clicked() {
                    *action = Some(McpNoticeAction::Dismiss(notice.clone()));
                }
            });
        });
}
