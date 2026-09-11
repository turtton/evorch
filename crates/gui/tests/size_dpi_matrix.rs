#[path = "size_dpi_matrix/cases.rs"]
mod cases;
#[path = "size_dpi_matrix/geometry.rs"]
mod geometry;
#[path = "size_dpi_matrix/states.rs"]
mod states;

use states::State;

// Given: empty workspace / When: all size-DPI layouts / Then: controls are reachable.
#[test]
fn empty_geometry_matrix() {
    cases::verify(State::Empty);
}

// Given: demo workspace / When: all size-DPI layouts / Then: regions are well placed.
#[test]
fn demo_geometry_matrix() {
    cases::verify(State::Demo);
}

// Given: failed demo thread / When: all size-DPI layouts / Then: error remains reachable.
#[test]
fn error_thread_geometry_matrix() {
    cases::verify(State::ErrorThread);
}

// Given: local profile editor / When: all size-DPI layouts / Then: modal controls fit.
#[test]
fn edit_profile_geometry_matrix() {
    cases::verify(State::EditProfile);
}

#[test]
fn min_size_matches_real_app_viewport() {
    // Given: real viewport minimum / When: matrix sizes / Then: minimum is covered.
    assert!(
        cases::SIZES
            .iter()
            .any(|size| { size.map(f32::from) == gui::window::MIN_INNER_SIZE })
    );
}
