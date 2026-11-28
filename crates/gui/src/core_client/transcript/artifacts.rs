//! The card a conversation shows where its owner presented artifacts: the presentation's
//! title and caption, and for each artifact its title, caption, type and an Open button.
//! Nothing is read to draw it, and nothing but a reference leaves it: Open asks the shell,
//! which hands an image to the artifacts feature and a page to the browser preview.
use std::hash::{Hash, Hasher};

use egui::{Label, RichText, Ui};
use workbench_protocol::{
    ids::ThreadId,
    namespaces::artifacts::{ArtifactPresentationView, ArtifactRef, PresentedArtifactView},
    views::CommittedMessage,
};

use super::Effect;
use super::cards::Env;
use crate::theme::icons;
use crate::theme::text::{medium, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::{accessible, ghost, soft_frame};

const HTML: &str = "text/html";

/// What Open on a card asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenArtifact {
    pub reference: ArtifactRef,
    pub title: String,
    pub media_type: String,
}

impl OpenArtifact {
    /// A page goes to the browser preview; every other artifact to the artifacts feature.
    pub fn is_page(&self) -> bool {
        self.media_type == HTML
    }
}

/// The reference Open sends: the artifact of the card, in the conversation the card was
/// committed to and the project that conversation belongs to. `None` when the conversation's
/// project is not known, which leaves Open off.
fn open_request(
    project: Option<&workbench_protocol::ids::ProjectId>,
    thread: &ThreadId,
    item: &PresentedArtifactView,
) -> Option<OpenArtifact> {
    let reference = ArtifactRef {
        project: project?.clone(),
        thread: thread.clone(),
        artifact_id: item.artifact_id.clone(),
        digest: item.digest.clone(),
    };
    reference.validate().ok()?;
    Some(OpenArtifact {
        reference,
        title: item.title.clone(),
        media_type: item.media_type.clone(),
    })
}

fn media_icon(media_type: &str) -> &'static str {
    if media_type == HTML {
        icons::FILE_HTML
    } else {
        icons::IMAGE
    }
}

/// What changes the look of a card, for the view that keeps the messages in place. The shown
/// text itself is hashed: the same length with a line break elsewhere changes a card's height.
pub(super) fn fingerprint(presentation: &ArtifactPresentationView, hasher: &mut impl Hasher) {
    presentation.title.hash(hasher);
    presentation.caption.hash(hasher);
    presentation.artifacts.len().hash(hasher);
    for item in &presentation.artifacts {
        item.title.hash(hasher);
        item.caption.hash(hasher);
        item.media_type.hash(hasher);
    }
}

pub(super) fn card(ui: &mut Ui, env: &mut Env<'_, '_>, message: &CommittedMessage) {
    let Some(presentation) = &message.presentation else {
        return;
    };
    let project = env
        .cache
        .threads
        .get(message.thread.as_str())
        .map(|thread| &thread.project);
    ui.push_id(
        ("artifacts", message.run.as_str(), message.index.get()),
        |ui| {
            soft_frame(palette().SURFACE).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let heading = presentation.title.as_deref().unwrap_or("Artifacts");
                ui.label(medium(icons::with_icon(icons::IMAGES, heading)));
                if let Some(caption) = presentation.caption.as_deref().filter(|c| !c.is_empty()) {
                    ui.add(Label::new(muted(caption)).wrap());
                }
                if presentation.artifacts.is_empty() {
                    ui.label(muted("No artifacts were presented."));
                }
                for (at, item) in presentation.artifacts.iter().enumerate() {
                    ui.push_id(at, |ui| {
                        if row(ui, item, project.is_some()) {
                            match open_request(project, &message.thread, item) {
                                Some(request) => env.effects.push(Effect::OpenArtifact(request)),
                                None => env.effects.push(Effect::Status(
                                    "This artifact cannot be opened.".to_owned(),
                                )),
                            }
                        }
                    });
                }
            });
        },
    );
}

/// One artifact; true when its Open was pressed.
fn row(ui: &mut Ui, item: &PresentedArtifactView, can_open: bool) -> bool {
    let mut pressed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(media_icon(&item.media_type)).color(palette().TEXT_MUTED));
        ui.label(medium(item.title.as_str()).color(palette().TEXT));
        ui.label(muted(item.media_type.as_str()).size(FONT_SMALL));
        let button = ui.add_enabled(
            can_open,
            ghost(RichText::new(icons::with_icon(
                icons::ARROW_SQUARE_OUT,
                "Open",
            ))),
        );
        accessible(&button, &format!("Open {}", item.title));
        let button = if can_open {
            button
        } else {
            button.on_disabled_hover_text("The conversation's project is not available.")
        };
        pressed = button.clicked();
    });
    if let Some(caption) = item.caption.as_deref().filter(|c| !c.is_empty()) {
        ui.add(Label::new(muted(caption).size(FONT_SMALL)).wrap());
    }
    pressed
}

#[cfg(test)]
mod tests;
