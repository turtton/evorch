//! Bounded LRU of prepared markdown, so unchanged messages are not re-parsed.
//!
//! Entries keep the full source and base directory they were prepared from and
//! a hit requires an exact match, so a hash collision can only cost a miss,
//! never show another message's content. Image links resolve through the
//! filesystem, so a hit is also checked against what they resolve to now.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::Prepared;

/// Transcripts render every entry each frame; keep room for all of them.
const MAX_ENTRIES: usize = 512;
/// Keeps prepared copies of huge messages from pinning memory.
const MAX_BYTES: usize = 16 * 1024 * 1024;

struct Entry {
    hash: u64,
    base: Option<PathBuf>,
    source: Box<str>,
    prepared: Arc<Prepared>,
    bytes: usize,
    last_used: u64,
}

pub(super) struct PreparedCache {
    entries: Vec<Entry>,
    bytes: usize,
    clock: u64,
    max_entries: usize,
    max_bytes: usize,
}

impl Default for PreparedCache {
    fn default() -> Self {
        Self::new(MAX_ENTRIES, MAX_BYTES)
    }
}

impl PreparedCache {
    pub(super) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: Vec::new(),
            bytes: 0,
            clock: 0,
            max_entries,
            max_bytes,
        }
    }

    /// The entry for `(source, base)`, running `prepare` only on a miss.
    pub(super) fn get_or_prepare(
        &mut self,
        source: &str,
        base: Option<&Path>,
        prepare: impl FnOnce() -> Prepared,
    ) -> Arc<Prepared> {
        let hash = key_hash(source, base);
        self.clock += 1;
        let clock = self.clock;
        if let Some(index) = self.entries.iter().position(|entry| {
            entry.hash == hash && *entry.source == *source && entry.base.as_deref() == base
        }) {
            let entry = &mut self.entries[index];
            if entry.prepared.is_current(base) {
                entry.last_used = clock;
                return Arc::clone(&entry.prepared);
            }
            // Its images now resolve differently; prepare again below.
            self.bytes -= self.entries.swap_remove(index).bytes;
        }
        let prepared = Arc::new(prepare());
        let bytes = source.len()
            + prepared.bytes()
            + base.map_or(0, |base| base.as_os_str().len())
            + size_of::<Entry>();
        if bytes <= self.max_bytes && self.max_entries > 0 {
            self.bytes += bytes;
            self.entries.push(Entry {
                hash,
                base: base.map(Path::to_path_buf),
                source: source.into(),
                prepared: Arc::clone(&prepared),
                bytes,
                last_used: clock,
            });
            self.evict();
        }
        prepared
    }

    /// Drops least recently used entries until both limits hold.
    fn evict(&mut self) {
        while self.entries.len() > self.max_entries || self.bytes > self.max_bytes {
            let Some(oldest) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| index)
            else {
                return;
            };
            self.bytes -= self.entries.swap_remove(oldest).bytes;
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }
}

fn key_hash(source: &str, base: Option<&Path>) -> u64 {
    let mut hasher = DefaultHasher::new();
    base.hash(&mut hasher);
    source.hash(&mut hasher);
    hasher.finish()
}
