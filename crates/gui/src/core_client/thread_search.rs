//! The Ctrl+K thread search: a modal that narrows the threads this client already holds by
//! title and opens the one chosen (ADR 0045, E5).
//!
//! This is the client-side first step. The core's `search_threads {project, query, cursor}`
//! does not exist yet; when it does, [`search_titles`] is the one place to replace with a
//! request, and its caller keeps showing whatever rows it returns.
use workbench_protocol::{
    ids::{ProjectId, ThreadId},
    views::ThreadSummary,
};

use super::cache::Cache;
use super::features::workspace::SidebarModel;
use crate::theme::text::{h3, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::surface_frame;

const QUERY_HINT: &str = "Search threads by title";
pub(super) const QUERY_LABEL: &str = "Thread search query";
const NO_MATCHES: &str = "No matching threads";
const RESULT_MAX_HEIGHT: f32 = 360.0;
/// Rows drawn at most: a long list is narrowed by typing, not by scrolling.
const MAX_ROWS: usize = 50;

/// One thread the search offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub project: ProjectId,
    pub thread: ThreadId,
    pub title: String,
    /// Known only once the thread's detail is read.
    pub archived: bool,
}

/// The threads of `project` (every project when `None`) whose title holds each word of
/// `query`, ignoring case, newest first. A blank query matches every thread. A thread
/// shows its detail's title once read, as the sidebar does.
///
/// The replacement point for the core's `search_threads`: same inputs, rows out.
pub fn search_titles(
    cache: &Cache,
    model: Option<&SidebarModel>,
    project: Option<&ProjectId>,
    query: &str,
) -> Vec<Row> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut found: Vec<(&ThreadSummary, Row)> = cache
        .threads
        .values()
        .filter(|summary| project.is_none_or(|project| &summary.project == project))
        .filter_map(|summary| {
            let detail = model.and_then(|model| model.details.get(summary.id.as_str()));
            let title = detail.map_or(summary.title.as_str(), |detail| detail.title.as_str());
            let lowered = title.to_lowercase();
            words.iter().all(|word| lowered.contains(word)).then(|| {
                let row = Row {
                    project: summary.project.clone(),
                    thread: summary.id.clone(),
                    title: title.to_owned(),
                    archived: detail.is_some_and(|detail| detail.archived),
                };
                (summary, row)
            })
        })
        .collect();
    found.sort_by(|(left, _), (right, _)| {
        (right.updated_at, &right.id).cmp(&(left.updated_at, &left.id))
    });
    found.into_iter().map(|(_, row)| row).collect()
}

/// Keys pressed this frame that act on the search.
#[derive(Debug, Clone, Copy, Default)]
struct Keys {
    down: bool,
    up: bool,
    enter: bool,
    escape: bool,
}

/// What a frame of the open modal decided.
pub enum Outcome {
    Open(Row),
    Close,
}

#[derive(Default)]
pub struct ThreadSearch {
    open: bool,
    /// The project the search was opened in: the selected one, else every project.
    project: Option<ProjectId>,
    query: String,
    selected: usize,
    focus_pending: bool,
    /// An IME conversion is open in the query field.
    composing: bool,
    keys: Keys,
}

impl ThreadSearch {
    pub fn is_open(&self) -> bool {
        self.open
    }

    fn open(&mut self, project: Option<ProjectId>) {
        *self = Self {
            open: true,
            project,
            focus_pending: true,
            ..Self::default()
        };
    }

    fn close(&mut self) {
        *self = Self::default();
    }

    /// Before the dock is drawn: Ctrl+K opens or closes the search, and while it is open
    /// its keys leave the frame's input, so Esc closes it instead of stopping the chat
    /// behind it and the arrows do not move the composer's cursor.
    pub fn begin_frame(&mut self, ctx: &egui::Context, selected_project: Option<&ProjectId>) {
        // Another modal owns input: Ctrl+K is left alone, so two modals never stack.
        let can_toggle = self.open || ctx.memory(|memory| memory.top_modal_layer()).is_none();
        if can_toggle
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::K))
        {
            if self.open {
                self.close();
            } else {
                self.open(selected_project.cloned());
            }
        }
        if !self.open {
            return;
        }
        let (for_ime, composing) = ctx.input(|input| ime_keys(&input.events, self.composing));
        self.composing = composing;
        let none = egui::Modifiers::NONE;
        ctx.input_mut(|input| {
            let keys = Keys {
                down: input.consume_key(none, egui::Key::ArrowDown),
                up: input.consume_key(none, egui::Key::ArrowUp),
                enter: input.consume_key(none, egui::Key::Enter),
                escape: input.consume_key(none, egui::Key::Escape),
            };
            if !for_ime {
                self.keys = keys;
            }
        });
    }

    /// Draws the modal when open. `Open` and `Close` have already closed it.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        cache: &Cache,
        model: Option<&SidebarModel>,
    ) -> Option<Outcome> {
        if !self.open {
            return None;
        }
        let keys = std::mem::take(&mut self.keys);
        let mut outcome = keys.escape.then_some(Outcome::Close);
        let focus = std::mem::take(&mut self.focus_pending);
        let query_before = self.query.clone();
        let rows = search_titles(cache, model, self.project.as_ref(), &self.query);
        let shown = rows.len().min(MAX_ROWS);
        // The rows were found for the text as it was before this frame's typing, so a
        // key that arrives with a keystroke acts on those rows, as the person saw them.
        self.selected = step(self.selected, shown, keys.up, keys.down);
        let mut clicked = None;
        let modal = egui::Modal::new(egui::Id::new("core-thread-search"))
            .backdrop_color(palette().OVERLAY)
            .frame(surface_frame(palette().SURFACE_RAISED))
            .show(ctx, |ui| {
                ui.set_width(
                    (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
                );
                ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
                ui.label(h3("Search threads"));
                let query = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text(QUERY_HINT)
                        .desired_width(f32::INFINITY),
                );
                query.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, QUERY_LABEL)
                });
                if focus {
                    query.request_focus();
                }
                if rows.is_empty() {
                    ui.label(muted(NO_MATCHES));
                }
                let moved = keys.up || keys.down;
                egui::ScrollArea::vertical()
                    .max_height(RESULT_MAX_HEIGHT)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (index, row) in rows.iter().take(shown).enumerate() {
                            let label = if row.archived {
                                format!("{} (archived)", row.title)
                            } else {
                                row.title.clone()
                            };
                            let response = ui.selectable_label(index == self.selected, &label);
                            if moved && index == self.selected {
                                response.scroll_to_me(None);
                            }
                            if response.clicked() {
                                clicked = Some(row.clone());
                            }
                        }
                    });
                if rows.len() > shown {
                    ui.label(muted(format!(
                        "Showing the newest {shown} of {} matches. Type to narrow them.",
                        rows.len()
                    )));
                }
            });
        if self.query != query_before {
            self.selected = 0;
        } else if keys.enter && outcome.is_none() {
            outcome = rows.get(self.selected).cloned().map(Outcome::Open);
        }
        if let Some(row) = clicked {
            outcome = Some(Outcome::Open(row));
        }
        if outcome.is_none() && modal.should_close() {
            outcome = Some(Outcome::Close);
        }
        if outcome.is_some() {
            self.close();
        }
        outcome
    }
}

/// Whether this frame's Enter, Esc and arrows belong to an IME conversion, and whether one
/// is still open after the frame. They do while a conversion was open or is being edited,
/// and in the frame that commits it. An empty preedit alone is no conversion: a conversion
/// ends with one, and Fcitx5 repeats it after every commit until the field loses focus, so
/// it must not hold the keys back.
fn ime_keys(events: &[egui::Event], mut composing: bool) -> (bool, bool) {
    let mut for_ime = composing;
    for event in events {
        if let egui::Event::Ime(ime) = event {
            match ime {
                egui::ImeEvent::Preedit { text, .. } if !text.is_empty() => {
                    composing = true;
                    for_ime = true;
                }
                egui::ImeEvent::Commit(_) => {
                    composing = false;
                    for_ime = true;
                }
                egui::ImeEvent::Preedit { .. } => composing = false,
                _ => {}
            }
        }
    }
    (for_ime, composing)
}

/// The selection after Up or Down, kept within `rows` rows.
fn step(selected: usize, rows: usize, up: bool, down: bool) -> usize {
    let last = rows.saturating_sub(1);
    let selected = selected.min(last);
    match (up, down) {
        (true, false) => selected.saturating_sub(1),
        (false, true) => (selected + 1).min(last),
        _ => selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cache_of(threads: &[(&str, &str, &str, u64)]) -> Cache {
        let mut cache = Cache::default();
        for (id, project, title, updated) in threads {
            let summary: ThreadSummary = serde_json::from_value(json!({
                "id": id, "project": project, "title": title, "state": "idle",
                "revision": "1", "updated_at": updated.to_string(),
            }))
            .unwrap();
            cache.threads.insert((*id).into(), summary);
        }
        cache
    }

    fn titles(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.title.as_str()).collect()
    }

    // Given: threads in two projects with different titles and ages
    // When: the query holds words, in another case or order, or is blank
    // Then: only threads whose title holds every word match, newest first
    #[test]
    fn titles_are_narrowed_by_every_word_ignoring_case_newest_first() {
        let cache = cache_of(&[
            ("a", "p1", "Fix login redirect", 10),
            ("b", "p1", "Refactor storage writer", 30),
            ("c", "p2", "Login page copy", 20),
        ]);
        assert_eq!(
            titles(&search_titles(&cache, None, None, "  ")),
            [
                "Refactor storage writer",
                "Login page copy",
                "Fix login redirect"
            ]
        );
        assert_eq!(
            titles(&search_titles(&cache, None, None, "LOGIN")),
            ["Login page copy", "Fix login redirect"]
        );
        assert_eq!(
            titles(&search_titles(&cache, None, None, "redirect fix")),
            ["Fix login redirect"]
        );
        assert!(search_titles(&cache, None, None, "login storage").is_empty());
    }

    // Given: the same threads
    // When: the search is scoped to one project
    // Then: other projects' threads are not offered, and each row names its own project
    #[test]
    fn a_project_scope_leaves_out_the_other_projects() {
        let cache = cache_of(&[
            ("a", "p1", "Fix login redirect", 10),
            ("c", "p2", "Login page copy", 20),
        ]);
        let p2 = ProjectId::new("p2").unwrap();
        let rows = search_titles(&cache, None, Some(&p2), "login");
        assert_eq!(titles(&rows), ["Login page copy"]);
        assert_eq!(rows[0].project, p2);
        assert_eq!(rows[0].thread.as_str(), "c");
    }

    // Given: a thread whose detail, read later, carries a renamed title and is archived
    // When: searched by the new title
    // Then: it matches that title and not the summary's, and is marked archived
    #[test]
    fn a_read_detail_supplies_the_title_and_the_archived_mark() {
        let cache = cache_of(&[("a", "p1", "Old name", 10)]);
        let mut model = SidebarModel::default();
        model.details.insert(
            "a".into(),
            serde_json::from_value(json!({
                "thread": "a", "project": "p1", "title": "Renamed spike",
                "extra_read_dirs": [], "archived": true, "pinned": false, "revision": "2",
            }))
            .unwrap(),
        );
        let rows = search_titles(&cache, Some(&model), None, "spike");
        assert_eq!(titles(&rows), ["Renamed spike"]);
        assert!(rows[0].archived);
        assert!(search_titles(&cache, Some(&model), None, "old").is_empty());
    }

    fn preedit(text: &str) -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Preedit {
            text: text.into(),
            active_range_chars: None,
        })
    }

    // Given: no conversion open, or one open
    // When: a frame carries an empty preedit alone, a non-empty one, or a commit
    // Then: the keys are held back only for an open conversion, an edited one and the
    //       committing frame; the empty preedit Fcitx5 repeats after a commit holds nothing
    #[test]
    fn only_a_conversion_holds_the_keys_back_not_an_empty_preedit() {
        let commit = egui::Event::Ime(egui::ImeEvent::Commit("x".into()));
        assert_eq!(ime_keys(&[], false), (false, false));
        assert_eq!(ime_keys(&[preedit("")], false), (false, false));
        assert_eq!(ime_keys(&[preedit("")], true), (true, false));
        assert_eq!(ime_keys(&[preedit("に")], false), (true, true));
        assert_eq!(ime_keys(&[], true), (true, true));
        assert_eq!(
            ime_keys(&[preedit(""), commit.clone()], true),
            (true, false)
        );
        assert_eq!(ime_keys(&[commit], false), (true, false));
    }

    /// What `begin_frame` takes from one frame holding `events`, with the search open.
    fn keys_of_frame(events: Vec<egui::Event>) -> Keys {
        let ctx = egui::Context::default();
        let mut search = ThreadSearch::default();
        search.open(None);
        let raw = egui::RawInput {
            events,
            ..egui::RawInput::default()
        };
        ctx.run_ui(raw, |ui| search.begin_frame(ui.ctx(), None))
            .drop_without_applying_deltas();
        search.keys
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    // Given: the search open, and a frame in which Esc comes with the empty preedit that
    //        Fcitx5 repeats after a commit
    // When: the frame is taken
    // Then: Esc acts on the search; with a non-empty preedit or a commit in the frame it
    //       belongs to the conversion instead
    #[test]
    fn esc_acts_on_the_search_beside_an_empty_preedit_but_not_beside_a_conversion() {
        let commit = egui::Event::Ime(egui::ImeEvent::Commit("に".into()));
        assert!(keys_of_frame(vec![preedit(""), key(egui::Key::Escape)]).escape);
        assert!(keys_of_frame(vec![key(egui::Key::Escape)]).escape);
        assert!(!keys_of_frame(vec![preedit("に"), key(egui::Key::Escape)]).escape);
        assert!(!keys_of_frame(vec![commit, key(egui::Key::Enter)]).enter);
    }

    // Given: three rows
    // When: Up and Down move the selection past either end, or a stale index is given
    // Then: it stays on the first or last row
    #[test]
    fn the_selection_moves_within_the_rows() {
        assert_eq!(step(0, 3, false, true), 1);
        assert_eq!(step(2, 3, false, true), 2);
        assert_eq!(step(0, 3, true, false), 0);
        assert_eq!(step(1, 3, true, false), 0);
        assert_eq!(step(7, 3, false, false), 2);
        assert_eq!(step(0, 0, false, true), 0);
    }
}
