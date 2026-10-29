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

#[cfg(unix)]
#[test]
fn retargeted_image_symlink_is_not_served_from_a_stale_entry() {
    // Given: `pic.png` is a symlink to `a.png` and a message using it is cached.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for name in ["a.png", "b.png"] {
        std::fs::write(root.join(name), b"x").unwrap();
    }
    let link = root.join("pic.png");
    std::os::unix::fs::symlink(root.join("a.png"), &link).unwrap();
    let source = "![pic](pic.png)";
    let mut cache = PreparedCache::new(8, usize::MAX);
    let before = prepare_count();
    let uri =
        |name: &str| crate::panes::file_viewer::image_uri(&root.join(name)).expect("file uri");
    let markdown = |cache: &mut PreparedCache| {
        cache
            .get_or_prepare(source, Some(&root), || prepare(source, Some(&root)))
            .markdown
            .clone()
    };
    assert!(markdown(&mut cache).contains(&uri("a.png")));
    assert!(markdown(&mut cache).contains(&uri("a.png")));
    assert_eq!(prepare_count() - before, 1, "unchanged link must hit");

    // When: the same source and base now resolve to another file.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(root.join("b.png"), &link).unwrap();

    // Then: the entry is refreshed instead of reusing the old URI.
    let refreshed = markdown(&mut cache);
    assert!(refreshed.contains(&uri("b.png")), "{refreshed}");
    assert!(!refreshed.contains(&uri("a.png")));
    assert_eq!(prepare_count() - before, 2);
    assert_eq!(cache.len(), 1, "stale entry is replaced, not duplicated");
    markdown(&mut cache);
    assert_eq!(prepare_count() - before, 2, "refreshed entry hits again");
}

/// A few KiB of source whose table is padded with empty cells to every row.
fn wide_table_source(columns: usize, rows: usize) -> String {
    let mut source = String::new();
    source.push_str(&format!("|{}\n", "h|".repeat(columns)));
    source.push_str(&format!("|{}\n", "-|".repeat(columns)));
    for _ in 0..rows {
        source.push_str("|x|\n");
    }
    source
}

#[test]
fn many_empty_cells_count_against_the_byte_budget() {
    // Given: ~6 KiB of text that parses into columns × rows cells, each at
    // least one `String` header even when empty.
    let (columns, rows) = (1000, 200);
    let source = wide_table_source(columns, rows);
    assert!(source.len() < 8 * 1024);
    let cells = columns * (rows + 1);
    let floor = cells * size_of::<String>();

    // When: the budget is below what those cells hold.
    let mut small = PreparedCache::new(8, floor / 2);
    let before = prepare_count();
    cached(&mut small, &source, None);
    cached(&mut small, &source, None);

    // Then: it is served but never stored.
    assert_eq!(small.len(), 0);
    assert_eq!(prepare_count() - before, 2);

    // And: with enough budget it is stored and accounted for at least the cells.
    let mut large = PreparedCache::new(8, floor * 4);
    cached(&mut large, &source, None);
    assert_eq!(large.len(), 1);
    assert!(large.bytes() >= floor, "{} < {floor}", large.bytes());
}

#[test]
fn many_small_wide_tables_stay_within_the_default_budget() {
    // Given: many distinct messages that each hold ~190 KiB of empty cells.
    let mut cache = PreparedCache::default();
    let per_entry = 100 * 100 * size_of::<String>();
    let budget = 16 * 1024 * 1024;

    // When: far more of them than the budget allows are rendered.
    for index in 0..(budget / per_entry) * 3 {
        cached(
            &mut cache,
            &format!("{index}\n\n{}", wide_table_source(100, 99)),
            None,
        );
    }

    // Then: held bytes stay under the budget rather than growing unbounded.
    assert!(cache.bytes() <= budget, "{}", cache.bytes());
    assert!(cache.len() < (budget / per_entry) * 2);
}
