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

// Given: theme picker state / When: all size-DPI layouts / Then: picker controls fit.
#[test]
fn theme_settings_geometry_matrix() {
    cases::verify(State::ThemeSettings);
}

#[test]
fn tokyo_night_demo_geometry_matrix() {
    const CHILD: &str = "EVORCH_TOKYO_NIGHT_MATRIX_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tokyo_night_demo_geometry_matrix"])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    cases::verify_with_theme(State::Demo, gui::theme::style::ThemePreset::TokyoNight);
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
