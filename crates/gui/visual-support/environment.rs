//! The process a capture runs in. The coordinator starts it from an empty environment
//! with an allowlist; the tool checks that again, so a token or a user setting that
//! reached it by another route is a failure and not a picture of the runner.

use std::path::Path;

/// Exact names a capture process may have.
const ALLOWED: &[&str] = &[
    "HOME",
    "TMPDIR",
    "PATH",
    "LD_LIBRARY_PATH",
    "FONTCONFIG_FILE",
    "FONTCONFIG_PATH",
    "LANG",
    "LC_ALL",
    "TZ",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "WGPU_BACKEND",
    "WGPU_ADAPTER_NAME",
    "VK_ICD_FILENAMES",
    "VK_DRIVER_FILES",
    "LIBGL_ALWAYS_SOFTWARE",
    "RUST_BACKTRACE",
    "RUST_LOG",
    // a nextest run of the tests that start this tool
    "CARGO_MANIFEST_DIR",
];

/// The prefixes of the names the visual tooling itself sets.
const ALLOWED_PREFIXES: &[&str] = &["EVORCH_VISUAL_", "MESA_", "LP_", "RADV_", "NEXTEST"];

pub fn is_allowed(name: &str) -> bool {
    ALLOWED.contains(&name)
        || ALLOWED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// The names (never the values) that are not on the allowlist, sorted.
pub fn violations<I, K>(vars: I) -> Vec<String>
where
    I: IntoIterator<Item = K>,
    K: AsRef<str>,
{
    let mut names: Vec<String> = vars
        .into_iter()
        .map(|name| name.as_ref().to_owned())
        .filter(|name| !is_allowed(name))
        .collect();
    names.sort();
    names
}

pub fn check_process_environment() -> Result<(), String> {
    let names: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    let bad = violations(names);
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the capture process has variables outside the allowlist: {}",
            bad.join(", ")
        ))
    }
}

/// Text that must never appear in a drawn screen: where this process runs and keeps files.
/// Values shorter than 4 characters (and `/`) would match too much to say anything.
pub fn forbidden_texts(extra: &[&Path]) -> Vec<String> {
    let mut texts: Vec<String> = Vec::new();
    for name in [
        "HOME",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
        "EVORCH_VISUAL_SENTINEL",
    ] {
        if let Some(value) = std::env::var_os(name) {
            texts.push(value.to_string_lossy().into_owned());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        texts.push(cwd.to_string_lossy().into_owned());
    }
    if let Ok(exe) = std::env::current_exe() {
        texts.push(exe.to_string_lossy().into_owned());
    }
    texts.extend(extra.iter().map(|path| path.to_string_lossy().into_owned()));
    texts.retain(|text| text.len() >= 4);
    texts.sort();
    texts.dedup();
    texts
}

/// The first drawn text that contains a forbidden one.
pub fn find_leak<'a>(drawn: &'a [String], forbidden: &[String]) -> Option<(&'a str, String)> {
    drawn.iter().find_map(|text| {
        forbidden
            .iter()
            .find(|needle| text.contains(needle.as_str()))
            .map(|needle| (text.as_str(), needle.clone()))
    })
}
