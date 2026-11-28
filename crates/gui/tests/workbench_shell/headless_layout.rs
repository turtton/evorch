//! Related GUI layout contracts share a test binary to reduce repeated linking.
//! Each module keeps its own fixtures; nextest still runs each test in a separate process.

#[path = "headless_layout/composer_band_layout.rs"]
mod composer_band_layout;
#[path = "headless_layout/pane_typography.rs"]
mod pane_typography;
#[path = "headless_layout/status_line_headless.rs"]
mod status_line_headless;
#[path = "headless_layout/theme_hierarchy.rs"]
mod theme_hierarchy;
