//! Desktop feature of the `memory` area, read side (PR 13): the lessons of the
//! selected project, filtered by status, favorite and words, and one lesson
//! opened for its text and evidence. PR 14 adds the updates to this file.
//!
//! The reads are operations ([`super::diagnostics::read`]). An answer for a project
//! or a lesson that is no longer the one shown is dropped, and a lesson's body is
//! only what the core cut for the wire.
use std::collections::BTreeMap;

use super::diagnostics::read::{Phase, Read, phase_line};
use super::layout::paged_list;
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use workbench_protocol::{
    events::UiEvent,
    handshake::Capability,
    ids::{MemoryId, PageCursor, ProjectId},
    management::MemoryStatus,
    namespaces::{
        diagnostics::BoundedText,
        memory::{
            GetMemory, ListMemory, MemoryDetail, MemoryEvent, MemoryQuery, MemoryReply,
            MemoryScope, MemorySummary,
        },
    },
    queries::{PageRequest, Query, QueryReply},
};

const STATUSES: [(Option<MemoryStatus>, &str); 5] = [
    (None, "すべて"),
    (Some(MemoryStatus::Candidate), "候補"),
    (Some(MemoryStatus::Validated), "検証済み"),
    (Some(MemoryStatus::Promoted), "昇格済み"),
    (Some(MemoryStatus::Rejected), "却下"),
];

#[derive(Debug, Clone, Copy)]
enum Pending {
    List { generation: u64, append: bool },
    Detail { generation: u64 },
}

#[derive(Default)]
pub struct MemoryFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    project: Option<ProjectId>,
    status: Option<MemoryStatus>,
    favorite_only: bool,
    words: String,
    list_read: Read,
    list_request: Option<ListMemory>,
    items: Vec<MemorySummary>,
    next_cursor: Option<PageCursor>,
    list_due: bool,
    more_requested: bool,
    selected: Option<MemoryId>,
    detail_read: Read,
    detail_request: Option<GetMemory>,
    detail: Option<MemoryDetail>,
    detail_due: bool,
}

fn status_label(status: MemoryStatus) -> &'static str {
    STATUSES
        .iter()
        .find(|(candidate, _)| *candidate == Some(status))
        .map_or("", |(_, label)| label)
}

fn scope_label(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Project => "プロジェクト",
        MemoryScope::Harness => "ハーネス",
        MemoryScope::User => "ユーザー",
    }
}

fn body(ui: &mut egui::Ui, heading: &str, text: &BoundedText) {
    ui.strong(heading);
    ui.label(&text.text);
    if text.truncated {
        ui.label(format!(
            "(先頭 {} バイトのみ表示 / 全体 {} バイト)",
            text.text.len(),
            text.total_bytes
        ));
    }
}

impl MemoryFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn switch_project(&mut self, project: Option<ProjectId>) {
        self.project = project;
        self.list_read.forget();
        self.detail_read.forget();
        self.list_request = None;
        self.detail_request = None;
        self.items.clear();
        self.next_cursor = None;
        self.selected = None;
        self.detail = None;
        self.list_due = self.project.is_some();
        self.detail_due = false;
    }

    fn begin_list(&mut self, ctx: &mut FeatureContext<'_>, project: ProjectId) {
        self.items.clear();
        self.next_cursor = None;
        let request = ListMemory {
            operation_id: self.list_read.begin("memory-list"),
            project,
            status: self.status,
            favorite: self.favorite_only.then_some(true),
            text: Some(self.words.trim().to_owned()).filter(|words| !words.is_empty()),
            page: PageRequest::default(),
        };
        self.ask_list(ctx, request, None);
    }

    fn ask_list(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        mut request: ListMemory,
        cursor: Option<PageCursor>,
    ) {
        let append = cursor.is_some();
        request.page = PageRequest {
            cursor,
            ..PageRequest::default()
        };
        let tag = self.tag(Pending::List {
            generation: self.list_read.generation(),
            append,
        });
        ctx.query(tag, Query::Memory(MemoryQuery::ListMemory(request.clone())));
        request.page = PageRequest::default();
        self.list_request = Some(request);
    }

    fn begin_detail(&mut self, ctx: &mut FeatureContext<'_>, project: ProjectId, id: MemoryId) {
        self.detail = None;
        let request = GetMemory {
            operation_id: self.detail_read.begin("memory-detail"),
            project,
            id,
        };
        self.ask_detail(ctx, request);
    }

    fn ask_detail(&mut self, ctx: &mut FeatureContext<'_>, request: GetMemory) {
        let tag = self.tag(Pending::Detail {
            generation: self.detail_read.generation(),
        });
        ctx.query(tag, Query::Memory(MemoryQuery::GetMemory(request.clone())));
        self.detail_request = Some(request);
    }
}

impl Feature for MemoryFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Memory
    }

    fn title(&self) -> &'static str {
        "Memory"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Queries],
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.list_read.forget();
        self.detail_read.forget();
        self.items.clear();
        self.next_cursor = None;
        self.detail = None;
        self.list_due = self.project.is_some();
        self.detail_due = self.selected.is_some();
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::Memory(MemoryEvent::MemoryOperationChanged { view }) = event else {
            return;
        };
        self.list_read.event(view);
        self.detail_read.event(view);
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        let FeatureReply::Query(result) = reply else {
            return;
        };
        let reply = match result {
            Ok(QueryReply::Memory(reply)) => reply,
            Ok(_) => return,
            Err(error) => {
                match pending {
                    Pending::List { generation, .. } => {
                        self.list_read.fail(generation, error.message);
                    }
                    Pending::Detail { generation } => {
                        self.detail_read.fail(generation, error.message);
                    }
                }
                return;
            }
        };
        match (pending, reply) {
            (Pending::List { generation, append }, MemoryReply::List(outcome)) => {
                if let Some(page) =
                    self.list_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    if append {
                        for item in page.items {
                            if !self
                                .items
                                .iter()
                                .any(|known| known.state.id == item.state.id)
                            {
                                self.items.push(item);
                            }
                        }
                    } else {
                        self.items = page.items;
                    }
                    self.next_cursor = page.next_cursor;
                }
            }
            (Pending::Detail { generation }, MemoryReply::Detail(outcome)) => {
                if let Some(detail) =
                    self.detail_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    self.detail = Some(detail);
                }
            }
            _ => {}
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let selected_project = ctx.selection().project.clone();
        if selected_project != self.project {
            self.switch_project(selected_project);
        }
        let Some(project) = self.project.clone() else {
            ui.label("プロジェクトを選択すると教訓を表示します。");
            return;
        };

        ui.horizontal_wrapped(|ui| {
            let before = (self.status, self.favorite_only);
            egui::ComboBox::from_id_salt("memory-status")
                .selected_text(
                    STATUSES
                        .iter()
                        .find(|(status, _)| *status == self.status)
                        .map_or("", |(_, label)| label),
                )
                .show_ui(ui, |ui| {
                    for (status, label) in STATUSES {
                        ui.selectable_value(&mut self.status, status, label);
                    }
                });
            ui.checkbox(&mut self.favorite_only, "お気に入りのみ");
            if before != (self.status, self.favorite_only) {
                self.list_due = true;
            }
            // Closed until asked for: every available feature is stacked in one column,
            // so an input drawn all the time would share the screen with every other one.
            egui::CollapsingHeader::new("検索語で絞り込む")
                .default_open(false)
                .show(ui, |ui| {
                    let response =
                        ui.add(egui::TextEdit::singleline(&mut self.words).hint_text("検索語"));
                    let submitted = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if ui.button("検索").clicked() || submitted {
                        self.list_due = true;
                    }
                });
        });

        if std::mem::take(&mut self.list_due) {
            self.begin_list(ctx, project.clone());
        } else if self.list_read.take_ask_again()
            && let Some(request) = self.list_request.clone()
        {
            self.ask_list(ctx, request, None);
        }
        if std::mem::take(&mut self.more_requested)
            && let Some(request) = self.list_request.clone()
        {
            let cursor = self.next_cursor.clone();
            self.ask_list(ctx, request, cursor);
        }
        if std::mem::take(&mut self.detail_due)
            && let Some(id) = self.selected.clone()
        {
            self.begin_detail(ctx, project.clone(), id);
        } else if self.detail_read.take_ask_again()
            && let Some(request) = self.detail_request.clone()
        {
            self.ask_detail(ctx, request);
        }

        if let Some(line) = phase_line(self.list_read.phase()) {
            ui.label(line);
        }
        if self.list_read.phase() == &Phase::Ready && self.items.is_empty() {
            ui.label("条件に合う教訓はありません。");
        }
        let mut open = None;
        let items = &self.items;
        let more = paged_list(
            ui,
            "memory-list",
            self.next_cursor.is_some().then_some("さらに読み込む"),
            |ui| {
                for item in items {
                    let star = if item.state.favorite { "★ " } else { "" };
                    let label = format!(
                        "{star}{} · {} · {}",
                        item.state.id.as_str(),
                        status_label(item.state.status),
                        item.preview
                    );
                    if ui.button(label).clicked() {
                        open = Some(item.state.id.clone());
                    }
                }
            },
        );
        if more {
            self.more_requested = true;
        }
        if let Some(id) = open {
            self.selected = Some(id);
            self.detail_due = true;
        }

        if self.selected.is_some() {
            ui.separator();
            if let Some(line) = phase_line(self.detail_read.phase()) {
                ui.label(line);
            }
            if let Some(detail) = &self.detail {
                ui.strong(format!(
                    "{} · {} · {}{}",
                    detail.state.id.as_str(),
                    status_label(detail.state.status),
                    scope_label(detail.scope),
                    if detail.state.favorite {
                        " · お気に入り"
                    } else {
                        ""
                    }
                ));
                ui.label(format!("タスク: {}", detail.task_id));
                body(ui, "内容", &detail.content);
                body(ui, "根拠", &detail.evidence);
            }
        }
    }
}

#[cfg(test)]
mod tests;
