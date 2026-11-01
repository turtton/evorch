//! Removal of artifact metadata for deleted threads (archived-thread retention).
//!
//! Only `meta/<artifact_id>.json` is removed here. Blobs are content addressed
//! and may be shared by other threads; collecting them belongs to the blob
//! collector, which first needs to know every remaining reference.
//!
//! Every file operation has four distinct outcomes and they are never merged:
//! the work finished (`stop == None`), the budget ran out (`Budget`, continue from
//! `last_name`), the directory could not be listed (`Listing`), or one file could
//! not be read or unlinked (an entry in `failed`). Only a file that is explicitly
//! *not found* is treated as already gone.

use std::collections::{BinaryHeap, HashSet};
use std::io::Read as _;
use std::path::PathBuf;

use serde::Deserialize;

use super::ArtifactStore;

/// A meta file larger than this is not parsed: a record is a few kilobytes, so a
/// bigger one is damaged or foreign, and it is reported instead of read into memory.
pub const MAX_META_BYTES: u64 = 1024 * 1024;
/// Failure details kept per call; further failures are only counted.
pub const MAX_FAILURES_KEPT: usize = 16;

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

/// Why a pass over the directory ended before examining all of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaStop {
    /// `budget` files were examined and more remain; continue after `last_name`.
    Budget,
    /// The directory could not be opened or listed; nothing is known about what is in
    /// it. Retrying from the start is the only way on.
    Listing(String),
}

/// What one [`ArtifactStore::remove_meta_for`] call did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MetaRemoval {
    /// Artifact IDs whose meta file was deleted.
    pub removed: Vec<String>,
    /// The first [`MAX_FAILURES_KEPT`] meta files that could not be read, parsed or
    /// deleted. They are never treated as "not ours": the family's files stay
    /// unfinished until they succeed.
    pub failed: Vec<(PathBuf, String)>,
    /// All failures, kept or not.
    pub failed_total: usize,
    /// Directory entries seen. Listing costs one pass over all of them per call.
    pub listed: usize,
    /// Meta files examined (opened) by this call.
    pub scanned: usize,
    /// Bytes read from them.
    pub bytes_read: u64,
    /// `None`: the whole directory was examined.
    pub stop: Option<MetaStop>,
    /// The last file name examined; pass it as `after` to continue.
    pub last_name: Option<String>,
}

impl MetaRemoval {
    /// The whole directory was examined (failures may still be listed).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.stop.is_none()
    }

    fn fail(&mut self, path: PathBuf, error: impl ToString) {
        self.failed_total += 1;
        if self.failed.len() < MAX_FAILURES_KEPT {
            self.failed.push((path, error.to_string()));
        }
    }
}

fn is_meta_name(name: &str) -> bool {
    name.strip_suffix(".json")
        .and_then(|id| id.strip_prefix("artifact-"))
        .is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

enum Read {
    Owner(MetaOwner),
    /// Removed since the listing: nothing to do.
    Gone,
    Failed(String),
}

fn read_owner(path: &std::path::Path, bytes_read: &mut u64) -> Read {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Read::Gone,
        Err(error) => return Read::Failed(error.to_string()),
    };
    let mut bytes = Vec::new();
    match file.take(MAX_META_BYTES + 1).read_to_end(&mut bytes) {
        Ok(read) => *bytes_read += read as u64,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Read::Gone,
        Err(error) => return Read::Failed(error.to_string()),
    }
    if bytes.len() as u64 > MAX_META_BYTES {
        return Read::Failed(format!("larger than {MAX_META_BYTES} bytes"));
    }
    match serde_json::from_slice::<MetaOwner>(&bytes) {
        Ok(owner) => Read::Owner(owner),
        Err(error) => Read::Failed(error.to_string()),
    }
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
    /// `last_name` of a call that stopped on its budget).
    ///
    /// The directory is listed once per call and only the `budget` smallest names
    /// after `after` are kept, so memory is bounded by the budget however many files
    /// exist; time is one pass over the entries (`listed`). At most
    /// [`MAX_META_BYTES`] of any one file are read.
    pub fn remove_meta_after(
        &self,
        threads: &HashSet<String>,
        runs: &HashSet<String>,
        after: Option<&str>,
        budget: usize,
    ) -> MetaRemoval {
        let directory = self.root.join("meta");
        let mut outcome = MetaRemoval::default();
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return outcome,
            Err(error) => {
                outcome.fail(directory, &error);
                outcome.stop = Some(MetaStop::Listing(error.to_string()));
                return outcome;
            }
        };
        // The `budget` smallest qualifying names; `qualifying` counts all of them.
        let mut smallest: BinaryHeap<String> = BinaryHeap::new();
        let mut qualifying = 0_usize;
        for entry in entries {
            let name = match entry {
                Ok(entry) => entry.file_name(),
                Err(error) => {
                    // A listing that failed midway says nothing about the rest.
                    outcome.fail(directory.clone(), &error);
                    outcome.stop = Some(MetaStop::Listing(error.to_string()));
                    return outcome;
                }
            };
            outcome.listed += 1;
            let Some(name) = name.to_str() else { continue };
            if !is_meta_name(name) || after.is_some_and(|after| name <= after) {
                continue;
            }
            qualifying += 1;
            if smallest.len() < budget {
                smallest.push(name.to_owned());
            } else if smallest
                .peek()
                .is_some_and(|largest| name < largest.as_str())
            {
                smallest.pop();
                smallest.push(name.to_owned());
            }
        }
        for name in smallest.into_sorted_vec() {
            outcome.scanned += 1;
            let path = directory.join(&name);
            outcome.last_name = Some(name.clone());
            let owner = match read_owner(&path, &mut outcome.bytes_read) {
                Read::Owner(owner) => owner,
                Read::Gone => continue,
                Read::Failed(error) => {
                    outcome.fail(path, error);
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
                Err(error) => outcome.fail(path, error),
            }
        }
        if qualifying > outcome.scanned {
            outcome.stop = Some(MetaStop::Budget);
        }
        outcome
    }
}
