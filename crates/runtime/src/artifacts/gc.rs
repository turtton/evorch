//! Collection of artifact blobs no meta file refers to (mark and sweep).
//!
//! `blobs/<sha256>.<ext>` is content addressed: several meta files, of several threads,
//! may name the same blob. Deleting a thread removes its meta files
//! ([`super::ArtifactStore::remove_meta_for`]) and leaves the blobs. This module runs in
//! the exclusive startup phase (no other evorch process exists, so no capture is in
//! flight) and removes the blobs that nothing refers to any more.
//!
//! * **Mark.** The `artifact_refs` table mirrors the meta files. A *pass* walks the meta
//!   file names in order and gives each one without a row a row, by reading it. A pass
//!   that fits the budget ends in the start that began it. A bigger one continues over
//!   several starts from a stored position, together with a fingerprint of the names
//!   up to it: every start lists the whole directory again, and if the names behind the
//!   position changed (a meta file written by a host that records no references may
//!   have been added there), the pass starts over. A new pass begins at every start
//!   after a complete one, so such a file is found before anything is swept.
//! * **Orphan references.** A row whose meta file is gone is deleted (the runtime
//!   removes rows with the files; this catches a stop between the two).
//! * **Sweep.** A blob whose SHA-256 no row names is unlinked, but **only** when the
//!   references are *complete*: this start's listing was finished, every meta file
//!   has a row, and no meta file is recorded as unreadable.
//!
//! Errors are never evidence. A meta file that cannot be listed, opened, read to the end,
//! parsed, or does not name a valid blob, is recorded in `gc_unknown_meta`; we cannot
//! tell which blob it protects, so nothing is swept until it reads again (or is
//! deleted). A database error stops the stage the same way. Only an explicit "not found"
//! means a file is gone. Everything is bounded by [`GcBudget`] counts, and what does not
//! fit continues at the next start.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use storage::Database;
use storage::StorageError;
use storage::artifact_refs::{ArtifactRef, GcState, MetaState};

use super::meta_removal::{
    MAX_FAILURES_KEPT, Smallest, is_meta_name, list_smallest_after, read_bounded,
};

/// Unreadable meta files named in a report; the rest are counted.
const UNKNOWN_LISTED: usize = 8;

/// What one start may do, in quantities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcBudget {
    /// Meta files opened.
    pub meta_files: usize,
    /// Bytes read from them.
    pub meta_bytes: u64,
    /// Meta file names looked up in the reference table, and held in memory, per start.
    /// A directory with more meta files is marked over several starts.
    pub meta_lookups: usize,
    /// Unreadable-meta records read again.
    pub unknown_retries: usize,
    /// Reference rows checked for a missing meta file.
    pub ref_checks: usize,
    /// Blob directory entries examined.
    pub blob_entries: usize,
    /// Blob files unlinked (attempted).
    pub unlinks: usize,
}

impl Default for GcBudget {
    fn default() -> Self {
        Self {
            meta_files: 20_000,
            meta_bytes: 32 * 1024 * 1024,
            meta_lookups: 100_000,
            unknown_retries: 1_000,
            ref_checks: 20_000,
            blob_entries: 50_000,
            unlinks: 5_000,
        }
    }
}

/// Why a part of the work did not finish in this start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcStop {
    /// `meta/` does not exist although the artifacts directory does.
    MetaDirMissing,
    /// `meta/` could not be listed (completely).
    MetaListing(String),
    /// More meta files after the pass position than `meta_lookups`; the pass continues at
    /// the next start.
    MetaLookupBudget,
    /// More meta files without a reference than `meta_files`.
    MetaOpenBudget,
    /// `meta_bytes` were read.
    MetaBytesBudget,
    /// More reference rows than `ref_checks`; the next start continues.
    RefCheckBudget,
    /// `blobs/` could not be listed (completely).
    BlobListing(String),
    /// More blob entries than `blob_entries`; the next start continues.
    BlobEntryBudget,
    /// `unlinks` were attempted; the next start continues.
    UnlinkBudget,
    /// A database error ended the stage. Nothing was concluded from it.
    Database(String),
}

/// What one collection did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Unreadable-meta records read again, and how many of them are settled now.
    pub unknown_retried: usize,
    pub unknown_resolved: usize,
    /// Reference rows created from meta files.
    pub refs_added: usize,
    /// Reference rows deleted because their meta file is gone.
    pub refs_removed_orphan: usize,
    /// Meta files seen in the directory, and opened.
    pub meta_listed: usize,
    pub meta_scanned: usize,
    /// A marking pass left by an earlier start was dropped because meta files were added
    /// or removed behind its position; this start began a new one.
    pub pass_restarted: bool,
    /// Every meta file has a reference and none is unreadable: references prove which
    /// blobs are in use. Only then is anything swept.
    pub refs_complete: bool,
    /// Meta files still recorded as unreadable, and the first few with their errors.
    pub unknown_remaining: u64,
    pub unknown_sample: Vec<(String, String)>,
    /// Blob directory entries examined and files unlinked, with the bytes they held.
    pub blobs_listed: usize,
    pub blobs_unlinked: usize,
    pub bytes_freed: u64,
    /// The first few file failures (the next start retries them) and how many there were.
    pub failed: Vec<(PathBuf, String)>,
    pub failed_total: usize,
    pub stops: Vec<GcStop>,
}

/// Run the collection on `artifacts_dir` (holding `meta/` and `blobs/`).
///
/// The caller holds both exclusive leases: `database` is written directly and no
/// capture can run. A database error is reported in [`GcReport::stops`], never raised.
#[must_use]
pub fn collect(
    database: &Database,
    artifacts_dir: &Path,
    budget: &GcBudget,
    now_ns: i64,
) -> GcReport {
    let mut run = Collection {
        database,
        meta_dir: artifacts_dir.join("meta"),
        blob_dir: artifacts_dir.join("blobs"),
        budget,
        now_ns,
        bytes_read: 0,
        report: GcReport::default(),
    };
    if let Err(error) = run.execute() {
        run.stop(GcStop::Database(error.to_string()));
    }
    run.report
}

/// The fields of a meta file the collector needs; unknown fields are tolerated so a
/// record written by another version still counts.
#[derive(Deserialize)]
struct MetaRef {
    artifact_id: String,
    sha256: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    run_id: Option<String>,
}

enum ReadMeta {
    Ref(ArtifactRef),
    /// The file does not exist (an explicit not-found, confirmed on the entry itself).
    Absent,
    Failed(String),
}

/// A blob SHA-256 as the store writes it.
fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `<64 lower-case hex>.<alphanumeric extension>`: the only names the sweep touches.
fn is_blob_name(name: &str) -> bool {
    name.split_once('.').is_some_and(|(stem, extension)| {
        is_sha256(stem)
            && !extension.is_empty()
            && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

/// Read a meta file's content from `reader` and take its reference from it.
fn interpret_meta(
    reader: impl std::io::Read,
    expected_id: &str,
    now_ns: i64,
    bytes_read: &mut u64,
) -> ReadMeta {
    let bytes = match read_bounded(reader, bytes_read) {
        Ok(bytes) => bytes,
        Err(None) => return ReadMeta::Failed("disappeared while being read".into()),
        Err(Some(error)) => return ReadMeta::Failed(error),
    };
    let meta: MetaRef = match serde_json::from_slice(&bytes) {
        Ok(meta) => meta,
        Err(error) => return ReadMeta::Failed(format!("not a valid record: {error}")),
    };
    if meta.artifact_id != expected_id {
        return ReadMeta::Failed(format!(
            "identity mismatch: the file names {}",
            meta.artifact_id
        ));
    }
    if !is_sha256(&meta.sha256) {
        return ReadMeta::Failed("sha256 is not a lower-case hex SHA-256".into());
    }
    ReadMeta::Ref(ArtifactRef {
        artifact_id: meta.artifact_id,
        sha256: meta.sha256,
        thread_id: meta.thread_id,
        run_id: meta.run_id,
        recorded_at_ns: now_ns,
    })
}

/// An order-independent fingerprint of a set of names: the count, and the sum modulo
/// 2^256 of the SHA-256 digest of each name.
///
/// Adding, removing or exchanging any name changes the sum unless the digests happen to
/// cancel, which for names nobody crafted has probability about 2^-256 per change. It is
/// a change detector for directories that only this program and its own captures write;
/// it is not a proof of identity against someone who can write arbitrary file names into
/// `meta/` (they can already write any meta file, and so any reference they like).
/// A name occurs once in a directory; a sum, unlike an XOR, would also tell a repeated one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Fingerprint {
    count: u64,
    /// Little-endian 64-bit limbs of the sum.
    sum: [u64; 4],
}

impl Fingerprint {
    /// The SHA-256 of `name` under a fixed label, as four little-endian limbs.
    fn digest(name: &str) -> [u64; 4] {
        let mut hasher = Sha256::new();
        hasher.update(b"evorch artifact gc meta name\0");
        hasher.update(name.as_bytes());
        let bytes = hasher.finalize();
        let (chunks, _) = bytes.as_slice().as_chunks::<8>();
        let mut limbs = [0_u64; 4];
        for (limb, chunk) in limbs.iter_mut().zip(chunks) {
            *limb = u64::from_le_bytes(*chunk);
        }
        limbs
    }

    fn add(&mut self, name: &str) {
        self.count += 1;
        let mut carry = false;
        for (limb, addend) in self.sum.iter_mut().zip(Self::digest(name)) {
            let (partial, first) = limb.overflowing_add(addend);
            let (total, second) = partial.overflowing_add(u64::from(carry));
            *limb = total;
            carry = first || second;
        }
    }

    fn hex(&self) -> String {
        self.sum
            .iter()
            .rev()
            .map(|limb| format!("{limb:016x}"))
            .collect()
    }

    /// Whether this is the fingerprint `state` recorded.
    fn matches(&self, state: &GcState) -> bool {
        self.count == state.meta_count && state.meta_hash == self.hex()
    }

    fn store(&self, state: &mut GcState) {
        state.meta_count = self.count;
        state.meta_hash = self.hex();
    }
}

/// One listing of the metadata directory.
struct Scan {
    /// The names up to the cursor.
    prefix: Fingerprint,
    /// The smallest names after the cursor, up to the lookup budget.
    chunk: Smallest,
    /// How many names there are after the cursor.
    qualifying: usize,
    /// Meta file names in the directory.
    listed: usize,
}

struct Mark {
    /// The directory could be listed to the end of what was looked at.
    readable: bool,
    /// Every meta file seen has a reference now.
    all_marked: bool,
}

struct Collection<'a> {
    database: &'a Database,
    meta_dir: PathBuf,
    blob_dir: PathBuf,
    budget: &'a GcBudget,
    now_ns: i64,
    bytes_read: u64,
    report: GcReport,
}

impl Collection<'_> {
    fn stop(&mut self, stop: GcStop) {
        if !self.report.stops.contains(&stop) {
            self.report.stops.push(stop);
        }
    }

    fn fail(&mut self, path: PathBuf, error: impl ToString) {
        self.report.failed_total += 1;
        if self.report.failed.len() < MAX_FAILURES_KEPT {
            self.report.failed.push((path, error.to_string()));
        }
    }

    fn execute(&mut self) -> Result<(), StorageError> {
        let mut state = self.database.gc_state()?;
        self.retry_unknown()?;
        let mark = self.mark(&mut state)?;
        let unknown = self.database.gc_unknown_meta_count()?;
        self.report.unknown_remaining = unknown;
        self.report.unknown_sample = self
            .database
            .gc_unknown_meta_list(UNKNOWN_LISTED)?
            .into_iter()
            .map(|row| (row.name, row.last_error))
            .collect();
        self.report.refs_complete = mark.all_marked && unknown == 0;
        state.refs_complete = self.report.refs_complete;
        self.database.set_gc_state(&state)?;
        if mark.readable {
            self.check_orphans(&mut state)?;
        }
        if self.report.refs_complete {
            self.sweep(&mut state)?;
        }
        Ok(())
    }

    /// Read the meta file `name` (`artifact-<hex>.json`) of the metadata directory.
    fn read_meta(&mut self, name: &str) -> ReadMeta {
        let path = self.meta_dir.join(name);
        match std::fs::metadata(&path) {
            Ok(metadata) if !metadata.is_file() => {
                return ReadMeta::Failed("not a regular file".into());
            }
            Ok(_) => {}
            // Gone only if the entry itself is gone: a dangling link is still a file
            // that names something we cannot see.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return match std::fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => ReadMeta::Absent,
                    Ok(_) => ReadMeta::Failed("dangling symbolic link".into()),
                    Err(error) => ReadMeta::Failed(error.to_string()),
                };
            }
            Err(error) => return ReadMeta::Failed(error.to_string()),
        }
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => return ReadMeta::Failed(error.to_string()),
        };
        let expected = name.strip_suffix(".json").unwrap_or(name);
        interpret_meta(file, expected, self.now_ns, &mut self.bytes_read)
    }

    /// Read again what an earlier start could not read.
    fn retry_unknown(&mut self) -> Result<(), StorageError> {
        let rows = self
            .database
            .gc_unknown_meta_list(self.budget.unknown_retries)?;
        let mut found = Vec::new();
        for row in rows {
            self.report.unknown_retried += 1;
            if !is_meta_name(&row.name) {
                // Not a name a meta file can have: it protects nothing.
                self.database.gc_unknown_meta_delete(&row.name)?;
                self.report.unknown_resolved += 1;
                continue;
            }
            // Half of the byte budget at most, so that re-reading files that keep failing
            // never leaves the marking pass without any.
            if self.bytes_read >= self.budget.meta_bytes / 2 {
                self.stop(GcStop::MetaBytesBudget);
                break;
            }
            match self.read_meta(&row.name) {
                ReadMeta::Ref(reference) => found.push((row.name, reference)),
                ReadMeta::Absent => {
                    self.database.gc_unknown_meta_delete(&row.name)?;
                    self.report.unknown_resolved += 1;
                }
                ReadMeta::Failed(error) => {
                    self.database
                        .gc_unknown_meta_upsert(&row.name, &error, self.now_ns)?;
                    self.fail(self.meta_dir.join(&row.name), error);
                }
            }
        }
        if !found.is_empty() {
            // The reference first: a stop in between leaves a record that blocks nothing.
            let references = found
                .iter()
                .map(|(_, reference)| reference.clone())
                .collect::<Vec<_>>();
            self.database.gc_insert_refs(&references)?;
            self.report.refs_added += references.len();
            for (name, _) in &found {
                self.database.gc_unknown_meta_delete(name)?;
                self.report.unknown_resolved += 1;
            }
        }
        Ok(())
    }

    /// Give every meta file a reference row, continuing a pass an earlier start left
    /// unfinished.
    ///
    /// A pass walks the meta file names in order and may span several starts: `state`
    /// keeps the last name looked at and a fingerprint of the names up to it. The
    /// directory is listed completely every start; if the names up to the cursor are no
    /// longer the ones the pass covered, a meta file may have been added behind the
    /// cursor without a reference, so the pass starts over. It is complete only when a
    /// start gets past the last name with the fingerprint intact.
    fn mark(&mut self, state: &mut GcState) -> Result<Mark, StorageError> {
        let unreadable = Mark {
            readable: false,
            all_marked: false,
        };
        let mut scan = match self.scan_meta_dir(state.meta_cursor.as_deref()) {
            Ok(scan) => scan,
            Err(stop) => {
                self.stop(stop);
                return Ok(unreadable);
            }
        };
        if state.meta_cursor.is_some() && scan.prefix.matches(state) {
            // Same names up to the cursor: carry on.
        } else if state.meta_cursor.is_some() {
            // Names behind the cursor were added or removed since the pass looked at
            // them: nothing it concluded there can be trusted.
            self.report.pass_restarted = true;
            state.meta_cursor = None;
            state.meta_count = 0;
            state.meta_hash.clear();
            scan = match self.scan_meta_dir(None) {
                Ok(scan) => scan,
                Err(stop) => {
                    self.stop(stop);
                    return Ok(unreadable);
                }
            };
        }
        self.report.meta_listed = scan.listed;

        let qualifying = scan.qualifying;
        let names = scan.chunk.into_sorted();
        let chunk_len = names.len();
        let mut covered = scan.prefix;
        let mut last = state.meta_cursor.clone();
        let mut found = Vec::new();
        let mut opened = 0_usize;
        let mut stopped = false;
        for name in names {
            let id = name.strip_suffix(".json").unwrap_or(&name);
            if self.database.artifact_meta_state(id)? == MetaState::Missing {
                if opened >= self.budget.meta_files {
                    self.stop(GcStop::MetaOpenBudget);
                    stopped = true;
                    break;
                }
                if self.bytes_read >= self.budget.meta_bytes {
                    self.stop(GcStop::MetaBytesBudget);
                    stopped = true;
                    break;
                }
                opened += 1;
                self.report.meta_scanned += 1;
                match self.read_meta(&name) {
                    ReadMeta::Ref(reference) => found.push(reference),
                    ReadMeta::Absent => {}
                    ReadMeta::Failed(error) => {
                        self.database
                            .gc_unknown_meta_upsert(&name, &error, self.now_ns)?;
                        self.fail(self.meta_dir.join(&name), error);
                    }
                }
            }
            covered.add(&name);
            last = Some(name);
        }
        self.database.gc_insert_refs(&found)?;
        self.report.refs_added += found.len();

        let finished = !stopped && qualifying == chunk_len;
        if finished {
            state.meta_cursor = None;
            state.meta_count = 0;
            state.meta_hash.clear();
        } else {
            if !stopped {
                self.stop(GcStop::MetaLookupBudget);
            }
            state.meta_cursor = last;
            covered.store(state);
        }
        Ok(Mark {
            readable: true,
            all_marked: finished,
        })
    }

    /// List the metadata directory: fingerprint the names up to `cursor` and keep the
    /// `meta_lookups` smallest names after it.
    fn scan_meta_dir(&mut self, cursor: Option<&str>) -> Result<Scan, GcStop> {
        let entries = match std::fs::read_dir(&self.meta_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(GcStop::MetaDirMissing);
            }
            Err(error) => {
                self.fail(self.meta_dir.clone(), &error);
                return Err(GcStop::MetaListing(error.to_string()));
            }
        };
        let mut scan = Scan {
            prefix: Fingerprint::default(),
            chunk: Smallest::new(self.budget.meta_lookups),
            qualifying: 0,
            listed: 0,
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    // A listing that failed midway says nothing about the rest.
                    self.fail(self.meta_dir.clone(), &error);
                    return Err(GcStop::MetaListing(error.to_string()));
                }
            };
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str().filter(|name| is_meta_name(name)) else {
                continue;
            };
            scan.listed += 1;
            if cursor.is_some_and(|cursor| name <= cursor) {
                scan.prefix.add(name);
            } else {
                scan.qualifying += 1;
                scan.chunk.push(name);
            }
        }
        Ok(scan)
    }

    /// Delete the references whose meta file is gone.
    fn check_orphans(&mut self, state: &mut GcState) -> Result<(), StorageError> {
        let limit = self.budget.ref_checks;
        if limit == 0 {
            self.stop(GcStop::RefCheckBudget);
            return Ok(());
        }
        let references = self
            .database
            .artifact_refs_after(state.ref_cursor.as_deref(), limit)?;
        let mut gone = Vec::new();
        for reference in &references {
            let name = format!("{}.json", reference.artifact_id);
            if !is_meta_name(&name) {
                // No meta file can have this ID, so the row protects nothing real.
                gone.push(reference.artifact_id.clone());
                continue;
            }
            let path = self.meta_dir.join(&name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    gone.push(reference.artifact_id.clone());
                }
                // Cannot tell: keep the row.
                Err(error) => self.fail(path, error),
            }
        }
        self.report.refs_removed_orphan += self.database.gc_delete_refs(&gone)?;
        if references.len() >= limit {
            state.ref_cursor = references.last().map(|last| last.artifact_id.clone());
            self.stop(GcStop::RefCheckBudget);
        } else {
            state.ref_cursor = None;
        }
        self.database.set_gc_state(state)
    }

    /// Unlink the blobs no reference names. Only called with complete references.
    fn sweep(&mut self, state: &mut GcState) -> Result<(), StorageError> {
        let listing = match list_smallest_after(
            &self.blob_dir,
            is_blob_name,
            state.blob_cursor.as_deref(),
            self.budget.blob_entries,
        ) {
            Ok(listing) => listing,
            Err(error) if error.not_found => {
                // No blob directory: nothing to collect.
                state.blob_cursor = None;
                return self.database.set_gc_state(state);
            }
            Err(error) => {
                self.report.blobs_listed = error.listed;
                self.stop(GcStop::BlobListing(error.message.clone()));
                self.fail(self.blob_dir.clone(), error.message);
                return Ok(());
            }
        };
        self.report.blobs_listed = listing.listed;
        let mut last: Option<&String> = None;
        let mut attempts = 0_usize;
        let mut finished = true;
        for name in &listing.names {
            if attempts >= self.budget.unlinks {
                self.stop(GcStop::UnlinkBudget);
                finished = false;
                break;
            }
            // A database error here propagates: "could not ask" is not "unreferenced".
            if !self.database.artifact_sha_referenced(&name[..64])? {
                attempts += 1;
                self.unlink(name);
            }
            last = Some(name);
        }
        if finished && listing.qualifying <= listing.names.len() {
            state.blob_cursor = None;
        } else {
            if listing.qualifying > listing.names.len() {
                self.stop(GcStop::BlobEntryBudget);
            }
            if let Some(last) = last {
                state.blob_cursor = Some(last.clone());
            }
        }
        self.database.set_gc_state(state)
    }

    fn unlink(&mut self, name: &str) {
        let path = self.blob_dir.join(name);
        let size = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => metadata.len(),
            // A directory or link under a blob name is not ours to delete.
            Ok(_) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.fail(path, error);
                return;
            }
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {
                self.report.blobs_unlinked += 1;
                self.report.bytes_freed += size;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => self.fail(path, error),
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    struct FailsAfter {
        data: &'static [u8],
        position: usize,
    }

    impl std::io::Read for FailsAfter {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.position >= self.data.len() {
                return Err(std::io::Error::other("disk error"));
            }
            let count = buffer.len().min(self.data.len() - self.position).min(8);
            buffer[..count].copy_from_slice(&self.data[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        }
    }

    #[test]
    fn a_read_that_fails_midway_is_a_failure_not_a_short_record() {
        // The bytes read before the error are a valid, complete JSON object: only the
        // error tells that the file may have continued.
        let complete = br#"{"artifact_id":"artifact-00000000000000000000000000000001","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
        let mut read = 0;
        let outcome = interpret_meta(
            FailsAfter {
                data: complete,
                position: 0,
            },
            "artifact-00000000000000000000000000000001",
            1,
            &mut read,
        );
        assert!(matches!(outcome, ReadMeta::Failed(error) if error.contains("disk error")));
    }

    #[test]
    fn bytes_read_before_a_failure_are_counted() {
        let data = br#"{"artifact_id":"artifact-00000000000000000000000000000001"}"#;
        let mut read = 0;
        for round in 1..=3_u64 {
            let outcome = interpret_meta(
                FailsAfter { data, position: 0 },
                "artifact-00000000000000000000000000000001",
                1,
                &mut read,
            );
            assert!(matches!(outcome, ReadMeta::Failed(_)));
            assert_eq!(read, round * data.len() as u64, "round {round}");
        }
    }

    fn fingerprint(names: &[&str]) -> Fingerprint {
        let mut fingerprint = Fingerprint::default();
        for name in names {
            fingerprint.add(name);
        }
        fingerprint
    }

    #[test]
    fn the_fingerprint_ignores_order_and_detects_every_kind_of_change() {
        let names = ["artifact-a.json", "artifact-b.json", "artifact-c.json"];
        let reference = fingerprint(&names);
        assert_eq!(
            reference,
            fingerprint(&[names[2], names[0], names[1]]),
            "order does not matter"
        );
        assert_eq!(reference.hex().len(), 64);

        // Added, removed, exchanged (same count), and doubled names all differ.
        assert_ne!(reference, fingerprint(&[names[0], names[1]]));
        assert_ne!(
            reference,
            fingerprint(&[names[0], names[1], names[2], "artifact-d.json"])
        );
        assert_ne!(
            reference,
            fingerprint(&[names[0], names[1], "artifact-d.json"])
        );
        assert_ne!(
            fingerprint(&[names[0], names[1]]),
            fingerprint(&[names[0], names[1], names[1]]),
        );
        assert_ne!(
            fingerprint(&[names[0], names[0]]),
            Fingerprint::default(),
            "a repeated name does not cancel itself out (as it would under XOR)"
        );
    }

    #[test]
    fn the_fingerprint_sum_carries_across_limbs_and_round_trips_through_the_state() {
        // Many names make carries between limbs certain; the result must not depend on
        // the order they are added in.
        let names = (0..2_000)
            .map(|n| format!("artifact-{n:032x}.json"))
            .collect::<Vec<_>>();
        let mut forward = Fingerprint::default();
        let mut backward = Fingerprint::default();
        for name in &names {
            forward.add(name);
        }
        for name in names.iter().rev() {
            backward.add(name);
        }
        assert_eq!(forward, backward);

        let mut state = GcState::default();
        assert!(!forward.matches(&state));
        forward.store(&mut state);
        assert!(forward.matches(&state));
        let mut other = forward.clone();
        other.add("artifact-extra.json");
        assert!(!other.matches(&state));
        state.meta_hash.pop();
        assert!(!forward.matches(&state), "a truncated value never matches");
    }

    #[test]
    fn blob_and_sha_names_are_exact() {
        let sha = "0123456789abcdef".repeat(4);
        assert!(is_sha256(&sha));
        assert!(!is_sha256(&sha.to_uppercase()));
        assert!(!is_sha256(&sha[..63]));
        assert!(is_blob_name(&format!("{sha}.png")));
        for foreign in [
            sha.clone(),
            format!("{sha}."),
            format!("{sha}.tar.gz"),
            format!(".{sha}.tmp"),
            format!("{}.png", sha.to_uppercase()),
            ".partial-1.tmp".to_owned(),
        ] {
            assert!(!is_blob_name(&foreign), "{foreign}");
        }
    }
}
