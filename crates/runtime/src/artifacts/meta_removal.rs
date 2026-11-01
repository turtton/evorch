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
use std::path::{Path, PathBuf};

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

pub(super) fn is_meta_name(name: &str) -> bool {
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

/// Read all of `reader`, at most [`MAX_META_BYTES`]. A reader that fails part-way is a
/// failure, never a short file: whatever was read before the error is dropped.
///
/// `Err(None)` is "the file is gone" (only an explicit not-found); `Err(Some(_))` is a
/// failure to report.
pub(super) fn read_bounded(
    reader: impl std::io::Read,
    bytes_read: &mut u64,
) -> Result<Vec<u8>, Option<String>> {
    let mut bytes = Vec::new();
    match reader.take(MAX_META_BYTES + 1).read_to_end(&mut bytes) {
        Ok(read) => *bytes_read += read as u64,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(None),
        Err(error) => return Err(Some(format!("read failed: {error}"))),
    }
    if bytes.len() as u64 > MAX_META_BYTES {
        return Err(Some(format!("larger than {MAX_META_BYTES} bytes")));
    }
    Ok(bytes)
}

fn read_owner(path: &Path, bytes_read: &mut u64) -> Read {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Read::Gone,
        Err(error) => return Read::Failed(error.to_string()),
    };
    let bytes = match read_bounded(file, bytes_read) {
        Ok(bytes) => bytes,
        Err(None) => return Read::Gone,
        Err(Some(error)) => return Read::Failed(error),
    };
    match serde_json::from_slice::<MetaOwner>(&bytes) {
        Ok(owner) => Read::Owner(owner),
        Err(error) => Read::Failed(error.to_string()),
    }
}

/// The `capacity` smallest names pushed, in memory proportional to `capacity` however
/// many names are pushed. `total` counts every push.
pub(super) struct Smallest {
    heap: BinaryHeap<String>,
    capacity: usize,
    pub total: usize,
}

impl Smallest {
    pub fn new(capacity: usize) -> Self {
        Self {
            heap: BinaryHeap::new(),
            capacity,
            total: 0,
        }
    }

    pub fn push(&mut self, name: &str) {
        self.total += 1;
        if self.heap.len() < self.capacity {
            self.heap.push(name.to_owned());
        } else if self
            .heap
            .peek()
            .is_some_and(|largest| name < largest.as_str())
        {
            self.heap.pop();
            self.heap.push(name.to_owned());
        }
    }

    pub fn into_sorted(self) -> Vec<String> {
        self.heap.into_sorted_vec()
    }
}

/// The result of one pass over a directory: the `budget` smallest accepted names after
/// a cursor.
pub(super) struct Listing {
    /// In name order.
    pub names: Vec<String>,
    /// Accepted names after the cursor, kept or not.
    pub qualifying: usize,
    /// Directory entries seen.
    pub listed: usize,
}

pub(super) struct ListError {
    /// The directory itself does not exist (as opposed to any other failure).
    pub not_found: bool,
    pub message: String,
    pub listed: usize,
}

/// List `directory` once, keeping only the `budget` smallest names after `after` for
/// which `accept` holds. A listing that fails part-way is an error: it says nothing
/// about the entries not yet seen. A name that is not UTF-8 is never accepted.
pub(super) fn list_smallest_after(
    directory: &Path,
    accept: impl Fn(&str) -> bool,
    after: Option<&str>,
    budget: usize,
) -> Result<Listing, ListError> {
    let entries = std::fs::read_dir(directory).map_err(|error| ListError {
        not_found: error.kind() == std::io::ErrorKind::NotFound,
        message: error.to_string(),
        listed: 0,
    })?;
    let mut smallest = Smallest::new(budget);
    let mut listed = 0_usize;
    for entry in entries {
        let name = entry.map_err(|error| ListError {
            not_found: false,
            message: error.to_string(),
            listed,
        })?;
        listed += 1;
        let name = name.file_name();
        let Some(name) = name.to_str() else { continue };
        if accept(name) && !after.is_some_and(|after| name <= after) {
            smallest.push(name);
        }
    }
    Ok(Listing {
        qualifying: smallest.total,
        names: smallest.into_sorted(),
        listed,
    })
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
        let listing = match list_smallest_after(&directory, is_meta_name, after, budget) {
            Ok(listing) => listing,
            Err(error) if error.not_found => return outcome,
            Err(error) => {
                outcome.listed = error.listed;
                outcome.fail(directory, &error.message);
                outcome.stop = Some(MetaStop::Listing(error.message));
                return outcome;
            }
        };
        outcome.listed = listing.listed;
        let qualifying = listing.qualifying;
        for name in listing.names {
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
