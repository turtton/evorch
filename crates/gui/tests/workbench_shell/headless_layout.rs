//! Related GUI layout contracts share a test binary to reduce repeated linking.
//! Each module keeps its own fixtures; nextest still runs each test in a separate process.

#[path = "headless_layout/theme_hierarchy.rs"]
mod theme_hierarchy;
