//! The card of a presentation as the conversation draws it, and the request Open makes.
use std::collections::BTreeSet;

use egui_kittest::{Harness, kittest::Queryable};
use workbench_protocol::ids::{ProjectId, ThreadId};

use super::super::tests::{State, history};
use super::super::{Scope, TranscriptView, transcript};
use super::*;
use crate::core_client::cache::Cache;

fn card(run: &str, index: u64, thread: &str, artifacts: serde_json::Value) -> CommittedMessage {
    serde_json::from_value(serde_json::json!({
        "thread": thread, "run": run, "index": index.to_string(), "role": "artifacts",
        "content": {"type": "inline", "payload": {"text": ""}},
        "presentation": {"title": "Compare", "caption": "Two renders", "artifacts": artifacts},
    }))
    .unwrap()
}

fn artifact(id: u8, title: &str, media_type: &str, caption: Option<&str>) -> serde_json::Value {
    let mut item = serde_json::json!({
        "artifact_id": format!("artifact-{id:032x}"), "digest": format!("{id:064x}"),
        "title": title, "media_type": media_type,
    });
    if let Some(caption) = caption {
        item["caption"] = caption.into();
    }
    item
}

fn summary(id: &str, project: &str) -> workbench_protocol::views::ThreadSummary {
    serde_json::from_value(serde_json::json!({
        "id": id, "project": project, "title": "T", "state": "idle",
        "revision": "1", "updated_at": "1",
    }))
    .unwrap()
}

fn cache(rows: Vec<CommittedMessage>, threads: &[(&str, &str)]) -> Cache {
    let mut cache = history(rows);
    for (id, project) in threads {
        cache.threads.insert((*id).into(), summary(id, project));
    }
    cache
}

fn harness(cache: Cache) -> Harness<'static, State> {
    let state = State {
        cache,
        requested: BTreeSet::new(),
        effects: Vec::new(),
    };
    Harness::new_ui_state(
        |ui, state: &mut State| {
            let thread = ThreadId::new("thr").unwrap();
            transcript(
                ui,
                &mut state.requested,
                TranscriptView {
                    cache: &state.cache,
                    scope: Scope::Thread(&thread),
                    base: None,
                    chips: None,
                },
                |_| {},
                |_, _| {},
                &mut state.effects,
            );
        },
        state,
    )
}

fn opened(state: &mut State) -> Vec<OpenArtifact> {
    std::mem::take(&mut state.effects)
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::OpenArtifact(open) => Some(open),
            _ => None,
        })
        .collect()
}

// Given: a presentation of an image and a page, the image listed twice, with captions
// Then: the card shows its title and caption and each artifact's title, caption and type with
//       its own Open, in the order they were presented
#[test]
fn a_card_shows_what_was_presented_in_order() {
    let row = card(
        "r",
        3,
        "thr",
        serde_json::json!([
            artifact(1, "Before", "image/png", Some("the old one")),
            artifact(2, "Report", "text/html", None),
            artifact(1, "Before again", "image/png", None),
        ]),
    );
    let mut harness = harness(cache(vec![row], &[("thr", "p-1")]));
    harness.run_steps(2);
    harness.get_by_label_contains("Compare");
    harness.get_by_label("Two renders");
    harness.get_by_label("the old one");
    harness.get_by_label("Report");
    harness.get_by_label("text/html");
    assert_eq!(harness.get_all_by_label("image/png").count(), 2);
    assert_eq!(harness.get_all_by_label_contains("Open ").count(), 3);
    let titles = ["Before", "Report", "Before again"];
    let ys: Vec<_> = titles
        .iter()
        .map(|title| harness.get_by_label(title).rect().min.y)
        .collect();
    assert!(ys.windows(2).all(|w| w[0] < w[1]), "{ys:?}");
}

// Given: a presentation with no artifacts
// Then: the card stays and says so
#[test]
fn an_empty_presentation_keeps_its_card() {
    let mut harness = harness(cache(
        vec![card("r", 0, "thr", serde_json::json!([]))],
        &[("thr", "p-1")],
    ));
    harness.run_steps(2);
    harness.get_by_label("No artifacts were presented.");
}

// Given: two conversations of different projects and a card committed to the first
// When: Open is pressed on the second of three artifacts
// Then: the request names that artifact in the card's conversation and in that conversation's
//       project, whatever else is selected
#[test]
fn open_names_the_artifact_of_the_card_in_its_threads_project() {
    let row = card(
        "r",
        1,
        "thr",
        serde_json::json!([
            artifact(1, "Before", "image/png", None),
            artifact(2, "Report", "text/html", None),
            artifact(3, "After", "image/png", None),
        ]),
    );
    let mut harness = harness(cache(vec![row], &[("thr", "p-1"), ("other", "p-2")]));
    harness.run_steps(2);
    harness.get_by_label("Open Report").click();
    harness.run_steps(2);
    let requests = opened(harness.state_mut());
    assert_eq!(
        requests,
        [OpenArtifact {
            reference: ArtifactRef {
                project: ProjectId::new("p-1").unwrap(),
                thread: ThreadId::new("thr").unwrap(),
                artifact_id: format!("artifact-{:032x}", 2),
                digest: format!("{:064x}", 2),
            },
            title: "Report".into(),
            media_type: "text/html".into(),
        }]
    );
    assert!(requests[0].is_page());
}

// Given: a card whose conversation the client has no summary of
// Then: Open is off, and nothing is requested
#[test]
fn open_is_off_without_the_conversations_project() {
    let row = card(
        "r",
        1,
        "thr",
        serde_json::json!([artifact(1, "Before", "image/png", None)]),
    );
    let mut harness = harness(cache(vec![row], &[("other", "p-2")]));
    harness.run_steps(2);
    harness.get_by_label("Open Before").click();
    harness.run_steps(2);
    assert!(opened(harness.state_mut()).is_empty());
    assert!(harness.state().effects.is_empty());
}

// Given: an artifact id of the wrong shape that a damaged row could carry
// Then: no request leaves the card
#[test]
fn a_malformed_reference_is_never_requested() {
    let item: PresentedArtifactView = serde_json::from_value(serde_json::json!({
        "artifact_id": "artifact-1", "digest": "ff", "title": "x", "media_type": "image/png",
    }))
    .unwrap();
    let project = ProjectId::new("p").unwrap();
    assert!(open_request(Some(&project), &ThreadId::new("thr").unwrap(), &item).is_none());
}

// Given: a presentation changed in place
// Then: the data the view keeps its place by differs, so the messages in view stay where they are
#[test]
fn the_fingerprint_follows_what_changes_the_height_of_a_card() {
    let hashed = |row: &CommittedMessage| {
        use std::hash::{DefaultHasher, Hasher};
        let mut hasher = DefaultHasher::new();
        fingerprint(row.presentation.as_ref().unwrap(), &mut hasher);
        hasher.finish()
    };
    let one = card(
        "r",
        0,
        "thr",
        serde_json::json!([artifact(1, "A", "image/png", None)]),
    );
    let captioned = card(
        "r",
        0,
        "thr",
        serde_json::json!([artifact(1, "A", "image/png", Some("more text"))]),
    );
    let two = card(
        "r",
        0,
        "thr",
        serde_json::json!([
            artifact(1, "A", "image/png", None),
            artifact(2, "B", "image/png", None)
        ]),
    );
    assert_ne!(hashed(&one), hashed(&captioned));
    assert_ne!(hashed(&one), hashed(&two));
    assert_eq!(hashed(&one), hashed(&one.clone()));
}
