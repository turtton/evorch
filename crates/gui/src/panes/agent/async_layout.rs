//! External layout versions, independent of whether a transcript row is drawn.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::model::html_snapshots::HtmlSnapshots;

#[derive(Default)]
struct Tracker {
    generation: Arc<AtomicU64>,
    images: Mutex<HashMap<String, Option<egui::Vec2>>>,
    snapshot: Mutex<Option<(Arc<HtmlSnapshots>, u64)>>,
}

fn tracker(ctx: &egui::Context) -> Arc<Tracker> {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Arc<Tracker>>(egui::Id::new("transcript-async-layout"))
            .clone()
    })
}

/// A producer may retain this counter without retaining the egui context.
pub(crate) fn generation(ctx: &egui::Context) -> Arc<AtomicU64> {
    Arc::clone(&tracker(ctx).generation)
}

pub(crate) fn invalidate(ctx: &egui::Context) {
    generation(ctx).fetch_add(1, Ordering::Release);
}

/// Register the URI used by egui, including rewritten local markdown images.
/// Only Pending URIs need polling; settled image data is immutable.
pub(crate) fn observe_image(ctx: &egui::Context, uri: &str) {
    if !super::mark_async_layout(ctx) {
        return;
    }
    observe(ctx, &tracker(ctx), uri);
}

fn observe(ctx: &egui::Context, tracker: &Tracker, uri: &str) {
    let image = ctx.try_load_image(uri, egui::load::SizeHint::default());
    let mut images = tracker.images.lock().unwrap_or_else(|e| e.into_inner());
    let changed = match image {
        Ok(egui::load::ImagePoll::Pending { size }) => images
            .insert(uri.to_owned(), size)
            .is_some_and(|previous| previous != size),
        _ => images.remove(uri).is_some(),
    };
    if changed {
        tracker.generation.fetch_add(1, Ordering::Release);
    }
}

/// Poll external state before consulting row heights. Loader completion already
/// wakes egui; polling must not request a continuous repaint for Pending data.
pub(super) fn poll(ctx: &egui::Context) -> u64 {
    let tracker = tracker(ctx);
    let current = HtmlSnapshots::installed(ctx).map(|snapshots| {
        let version = snapshots.generation();
        (snapshots, version)
    });
    let changed = {
        let mut previous = tracker.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        let same = match (&*previous, &current) {
            (None, None) => true,
            (Some((a, av)), Some((b, bv))) => Arc::ptr_eq(a, b) && av == bv,
            _ => false,
        };
        *previous = current;
        !same
    };
    if changed {
        tracker.generation.fetch_add(1, Ordering::Release);
    }
    let images: Vec<_> = tracker
        .images
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    for uri in images {
        observe(ctx, &tracker, &uri);
    }
    tracker.generation.load(Ordering::Acquire)
}
