#[test]
fn native_options_set_layout_sizes_when_created() {
    // Given: a title for an isolated smoke-test window.
    let title = "evorch-smoke-1";
    // When: native options are built without opening a window.
    let options = gui::window::native_options(title);
    // Then: the dense workbench has a minimum and a comfortable initial size.
    assert_eq!(
        options.viewport.min_inner_size,
        Some(egui::vec2(960.0, 600.0))
    );
    assert_eq!(options.viewport.inner_size, Some(egui::vec2(1280.0, 720.0)));
}

#[test]
fn native_options_preserve_title_when_provided() {
    // Given: a title different from the default application name.
    let title = "evorch-smoke-1";
    // When: native options are built.
    let options = gui::window::native_options(title);
    // Then: window automation can identify this instance.
    assert_eq!(options.viewport.title, Some("evorch-smoke-1".to_string()));
}

#[test]
fn minimum_fits_default_when_sizes_are_compared() {
    // Given: the public window-size policy.
    let minimum = gui::window::MIN_INNER_SIZE;
    let default = gui::window::DEFAULT_INNER_SIZE;
    // When: each minimum dimension is compared with its default.
    let fits = minimum
        .into_iter()
        .zip(default)
        .all(|(min, size)| min <= size);
    // Then: the initial window meets the layout floor.
    assert!(fits);
}
