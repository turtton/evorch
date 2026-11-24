//! The composer's completion keys and candidate list, shared by the embedded
//! workbench's composer and the core client's.

use crate::composer_input::{CompletionItem, CompletionKind};
use crate::theme::icons;
use crate::theme::tokens::palette;

/// Tab / Shift+Tab and arrows cycle the highlight, Tab accepts a sole
/// candidate, and Enter accepts only when that changes the draft, so a fully
/// typed `/new` still sends. `tabs` are the Tab presses captured before egui
/// saw them (`true` with Shift).
pub fn completion_keys(
    ui: &mut egui::Ui,
    items: &[CompletionItem],
    input: &str,
    selected: &mut usize,
    tabs: Vec<bool>,
    ime_composing: bool,
    focused: bool,
) -> Option<CompletionItem> {
    if items.is_empty() || ime_composing {
        return None;
    }
    let len = items.len();
    let mut current = *selected;
    let mut accept = len == 1 && tabs.iter().any(|shift| !shift);
    if len > 1 {
        for shift in tabs {
            current = if shift {
                (current + len - 1) % len
            } else {
                (current + 1) % len
            };
        }
    }
    if focused && !accept {
        ui.input_mut(|keys| {
            if consume_bare(keys, egui::Key::ArrowDown) {
                current = (current + 1) % len;
            }
            if consume_bare(keys, egui::Key::ArrowUp) {
                current = (current + len - 1) % len;
            }
            accept = items[current].changes(input) && consume_bare(keys, egui::Key::Enter);
        });
    }
    *selected = current;
    accept.then(|| items[current].clone())
}

/// `consume_key(Modifiers::NONE, ..)` also takes Shift+Enter and Shift+arrows,
/// which belong to the editor.
fn consume_bare(keys: &mut egui::InputState, key: egui::Key) -> bool {
    let before = keys.events.len();
    keys.events.retain(|event| {
        !matches!(event, egui::Event::Key { key: pressed, pressed: true, modifiers, .. }
            if *pressed == key && modifiers.is_none())
    });
    keys.events.len() != before
}

pub fn completion_list(
    ui: &mut egui::Ui,
    items: &[CompletionItem],
    selected: usize,
) -> Option<CompletionItem> {
    let mut picked = None;
    for (index, item) in items.iter().enumerate() {
        ui.push_id(("completion", index), |ui| {
            ui.horizontal(|ui| {
                let icon = match item.kind {
                    CompletionKind::Command => icons::TERMINAL_WINDOW,
                    CompletionKind::File => icons::FILE,
                    CompletionKind::Dir => icons::FOLDER,
                    CompletionKind::Skill => icons::SPARKLE,
                };
                ui.label(egui::RichText::new(icon).color(palette().TEXT_MUTED));
                // The theme's selection fill hides default text; keep the label legible.
                let response = ui.selectable_label(
                    index == selected,
                    egui::RichText::new(&item.label).color(palette().TEXT),
                );
                if index == selected {
                    response.scroll_to_me(None);
                }
                if response.clicked() {
                    picked = Some(item.clone());
                }
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&item.detail)
                            .small()
                            .color(palette().TEXT_MUTED),
                    )
                    .truncate(),
                );
            });
        });
    }
    picked
}
