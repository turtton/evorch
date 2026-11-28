//! External layout versions, independent of whether a transcript row is drawn.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Tracker {
    generation: Arc<AtomicU64>,
    images: Mutex<HashMap<String, Option<egui::Vec2>>>,
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
