//! Read-only view of the GUI's `sidebar.json`, reduced to what the cleanup needs: which
//! thread binds which run. The archive decision builds its run set from the sidebar's
//! `run_ids` and `root_run_id` as well as from `thread_runs`, so a deleted run that a live
//! sidebar thread still names can still hold a family; the cleanup must see that binding.
//!
//! The file is only read: no lock file, temporary file or rewrite. A file that is missing or
//! has an unknown shape is an error, never an empty set.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

/// More than any real sidebar; a larger file is refused instead of read into memory.
const MAX_SIDEBAR_BYTES: u64 = 64 * 1024 * 1024;

/// Thread ID to the runs the sidebar binds to it (`run_ids` plus `root_run_id`).
#[derive(Debug, Clone, Default)]
pub struct SidebarBindings {
    threads: BTreeMap<String, Vec<String>>,
}

impl SidebarBindings {
    /// Test and embedding constructor.
    pub fn from_threads(threads: impl IntoIterator<Item = (String, Vec<String>)>) -> Self {
        Self {
            threads: threads.into_iter().collect(),
        }
    }

    /// Reads the file. Every thread needs a string `id`; `run_ids` must be an array of
    /// strings when present and `root_run_id` a string or null.
    ///
    /// # Errors
    /// A readable description when the file is missing, too large, not JSON, or not shaped
    /// like a sidebar.
    pub fn read(path: &Path) -> Result<Self, String> {
        let meta = std::fs::symlink_metadata(path)
            .map_err(|error| format!("sidebar {}: {error}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("sidebar {} is not a regular file", path.display()));
        }
        if meta.len() > MAX_SIDEBAR_BYTES {
            return Err(format!("sidebar {} is too large", path.display()));
        }
        let bytes =
            std::fs::read(path).map_err(|error| format!("sidebar {}: {error}", path.display()))?;
        Self::parse(&bytes)
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let document: Value = serde_json::from_slice(bytes)
            .map_err(|error| format!("sidebar is not valid JSON: {error}"))?;
        let threads = document
            .get("threads")
            .and_then(Value::as_array)
            .ok_or("sidebar has no `threads` array")?;
        let mut out = BTreeMap::new();
        for thread in threads {
            let id = thread
                .get("id")
                .and_then(Value::as_str)
                .ok_or("a sidebar thread has no string `id`")?;
            let mut runs = Vec::new();
            match thread.get("run_ids") {
                None | Some(Value::Null) => {}
                Some(Value::Array(items)) => {
                    for item in items {
                        runs.push(
                            item.as_str()
                                .ok_or("a sidebar `run_ids` entry is not a string")?
                                .to_owned(),
                        );
                    }
                }
                Some(_) => return Err("a sidebar `run_ids` is not an array".into()),
            }
            match thread.get("root_run_id") {
                None | Some(Value::Null) => {}
                Some(Value::String(root)) => runs.push(root.clone()),
                Some(_) => return Err("a sidebar `root_run_id` is not a string".into()),
            }
            runs.sort();
            runs.dedup();
            if out.insert(id.to_owned(), runs).is_some() {
                return Err(format!("sidebar names thread {id} twice"));
            }
        }
        Ok(Self { threads: out })
    }

    /// Sidebar threads that bind `run` and are not named by `deleted`.
    pub fn live_threads_binding(&self, run: &str, deleted: &dyn Fn(&str) -> bool) -> Vec<String> {
        self.threads
            .iter()
            .filter(|(id, runs)| !deleted(id) && runs.iter().any(|r| r == run))
            .map(|(id, _)| id.clone())
            .collect()
    }
}
