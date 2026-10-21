//! Thread-scoped scratch directories (ADR 0033).
//!
//! A thread's root run and its descendants share one directory whose path
//! depends only on the parent directory and the thread, so it survives
//! restores and root rebinding. Its contents live only while this process
//! owns the thread (ADR 0024): the ownership host empties it on claim and
//! deletes it on release.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use providers::{ContentBlock, Role};
use sandbox::ScratchRoot;
use sha2::{Digest, Sha256};

use crate::agent_loop::LoopState;
use crate::{AgentContext, AgentRuntime, RunId};

const NOTE_PREFIX: &str = "Temporary files (evorch): ";

pub(crate) fn scratch_note(dir: &Path) -> String {
    format!(
        "{NOTE_PREFIX}$TMPDIR is {}. Sandboxed and escalated shell commands and the file tools see this directory at the same absolute path, and every run in this thread shares it. `/tmp` and `~` point to different places in the shell and in the file tools, so keep temporary files in $TMPDIR and refer to them by this absolute path. Give them names that parallel runs in this thread will not reuse. The directory is emptied when evorch restarts or another process takes over the thread.",
        dir.display()
    )
}

pub(crate) fn emptied_notice(dir: &Path) -> String {
    format!(
        "[runtime temporary files] {} was emptied when this evorch process took ownership of the thread. Files saved there before are gone; recreate any you still need.",
        dir.display()
    )
}

fn has_current_note(context: &AgentContext, note: &str) -> bool {
    context
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Text { text } if text == note))
}

/// Add the note to the existing system message as its own block; never rewrite one.
pub(crate) fn push_system_note(context: &mut AgentContext, note: String) {
    if let Some(message) = context
        .messages
        .iter_mut()
        .find(|message| message.role == Role::System)
    {
        message.content.push(ContentBlock::Text { text: note });
    } else {
        context.push_system(&note);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ScratchKey {
    /// The thread bound by [`AgentRuntime::bind_thread_root`].
    Thread(String),
    /// A root run that no thread is bound to.
    Run(RunId),
}

impl ScratchKey {
    fn name(&self) -> String {
        match self {
            Self::Thread(thread) => thread_dir_name(thread),
            Self::Run(run) => format!("r-{run}"),
        }
    }
}

/// `t-<id>` for plain ids; `t.<digest>` (no plain id contains `.`) for the rest.
fn thread_dir_name(thread: &str) -> String {
    if !thread.is_empty()
        && thread.len() <= 96
        && thread
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        format!("t-{thread}")
    } else {
        format!("t.{}", &hex_digest(thread.as_bytes())[..32])
    }
}

fn is_thread_dir(name: &str) -> bool {
    name.starts_with("t-") || name.starts_with("t.")
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Per-thread scratch directories under one parent.
#[derive(Debug)]
pub struct ThreadScratch {
    root: ScratchRoot,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// How many times an ownership transition emptied each directory.
    resets: HashMap<String, u64>,
    /// The directory and reset count each run last observed.
    seen: HashMap<RunId, (String, u64)>,
    /// Unbound root-run directories, which no thread owner releases.
    run_dirs: HashSet<String>,
}

impl ThreadScratch {
    /// Directories under `/var/tmp/evorch-scratch-<uid>-<digest of ownership root>`,
    /// shared by every process using that ownership root.
    ///
    /// # Errors
    /// Propagates [`ScratchRoot::fixed`] failures.
    pub fn for_ownership_root(ownership_root: &Path) -> io::Result<Self> {
        let canonical = ownership_root
            .canonicalize()
            .unwrap_or_else(|_| ownership_root.to_path_buf());
        let digest = hex_digest(canonical.as_os_str().as_bytes());
        Ok(Self::new(ScratchRoot::fixed(&digest[..16])?))
    }

    /// A private parent removed when this value drops, for runtimes without ownership.
    ///
    /// # Errors
    /// Propagates directory creation failures.
    pub fn ephemeral() -> io::Result<Self> {
        Ok(Self::new(ScratchRoot::ephemeral()?))
    }

    /// Directories under a caller-chosen parent, such as a test's temporary directory.
    pub fn at(parent: PathBuf) -> Self {
        Self::new(ScratchRoot::at(parent))
    }

    fn new(root: ScratchRoot) -> Self {
        Self {
            root,
            state: Mutex::default(),
        }
    }

    pub fn parent(&self) -> &Path {
        self.root.parent()
    }

    /// The fixed directory of `thread_id`, whether or not it exists yet.
    pub fn thread_dir(&self, thread_id: &str) -> PathBuf {
        self.dir(&ScratchKey::Thread(thread_id.into()))
    }

    pub(crate) fn dir(&self, key: &ScratchKey) -> PathBuf {
        let name = key.name();
        self.root
            .dir(&name)
            .unwrap_or_else(|_| self.root.parent().join(name))
    }

    pub(crate) fn prepare(&self, key: &ScratchKey) -> io::Result<PathBuf> {
        let name = key.name();
        let mut state = self.lock();
        if matches!(key, ScratchKey::Run(_)) {
            state.run_dirs.insert(name.clone());
        }
        self.root.prepare(&name)
    }

    /// Empty `thread_id`'s directory before this process starts using it as the owner.
    pub fn reset_thread(&self, thread_id: &str) {
        let name = thread_dir_name(thread_id);
        let mut state = self.lock();
        self.discard(&name);
        *state.resets.entry(name).or_default() += 1;
    }

    /// Delete `thread_id`'s directory after this process stops owning the thread.
    pub fn release_thread(&self, thread_id: &str) {
        let name = thread_dir_name(thread_id);
        let mut state = self.lock();
        self.discard(&name);
        state.seen.retain(|_, (seen, _)| *seen != name);
    }

    /// Delete directories nobody can still be using: discarded trees, threads
    /// outside `live_threads`, and unbound runs unless another process may be live.
    pub fn sweep(&self, live_threads: &HashSet<String>, other_process_may_be_live: bool) {
        let _state = self.lock();
        if let Err(error) = self.root.purge_trash() {
            tracing::warn!(%error, parent = %self.parent().display(), "scratch trash sweep failed");
        }
        let live = live_threads
            .iter()
            .map(|thread| thread_dir_name(thread))
            .collect::<HashSet<_>>();
        let names = match self.root.names() {
            Ok(names) => names,
            Err(error) => {
                tracing::warn!(%error, parent = %self.parent().display(), "scratch sweep failed");
                return;
            }
        };
        for name in names {
            let unused = if is_thread_dir(&name) {
                !live.contains(&name)
            } else {
                name.starts_with("r-") && !other_process_may_be_live
            };
            if unused {
                self.discard(&name);
            }
        }
    }

    /// Whether `run` must be told its directory was emptied: a restored run
    /// that has not seen the latest reset. Each reset is reported once per run.
    pub(crate) fn take_emptied_notice(&self, key: &ScratchKey, run: RunId, restored: bool) -> bool {
        let name = key.name();
        let mut state = self.lock();
        let resets = state.resets.get(&name).copied().unwrap_or(0);
        let previous = state.seen.insert(run, (name.clone(), resets));
        restored
            && resets > 0
            && previous.is_none_or(|(seen, count)| seen != name || count < resets)
    }

    fn discard(&self, name: &str) {
        if let Err(error) = self.root.discard(name) {
            tracing::warn!(%error, name, parent = %self.parent().display(), "scratch removal failed");
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for ThreadScratch {
    fn drop(&mut self) {
        let names = std::mem::take(&mut self.lock().run_dirs);
        for name in names {
            self.discard(&name);
        }
    }
}

/// One run's scratch binding and the notices it still owes the model.
#[derive(Debug)]
pub(crate) struct RunScratch {
    pub(crate) dir: PathBuf,
    guidance_pending: bool,
    emptied_pending: bool,
}

impl RunScratch {
    pub(crate) const fn new(dir: PathBuf, emptied: bool) -> Self {
        Self {
            dir,
            guidance_pending: false,
            emptied_pending: emptied,
        }
    }

    /// New runs carry the note in their first system message. Restored runs keep
    /// their system message and get the note appended when it is missing or stale.
    pub(crate) fn attach(&mut self, context: &mut AgentContext, restored: bool) {
        let note = scratch_note(&self.dir);
        if restored {
            self.guidance_pending = !has_current_note(context, &note);
        } else {
            push_system_note(context, note);
        }
    }

    /// The text to append before the next model request, at most once per run.
    pub(crate) fn take_pending(&mut self) -> Option<String> {
        let mut parts = Vec::new();
        if std::mem::take(&mut self.guidance_pending) {
            parts.push(format!(
                "[runtime temporary files] {}",
                scratch_note(&self.dir)
            ));
        }
        if std::mem::take(&mut self.emptied_pending) {
            parts.push(emptied_notice(&self.dir));
        }
        (!parts.is_empty()).then(|| parts.join("\n"))
    }
}

impl LoopState {
    pub(crate) fn scratch_dir(&self) -> Option<&Path> {
        self.scratch.as_ref().map(|scratch| scratch.dir.as_path())
    }

    pub(crate) fn prepare_scratch(&self, restored: bool) -> io::Result<Option<RunScratch>> {
        let Some(runtime) = self.runtime() else {
            return Ok(None);
        };
        let scratch = runtime.thread_scratch()?;
        let run = self.task.run_id;
        let key = runtime.scratch_key(run);
        let dir = scratch.prepare(&key)?;
        let emptied = scratch.take_emptied_notice(&key, run, restored);
        Ok(Some(RunScratch::new(dir, emptied)))
    }

    /// Called with the procedure context, so an interrupted request still
    /// retries its exact saved input first. Existing history is never edited.
    pub(crate) fn append_scratch_context(&mut self) {
        if self.todo_context_deferred {
            return;
        }
        if let Some(text) = self.scratch.as_mut().and_then(RunScratch::take_pending) {
            self.context.push_user(&text);
            self.publish_message_count();
        }
    }
}

impl AgentRuntime {
    /// Share thread scratch directories with an ownership host, which empties
    /// and deletes them on ownership transitions (ADR 0024, ADR 0033).
    pub fn set_thread_scratch(&self, scratch: Arc<ThreadScratch>) {
        *self
            .shared
            .thread_scratch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(scratch);
    }

    pub(crate) fn thread_scratch(&self) -> io::Result<Arc<ThreadScratch>> {
        let mut scratch = self
            .shared
            .thread_scratch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(scratch) = scratch.as_ref() {
            return Ok(Arc::clone(scratch));
        }
        let created = Arc::new(ThreadScratch::ephemeral()?);
        *scratch = Some(Arc::clone(&created));
        Ok(created)
    }

    /// The bound thread of `run`'s delegation tree, else its root run.
    pub(crate) fn scratch_key(&self, run: RunId) -> ScratchKey {
        self.thread_of_run(run).map_or_else(
            || ScratchKey::Run(self.run_root(run).unwrap_or(run)),
            ScratchKey::Thread,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: thread ids and unbound runs / When: deriving directories / Then: they depend
    // only on the parent and the key, are distinct, and hostile ids stay one component
    #[test]
    fn directories_depend_only_on_parent_and_key() {
        let base = tempfile::tempdir().unwrap();
        let scratch = ThreadScratch::at(base.path().join("scratch"));
        let thread = scratch.thread_dir("thread-1");
        assert_eq!(thread, base.path().join("scratch/t-thread-1"));
        assert_eq!(
            ThreadScratch::at(base.path().join("scratch")).thread_dir("thread-1"),
            thread
        );
        assert_ne!(scratch.thread_dir("thread-2"), thread);
        assert_eq!(
            scratch.dir(&ScratchKey::Run(RunId::new(7))),
            base.path().join("scratch/r-run-7")
        );
        for hostile in ["../escape", "a/b", "", &"x".repeat(200)] {
            let dir = scratch.thread_dir(hostile);
            assert_eq!(dir.parent(), Some(scratch.parent()), "{hostile:?}");
            assert!(dir.file_name().unwrap().to_str().unwrap().starts_with("t."));
        }
        assert_ne!(scratch.thread_dir("a/b"), scratch.thread_dir("a/c"));
    }

    // Given: two ownership roots / When: deriving parents / Then: they are per-root,
    // per-user and stable for the same root
    #[test]
    fn ownership_roots_get_separate_fixed_parents() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let a = ThreadScratch::for_ownership_root(first.path()).unwrap();
        let b = ThreadScratch::for_ownership_root(second.path()).unwrap();
        assert_ne!(a.parent(), b.parent());
        assert_eq!(
            a.parent(),
            ThreadScratch::for_ownership_root(first.path())
                .unwrap()
                .parent()
        );
        let uid = std::fs::metadata(first.path()).map(|meta| {
            use std::os::unix::fs::MetadataExt;
            meta.uid()
        });
        let name = a.parent().file_name().unwrap().to_str().unwrap().to_owned();
        assert!(name.starts_with(&format!("evorch-scratch-{}-", uid.unwrap())));
        assert!(!a.parent().exists(), "the parent is created lazily");
    }

    // Given: a prepared thread directory with a file / When: the owner resets and then
    // releases it / Then: reset empties it, release deletes it, and the parent is 0700
    #[test]
    fn reset_empties_and_release_deletes_the_thread_directory() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let scratch = ThreadScratch::at(base.path().join("scratch"));
        let key = ScratchKey::Thread("thread".into());
        let dir = scratch.prepare(&key).unwrap();
        assert_eq!(
            std::fs::metadata(scratch.parent())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::write(dir.join("file"), "old").unwrap();
        scratch.reset_thread("thread");
        assert!(!dir.exists());
        assert_eq!(scratch.prepare(&key).unwrap(), dir);
        assert!(!dir.join("file").exists());
        scratch.release_thread("thread");
        assert!(!dir.exists());
    }

    // Given: a reset thread / When: runs observe it / Then: only restored runs that have
    // not seen this reset are told, once each, and a later reset is reported again
    #[test]
    fn emptied_notice_is_reported_once_per_run_and_reset() {
        let base = tempfile::tempdir().unwrap();
        let scratch = ThreadScratch::at(base.path().join("scratch"));
        let key = ScratchKey::Thread("thread".into());
        let (fresh, restored) = (RunId::new(1), RunId::new(2));
        assert!(!scratch.take_emptied_notice(&key, restored, true));
        scratch.reset_thread("thread");
        assert!(!scratch.take_emptied_notice(&key, fresh, false));
        assert!(!scratch.take_emptied_notice(&key, fresh, true));
        assert!(scratch.take_emptied_notice(&key, restored, true));
        assert!(!scratch.take_emptied_notice(&key, restored, true));
        scratch.reset_thread("thread");
        assert!(scratch.take_emptied_notice(&key, restored, true));
        let other = ScratchKey::Thread("other".into());
        assert!(!scratch.take_emptied_notice(&other, RunId::new(3), true));
    }

    // Given: thread, unbound-run and discarded directories / When: sweeping / Then: only
    // live threads stay, and run directories stay while another process may be live
    #[test]
    fn sweep_keeps_only_directories_a_live_owner_may_use() {
        let base = tempfile::tempdir().unwrap();
        let scratch = ThreadScratch::at(base.path().join("scratch"));
        let live = scratch.prepare(&ScratchKey::Thread("live".into())).unwrap();
        let dead = scratch.prepare(&ScratchKey::Thread("dead".into())).unwrap();
        let run = scratch.prepare(&ScratchKey::Run(RunId::new(9))).unwrap();
        let threads = HashSet::from(["live".to_owned()]);
        scratch.sweep(&threads, true);
        assert!(live.exists() && run.exists() && !dead.exists());
        scratch.sweep(&threads, false);
        assert!(live.exists() && !run.exists());
    }

    // Given: new and restored contexts / When: attaching a scratch / Then: new runs get a
    // system block, restored runs without the current note get it appended once
    #[test]
    fn restored_runs_without_the_note_get_it_appended_once() {
        let dir = PathBuf::from("/var/tmp/evorch-scratch-1-x/t-thread");
        let attach = |context: &mut AgentContext, dir: &Path, restored, emptied| {
            let mut binding = RunScratch::new(dir.to_path_buf(), emptied);
            binding.attach(context, restored);
            binding
        };
        let mut fresh = AgentContext::new(RunId::new(1), agents::Role::Worker);
        fresh.push_system("base");
        let mut binding = attach(&mut fresh, &dir, false, false);
        assert_eq!(fresh.messages[0].content.len(), 2);
        assert!(binding.take_pending().is_none());

        let mut restored = AgentContext::new(RunId::new(2), agents::Role::Worker);
        restored.push_system("base");
        let before = restored.messages.clone();
        let mut binding = attach(&mut restored, &dir, true, true);
        assert_eq!(
            restored.messages, before,
            "the system message is never rewritten"
        );
        let pending = binding.take_pending().unwrap();
        assert!(pending.contains(&scratch_note(&dir)));
        assert!(pending.contains(&emptied_notice(&dir)));
        assert!(binding.take_pending().is_none());

        let mut current = fresh.clone();
        assert!(
            attach(&mut current, &dir, true, false)
                .take_pending()
                .is_none()
        );
        assert!(
            attach(&mut current, &dir.join("moved"), true, false)
                .take_pending()
                .is_some(),
            "a stale path is corrected"
        );
    }
}
