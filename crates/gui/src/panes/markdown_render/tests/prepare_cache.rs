use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use egui_kittest::Harness;

use super::super::cache::PreparedCache;
use super::super::{PREPARE_COUNT, prepare, render_markdown};

fn prepare_count() -> usize {
    PREPARE_COUNT.with(std::cell::Cell::get)
}

fn cached(cache: &mut PreparedCache, source: &str, base: Option<&Path>) {
    cache.get_or_prepare(source, base, || prepare(source, base));
}

#[test]
fn unchanged_source_is_prepared_once_across_frames() {
    // Given: a message rendered through the real viewer.
    let source = Rc::new(RefCell::new(String::from("# Title\n\n```\ncode\n```")));
    let shown = Rc::clone(&source);
    // The harness draws the first frame while building.
    let before = prepare_count();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(400.0, 400.0))
        .build_ui(move |ui| {
            crate::theme::install(ui.ctx());
            render_markdown(ui, &shown.borrow(), "cached-message");
        });

    // When: the same text is drawn on several frames.
    harness.run_steps(4);

    // Then: it was preprocessed only for the first one.
    assert_eq!(prepare_count() - before, 1);

    // When: the text changes.
    source.borrow_mut().push_str("\n\nmore");
    harness.run_steps(4);

    // Then: it is preprocessed again, once.
    assert_eq!(prepare_count() - before, 2);
}

#[test]
fn same_source_hits_and_changed_input_misses() {
    let mut cache = PreparedCache::new(8, usize::MAX);
    let before = prepare_count();

    cached(&mut cache, "alpha", None);
    cached(&mut cache, "alpha", None);
    assert_eq!(prepare_count() - before, 1, "second lookup must hit");

    cached(&mut cache, "alpha!", None);
    assert_eq!(prepare_count() - before, 2, "changed source must miss");

    cached(&mut cache, "alpha", Some(Path::new("/a")));
    assert_eq!(prepare_count() - before, 3, "changed base must miss");
    cached(&mut cache, "alpha", Some(Path::new("/a")));
    assert_eq!(prepare_count() - before, 3);
}

#[test]
fn entry_limit_evicts_least_recently_used() {
    let mut cache = PreparedCache::new(2, usize::MAX);
    let before = prepare_count();
    cached(&mut cache, "a", None);
    cached(&mut cache, "b", None);
    cached(&mut cache, "a", None); // refresh `a`, leaving `b` oldest

    cached(&mut cache, "c", None); // evicts `b`
    assert_eq!(cache.len(), 2);
    assert_eq!(prepare_count() - before, 3);

    cached(&mut cache, "a", None);
    assert_eq!(prepare_count() - before, 3, "`a` survived");
    cached(&mut cache, "b", None);
    assert_eq!(prepare_count() - before, 4, "`b` was evicted");
}

#[test]
fn byte_limit_bounds_memory_and_skips_oversized_entries() {
    let small = "x".repeat(100);
    let mut probe = PreparedCache::new(8, usize::MAX);
    cached(&mut probe, &small, None);
    let one = probe.bytes();
    let mut cache = PreparedCache::new(8, one * 2 + one / 2);

    // When: more entries than the byte budget holds are added.
    for index in 0..5 {
        cached(&mut cache, &format!("{index}{}", &small[1..]), None);
    }

    // Then: only what fits stays.
    assert_eq!(cache.len(), 2);
    assert!(cache.bytes() <= one * 2 + one / 2);

    // And: a source larger than the whole budget is served but never kept.
    let huge = "y".repeat(one * 4);
    let before = prepare_count();
    cached(&mut cache, &huge, None);
    cached(&mut cache, &huge, None);
    assert_eq!(prepare_count() - before, 2);
    assert_eq!(cache.len(), 2);
}
