//! Sidebar JSON (de)serialisation and the plain file save/load helpers.
use std::fs;
use std::path::Path;

use crate::{SidebarError, SidebarState};

pub fn sidebar_to_json(sidebar: &SidebarState) -> Result<String, SidebarError> {
    sidebar.validate()?;
    serde_json::to_string_pretty(sidebar)
        .map_err(|error| SidebarError::Serialization(error.to_string()))
}

pub fn sidebar_from_json(json: &str) -> Result<SidebarState, SidebarError> {
    sidebar_from_json_report(json).map(|(sidebar, _)| sidebar)
}

/// [`sidebar_from_json`] that also returns what the one-time conversion of an older
/// file did, when it ran.
pub fn sidebar_from_json_report(
    json: &str,
) -> Result<(SidebarState, Option<crate::family::MigrationReport>), SidebarError> {
    let mut sidebar: SidebarState = serde_json::from_str(json)
        .map_err(|error| SidebarError::Serialization(error.to_string()))?;
    crate::normalize_versions(&mut sidebar.threads);
    let report = sidebar.settle_loaded();
    sidebar.validate()?;
    Ok((sidebar, report))
}

pub fn save_sidebar(sidebar: &SidebarState, path: &Path) -> Result<(), SidebarError> {
    let json = sidebar_to_json(sidebar)?;
    fs::write(path, json).map_err(|error| SidebarError::Io(error.to_string()))
}

pub fn load_sidebar(path: &Path) -> Result<SidebarState, SidebarError> {
    load_sidebar_report(path).map(|(sidebar, _)| sidebar)
}

/// [`load_sidebar`] that also returns what the one-time conversion of an older file did.
pub fn load_sidebar_report(
    path: &Path,
) -> Result<(SidebarState, Option<crate::family::MigrationReport>), SidebarError> {
    let json = fs::read_to_string(path).map_err(|error| SidebarError::Io(error.to_string()))?;
    sidebar_from_json_report(&json)
}
