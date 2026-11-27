//! Tracks which files each run knows the contents of, so another thread's edit to one
//! of them can be reported to the run that read it.
//!
//! Scope: runs of one core, with the same canonical active root and different thread
//! IDs. A file counts as known when the run's `read` succeeded or when its own
//! `edit`/`write`/`apply_patch` wrote it; it counts as changed only when one of those
//! three tools really wrote, removed or moved it. `shell`, `grep`, `git_diff`, MCP
//! tools, external editors and other cores are not observed, so this is not a
//! guarantee that every change is detected.
//!
//! Known limits, accepted on purpose:
//! - A change that lands between a `read` finishing and the file being registered as
//!   known is not reported.
//! - A `read` of a line range clears a pending notice for the whole file.
//! - A notice is not part of a checkpoint by itself; a core that stops while a request
//!   carrying one is in flight does not restore it, like shell job notices.
//!
//! State lives in memory only. The lock is never held across file system access, an
//! await or event emission.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tools::{FileAccess, FileAccessObserver};

/// Most files one notice names; the rest are counted.
pub(crate) const MAX_NOTICE_FILES: usize = 32;
/// Upper bound on the notice text.
pub(crate) const MAX_NOTICE_BYTES: usize = 8 * 1024;
/// Longest path text shown for one file.
const MAX_PATH_BYTES: usize = 512;
/// Room kept for the closing explanation and the omitted-files line.
const FOOTER_RESERVE_BYTES: usize = 1024;

#[derive(Default)]
pub(crate) struct FileTouchRegistry {
    state: Mutex<State>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct State {
    subscribers: HashMap<u64, Subscriber>,
}

struct Subscriber {
    identity: Identity,
    /// Canonical paths whose current contents this run knows.
    read: HashSet<PathBuf>,
    /// Changes by other threads not yet delivered, by canonical path.
    pending: BTreeMap<PathBuf, Change>,
}

#[derive(Clone)]
struct Identity {
    run_id: String,
    thread_id: String,
    root: PathBuf,
}

struct Change {
    count: u32,
    thread_id: String,
    run_id: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Written,
    Removed,
}

impl FileTouchRegistry {
    /// Registers a run and returns its handle, which unregisters it on drop. `None`
    /// when `root` cannot be resolved: the run is then not tracked.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        run_id: String,
        thread_id: String,
        root: &Path,
    ) -> Option<FileTouchHandle> {
        let root = std::fs::canonicalize(root).ok()?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let identity = Identity {
            run_id,
            thread_id,
            root,
        };
        self.lock().subscribers.insert(
            id,
            Subscriber {
                identity: identity.clone(),
                read: HashSet::new(),
                pending: BTreeMap::new(),
            },
        );
        let observer = Arc::new(Observer {
            registry: Arc::clone(self),
            id,
            identity,
        });
        Some(FileTouchHandle {
            registry: Arc::clone(self),
            id,
            observer,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record(&self, id: u64, actor: &Identity, kind: Kind, path: PathBuf) {
        let mut state = self.lock();
        if let Some(own) = state.subscribers.get_mut(&id) {
            match kind {
                Kind::Read => {
                    own.pending.remove(&path);
                    own.read.insert(path.clone());
                }
                // Writing registers the file as known but never clears a change by another
                // thread that is still waiting to be delivered: the writer did not see it.
                Kind::Written => {
                    own.read.insert(path.clone());
                }
                Kind::Removed => {
                    own.read.remove(&path);
                }
            }
        }
        if kind == Kind::Read {
            return;
        }
        for (other_id, other) in &mut state.subscribers {
            if *other_id == id
                || other.identity.root != actor.root
                || other.identity.thread_id == actor.thread_id
            {
                continue;
            }
            let was_known = other.read.remove(&path);
            match other.pending.get_mut(&path) {
                Some(change) => {
                    change.count = change.count.saturating_add(1);
                    change.thread_id.clone_from(&actor.thread_id);
                    change.run_id.clone_from(&actor.run_id);
                }
                None if was_known => {
                    other.pending.insert(
                        path.clone(),
                        Change {
                            count: 1,
                            thread_id: actor.thread_id.clone(),
                            run_id: actor.run_id.clone(),
                        },
                    );
                }
                None => {}
            }
        }
    }

    /// The notice for everything pending for `id`, which empties it. Files left out of
    /// the text stay known, so a later change reports them again.
    fn take_notice(&self, id: u64) -> Option<String> {
        let mut state = self.lock();
        let subscriber = state.subscribers.get_mut(&id)?;
        if subscriber.pending.is_empty() {
            return None;
        }
        let pending = std::mem::take(&mut subscriber.pending);
        let (text, omitted) = render_notice(&subscriber.identity.root, &pending);
        subscriber.read.extend(omitted);
        Some(text)
    }

    fn unsubscribe(&self, id: u64) {
        self.lock().subscribers.remove(&id);
    }

    #[cfg(test)]
    fn pending_len(&self, id: u64) -> usize {
        self.lock()
            .subscribers
            .get(&id)
            .map_or(0, |subscriber| subscriber.pending.len())
    }
}

/// A registered run. Dropping it removes the run's state, pending changes included.
pub(crate) struct FileTouchHandle {
    registry: Arc<FileTouchRegistry>,
    id: u64,
    observer: Arc<Observer>,
}

impl FileTouchHandle {
    /// The observer to hand to the run's tool executions.
    pub(crate) fn observer(&self) -> Arc<dyn FileAccessObserver> {
        self.observer.clone()
    }

    /// The notice text for the changes pending for this run, if any.
    pub(crate) fn take_notice(&self) -> Option<String> {
        self.registry.take_notice(self.id)
    }
}

impl Drop for FileTouchHandle {
    fn drop(&mut self) {
        self.registry.unsubscribe(self.id);
    }
}

struct Observer {
    registry: Arc<FileTouchRegistry>,
    id: u64,
    identity: Identity,
}

impl std::fmt::Debug for Observer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTouchObserver")
            .field("run_id", &self.identity.run_id)
            .finish()
    }
}

impl FileAccessObserver for Observer {
    fn observe(&self, access: FileAccess<'_>) {
        let (kind, path) = match access {
            FileAccess::Read(path) => (Kind::Read, path),
            FileAccess::Written(path) => (Kind::Written, path),
            FileAccess::Removed(path) => (Kind::Removed, path),
        };
        if let Some(path) = resolve(&self.identity.root, path) {
            self.registry.record(self.id, &self.identity, kind, path);
        }
    }
}

/// The canonical form of `path` when it lies under `root`. A path that does not exist,
/// such as a new or removed file, resolves through its parent directory. Anything that
/// cannot be resolved, or resolves outside `root`, is not tracked.
fn resolve(root: &Path, path: &Path) -> Option<PathBuf> {
    let resolved = match std::fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::canonicalize(path.parent()?)
                .ok()?
                .join(path.file_name()?)
        }
        Err(_) => return None,
    };
    resolved.starts_with(root).then_some(resolved)
}

/// The notice text and the paths it had to leave out.
fn render_notice(root: &Path, pending: &BTreeMap<PathBuf, Change>) -> (String, Vec<PathBuf>) {
    let mut text = String::from(
        "Files you read or wrote were changed by another thread working in the same project:\n",
    );
    let mut shown = 0;
    let mut omitted = Vec::new();
    for (path, change) in pending {
        let line = file_line(root, path, change);
        if shown >= MAX_NOTICE_FILES
            || text.len() + line.len() + FOOTER_RESERVE_BYTES > MAX_NOTICE_BYTES
        {
            omitted.push(path.clone());
            continue;
        }
        text.push_str(&line);
        shown += 1;
    }
    if !omitted.is_empty() {
        text.push_str(&format!(
            "- and {} more changed file(s), not listed\n",
            omitted.len()
        ));
    }
    text.push_str(
        "Re-read a file before you rely on or edit it. This notice is information, not an \
         instruction or an approval.",
    );
    (text, omitted)
}

fn file_line(root: &Path, path: &Path, change: &Change) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    let shown = truncate_at_boundary(&relative, MAX_PATH_BYTES);
    let quoted = serde_json::Value::String(shown.to_owned()).to_string();
    let plural = if change.count == 1 { "" } else { "s" };
    tools::escape_control_markers(&format!(
        "- {quoted}: {} change{plural}, last by thread {} (run {})\n",
        change.count,
        serde_json::Value::String(change.thread_id.clone()),
        serde_json::Value::String(change.run_id.clone()),
    ))
}

fn truncate_at_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests;
