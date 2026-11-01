//! Removal of artifact metadata for deleted threads (archived-thread retention).
//!
//! Only `meta/<artifact_id>.json` is removed here. Blobs are content addressed
//! and may be shared by other threads; collecting them belongs to the blob
//! collector, which first needs to know every remaining reference.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::Deserialize;

use super::ArtifactStore;

/// The identity fields of a meta file; unknown fields are tolerated so a record
/// written by another version is still matched.
#[derive(Deserialize)]
struct MetaOwner {
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    root_run_id: Option<String>,
}

/// What one [`ArtifactStore::remove_meta_for`] call did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MetaRemoval {
    /// Artifact IDs whose meta file was deleted.
    pub removed: Vec<String>,
    /// Meta files that could not be read, parsed or deleted. They are never
    /// treated as "not ours": the family's files stay unfinished until they succeed.
    pub failed: Vec<(PathBuf, String)>,
    /// Meta files examined by this call.
    pub scanned: usize,
    /// The whole directory was examined; `false` when `budget` ran out first.
    pub complete: bool,
    /// The last file name examined; pass it as `after` to continue.
    pub last_name: Option<String>,
}

fn is_meta_name(name: &str) -> bool {
    name.strip_suffix(".json")
        .and_then(|id| id.strip_prefix("artifact-"))
        .is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

impl ArtifactStore {
    /// Delete the meta files whose `thread_id` is in `threads` or whose `run_id` /
    /// `root_run_id` is in `runs`, examining at most `budget` files in name order.
    pub fn remove_meta_for(
        &self,
        threads: &HashSet<String>,
        runs: &HashSet<String>,
        budget: usize,
    ) -> MetaRemoval {
        self.remove_meta_after(threads, runs, None, budget)
    }

    /// [`Self::remove_meta_for`] starting after the file name `after` (the
    /// `last_name` of an incomplete call). Listing the directory costs one pass over
    /// its names every call; only `budget` files are opened.
    pub fn remove_meta_after(
        &self,
        threads: &HashSet<String>,
        runs: &HashSet<String>,
        after: Option<&str>,
        budget: usize,
    ) -> MetaRemoval {
        let directory = self.root.join("meta");
        let mut outcome = MetaRemoval::default();
        let mut names = match std::fs::read_dir(&directory) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| {
                    is_meta_name(name) && after.is_none_or(|after| name.as_str() > after)
                })
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                outcome.complete = true;
                return outcome;
            }
            Err(error) => {
                outcome.failed.push((directory, error.to_string()));
                return outcome;
            }
        };
        names.sort_unstable();
        let total = names.len();
        for name in names.into_iter().take(budget) {
            outcome.scanned += 1;
            let path = directory.join(&name);
            outcome.last_name = Some(name.clone());
            let owner = std::fs::read(&path)
                .map_err(|error| error.to_string())
                .and_then(|bytes| {
                    serde_json::from_slice::<MetaOwner>(&bytes).map_err(|error| error.to_string())
                });
            let owner = match owner {
                Ok(owner) => owner,
                // A file removed since the listing is not ours any more.
                Err(_) if !path.exists() => continue,
                Err(error) => {
                    outcome.failed.push((path, error));
                    continue;
                }
            };
            let ours = owner
                .thread_id
                .as_ref()
                .is_some_and(|id| threads.contains(id))
                || owner.run_id.as_ref().is_some_and(|id| runs.contains(id))
                || owner
                    .root_run_id
                    .as_ref()
                    .is_some_and(|id| runs.contains(id));
            if !ours {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => outcome
                    .removed
                    .push(name.trim_end_matches(".json").to_owned()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => outcome.failed.push((path, error.to_string())),
            }
        }
        outcome.complete = outcome.scanned >= total;
        outcome
    }
}
