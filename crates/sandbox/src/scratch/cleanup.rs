//! The filesystem is the durable queue. Only this worker recursively deletes.
use super::{IDENTITY_MARKER, ScratchRoot, TRASH_PREFIX, euid};
use rustix::{
    fs::{
        AtFlags, Dir, FileType, Mode, OFlags, Stat, fchmod, fstat, open, openat, statat, unlinkat,
    },
    io::Errno,
};
use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, CString},
    io,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock},
    thread::JoinHandle,
    time::Duration,
};
const FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const BATCH: usize = 256;
const DELAY: Duration = Duration::from_millis(2);
type Identity = (u64, u64);
// Darwin's dev_t differs from Linux's; normalize the cross-platform identity.
#[allow(clippy::unnecessary_cast)]
fn identity(s: &Stat) -> Identity {
    (s.st_dev as u64, s.st_ino as u64)
}
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "scratch directory identity changed",
    )
}
#[derive(Clone)]
struct Root {
    path: PathBuf,
    identity: Option<String>,
    inode: Identity,
}
impl Root {
    fn open(&self) -> io::Result<OwnedFd> {
        let fd = open(&self.path, FLAGS, Mode::empty())?;
        let s = fstat(&fd)?;
        if identity(&s) != self.inode || s.st_uid != euid() || s.st_mode & 0o077 != 0 {
            return Err(changed());
        }
        if let Some(expected) = &self.identity {
            let marker = openat(
                &fd,
                IDENTITY_MARKER,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let found = io::read_to_string(io::Read::take(std::fs::File::from(marker), 4096))?;
            if &found != expected {
                return Err(changed());
            }
        }
        Ok(fd)
    }
}
#[derive(Default)]
struct Work {
    roots: HashMap<PathBuf, (Root, bool)>,
    exact: HashMap<PathBuf, Identity>,
    exact_dirty: bool,
    stop: bool,
}
#[derive(Default)]
struct Shared {
    work: Mutex<Work>,
    changed: Condvar,
}
#[derive(Default)]
struct Lifecycle {
    join: Option<JoinHandle<()>>,
    stopped: bool,
}
pub(super) struct CleanupService {
    lifecycle: Mutex<Lifecycle>,
    shared: Arc<Shared>,
    delay: Duration,
    #[cfg(test)]
    events: Option<std::sync::mpsc::Sender<TestEvent>>,
    #[cfg(test)]
    step: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl CleanupService {
    fn new(delay: Duration) -> Self {
        Self {
            lifecycle: Mutex::default(),
            shared: Arc::default(),
            delay,
            #[cfg(test)]
            events: None,
            #[cfg(test)]
            step: None,
        }
    }
    pub(super) fn wake_root(&self, scratch: &ScratchRoot) -> io::Result<()> {
        // Registration is private to ScratchRoot: no arbitrary-path API.
        if !scratch.owned_parent()? {
            return Ok(());
        }
        let fd = open(&scratch.parent, FLAGS, Mode::empty())?;
        let root = Root {
            path: scratch.parent.clone(),
            identity: scratch.identity.clone(),
            inode: identity(&fstat(&fd)?),
        };
        root.open()?;
        lock(&self.shared.work)
            .roots
            .insert(root.path.clone(), (root, true));
        self.start()
    }
    pub(super) fn request_exact(&self, path: &Path) {
        // Only directories created by Scratch/ScratchRoot can reach this method.
        let result = (|| -> io::Result<()> {
            let fd = open(path, FLAGS, Mode::empty())?;
            let s = fstat(&fd)?;
            if s.st_uid != euid() || s.st_mode & 0o077 != 0 {
                return Err(changed());
            }
            let mut work = lock(&self.shared.work);
            work.exact.insert(path.into(), identity(&s));
            work.exact_dirty = true;
            Ok(())
        })();
        if let Err(error) = result {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(%error, path = %path.display(), "scratch deletion request rejected");
            }
            return;
        }
        if let Err(error) = self.start() {
            tracing::warn!(%error, "scratch collector start failed");
        }
    }
    fn start(&self) -> io::Result<()> {
        let mut life = lock(&self.lifecycle);
        if life.stopped {
            return Ok(());
        }
        if life.join.as_ref().is_some_and(JoinHandle::is_finished)
            && life.join.take().expect("worker present").join().is_err()
        {
            tracing::error!("scratch collector panicked; restarting on notification");
        }
        if life.join.is_none() {
            lock(&self.shared.work).stop = false;
            let shared = Arc::clone(&self.shared);
            let delay = self.delay;
            #[cfg(test)]
            let events = self.events.clone();
            #[cfg(test)]
            let step = self.step.clone();
            life.join = Some(
                std::thread::Builder::new()
                    .name("evorch-scratch-cleanup".into())
                    .spawn(move || {
                        worker(
                            shared,
                            delay,
                            #[cfg(test)]
                            events,
                            #[cfg(test)]
                            step,
                        )
                    })?,
            );
        }
        self.shared.changed.notify_all();
        Ok(())
    }
    fn stop(&self) -> io::Result<()> {
        // Serializes join with start. Call without scratch/registry gates held.
        let mut life = lock(&self.lifecycle);
        life.stopped = true;
        lock(&self.shared.work).stop = true;
        self.shared.changed.notify_all();
        if let Some(join) = life.join.take() {
            join.join()
                .map_err(|_| io::Error::other("scratch collector panicked while joining"))?;
        }
        Ok(())
    }
    fn resume(&self) -> io::Result<()> {
        {
            let mut life = lock(&self.lifecycle);
            life.stopped = false;
            let mut work = lock(&self.shared.work);
            for (_, dirty) in work.roots.values_mut() {
                *dirty = true;
            }
            work.exact_dirty = true;
        }
        self.start()
    }
}
pub(super) fn service() -> &'static CleanupService {
    static SERVICE: OnceLock<CleanupService> = OnceLock::new();
    SERVICE.get_or_init(|| CleanupService::new(DELAY))
}
/// Stop and join at the next entry boundary, without draining every tree.
/// Producers must be stopped first. Pending trash remains discoverable on restart.
pub fn stop_scratch_cleanup() -> io::Result<()> {
    service().stop()
}
/// Resume trash collection after failed exec; does not touch live scratch.
pub fn resume_scratch_cleanup() -> io::Result<()> {
    service().resume()
}
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Complete,
    Stopped,
}
struct Pace<'a> {
    shared: &'a Shared,
    delay: Duration,
    entries: usize,
    #[cfg(test)]
    step: Option<&'a (dyn Fn() + Send + Sync)>,
}
impl Pace<'_> {
    fn stopped(&self) -> bool {
        lock(&self.shared.work).stop
    }
    fn entry(&mut self) -> bool {
        self.entries += 1;
        #[cfg(test)]
        if let Some(step) = self.step {
            step();
        }
        if self.entries.is_multiple_of(BATCH) && !self.delay.is_zero() {
            let work = lock(&self.shared.work);
            if work.stop {
                return true;
            }
            let (work, _) = self
                .shared
                .changed
                .wait_timeout_while(work, self.delay, |w| !w.stop)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            work.stop
        } else {
            self.stopped()
        }
    }
}
fn writable(fd: &OwnedFd) -> io::Result<Identity> {
    let s = fstat(fd)?;
    if s.st_uid != euid() {
        return Err(changed());
    }
    if s.st_mode & 0o700 != 0o700 {
        // Repair only the opened, euid-owned directory; no pathname fallback.
        fchmod(fd, Mode::from_raw_mode(s.st_mode) | Mode::RWXU)?;
    }
    Ok(identity(&s))
}
fn same_entry(parent: impl AsFd, name: &CStr, expected: Identity) -> io::Result<()> {
    if identity(&statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?) == expected {
        Ok(())
    } else {
        Err(changed())
    }
}
fn remove_tree(
    parent: &OwnedFd,
    name: &CStr,
    expected: Option<Identity>,
    pace: &mut Pace<'_>,
) -> io::Result<Outcome> {
    if pace.stopped() {
        return Ok(Outcome::Stopped);
    }
    let root = match openat(parent, name, FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(Outcome::Complete),
        Err(error @ (Errno::NOTDIR | Errno::LOOP)) if expected.is_none() => {
            let s = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if FileType::from_raw_mode(s.st_mode) == FileType::Directory {
                return Err(error.into());
            }
            unlinkat(parent, name, AtFlags::empty())?;
            return Ok(if pace.entry() {
                Outcome::Stopped
            } else {
                Outcome::Complete
            });
        }
        Err(error) => return Err(error.into()),
    };
    let original = identity(&fstat(&root)?);
    if expected.is_some_and(|expected| expected != original) {
        return Err(changed());
    }
    let id = writable(&root)?;
    let mut stack = vec![(name.to_owned(), id, Dir::new(root)?)];
    while let Some((_, _, entries)) = stack.last_mut() {
        if pace.stopped() {
            return Ok(Outcome::Stopped);
        }
        match entries.next() {
            Some(Ok(entry)) => {
                let name = entry.file_name();
                if name == c"." || name == c".." {
                    continue;
                }
                let fd = entries.fd()?;
                match openat(fd, name, FLAGS, Mode::empty()) {
                    Ok(child) => {
                        let id = writable(&child)?;
                        stack.push((name.to_owned(), id, Dir::new(child)?));
                    }
                    Err(Errno::NOENT) => {}
                    Err(error @ (Errno::NOTDIR | Errno::LOOP)) => {
                        let s = match statat(fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                            Ok(s) => s,
                            Err(Errno::NOENT) => continue,
                            Err(error) => return Err(error.into()),
                        };
                        if FileType::from_raw_mode(s.st_mode) == FileType::Directory {
                            return Err(error.into());
                        }
                        match unlinkat(fd, name, AtFlags::empty()) {
                            Ok(()) | Err(Errno::NOENT) => {}
                            Err(error) => return Err(error.into()),
                        }
                        if pace.entry() {
                            return Ok(Outcome::Stopped);
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Some(Err(error)) => return Err(error.into()),
            None => {
                let (name, id, _entries) = stack.pop().expect("frame present");
                let parent_fd = match stack.last() {
                    Some((_, _, dir)) => dir.fd()?,
                    None => parent.as_fd(),
                };
                match same_entry(parent_fd, &name, id) {
                    Ok(()) => match unlinkat(parent_fd, &name, AtFlags::REMOVEDIR) {
                        Ok(()) | Err(Errno::NOENT) => {}
                        Err(error) => return Err(error.into()),
                    },
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                if pace.entry() {
                    return Ok(Outcome::Stopped);
                }
            }
        }
    }
    Ok(Outcome::Complete)
}
fn scan(root: &Root, pace: &mut Pace<'_>, failures: &mut HashSet<PathBuf>) -> io::Result<Outcome> {
    let fd = match root.open() {
        Ok(fd) => fd,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Outcome::Complete),
        Err(error) => return Err(error),
    };
    for entry in Dir::read_from(&fd)? {
        if pace.stopped() {
            return Ok(Outcome::Stopped);
        }
        let entry = entry?;
        if !entry
            .file_name()
            .to_bytes()
            .starts_with(TRASH_PREFIX.as_bytes())
        {
            continue;
        }
        let path = root.path.join(entry.file_name().to_string_lossy().as_ref());
        match remove_tree(&fd, entry.file_name(), None, pace) {
            Ok(Outcome::Stopped) => return Ok(Outcome::Stopped),
            Ok(Outcome::Complete) => {
                failures.remove(&path);
            }
            Err(error) => {
                if failures.insert(path.clone()) {
                    tracing::warn!(%error, path = %path.display(), "scratch trash retained after deletion failure");
                }
            }
        }
    }
    Ok(Outcome::Complete)
}
fn worker(
    shared: Arc<Shared>,
    delay: Duration,
    #[cfg(test)] events: Option<std::sync::mpsc::Sender<TestEvent>>,
    #[cfg(test)] step: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    let mut failures = HashSet::new();
    loop {
        let (roots, exact) = {
            let mut work = lock(&shared.work);
            while !work.stop && !work.exact_dirty && !work.roots.values().any(|(_, dirty)| *dirty) {
                work = shared
                    .changed
                    .wait(work)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if work.stop {
                break;
            }
            // Clear before scanning: a concurrent successful rename cannot lose its wake.
            let roots: Vec<_> = work
                .roots
                .values_mut()
                .filter_map(|(root, dirty)| std::mem::take(dirty).then(|| root.clone()))
                .collect();
            let exact: Vec<_> = if std::mem::take(&mut work.exact_dirty) {
                work.exact.iter().map(|(p, id)| (p.clone(), *id)).collect()
            } else {
                vec![]
            };
            (roots, exact)
        };
        let mut pace = Pace {
            shared: &shared,
            delay,
            entries: 0,
            #[cfg(test)]
            step: step.as_deref(),
        };
        for root in roots {
            #[cfg(test)]
            if let Some(events) = &events {
                let _ = events.send(TestEvent::Scan(root.path.clone()));
            }
            if let Err(error) = scan(&root, &mut pace, &mut failures)
                && failures.insert(root.path.clone())
            {
                tracing::warn!(%error, parent = %root.path.display(), "scratch root scan failed");
            }
        }
        for (path, id) in exact {
            if pace.stopped() {
                break;
            }
            let result = (|| -> io::Result<Outcome> {
                let parent = path.parent().ok_or_else(changed)?;
                let name = CString::new(path.file_name().ok_or_else(changed)?.as_encoded_bytes())?;
                remove_tree(
                    &open(parent, FLAGS, Mode::empty())?,
                    &name,
                    Some(id),
                    &mut pace,
                )
            })();
            match result {
                Ok(Outcome::Complete) => {
                    lock(&shared.work).exact.remove(&path);
                    failures.remove(&path);
                }
                Ok(Outcome::Stopped) => {}
                Err(error) => {
                    if failures.insert(path.clone()) {
                        tracing::warn!(%error, path = %path.display(), "scratch exact tree retained");
                    }
                }
            }
        }
        #[cfg(test)]
        if let Some(events) = &events {
            let _ = events.send(TestEvent::Pass);
        }
    }
    #[cfg(test)]
    if let Some(events) = &events {
        let _ = events.send(TestEvent::Stopped);
    }
}
#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod tests;
#[cfg(test)]
#[derive(Debug)]
enum TestEvent {
    Scan(PathBuf),
    Pass,
    Stopped,
}
#[cfg(test)]
pub(super) fn remove_for_test(path: &Path) -> io::Result<()> {
    let shared = Shared::default();
    let mut pace = Pace {
        shared: &shared,
        delay: Duration::ZERO,
        entries: 0,
        step: None,
    };
    let parent = open(path.parent().unwrap(), FLAGS, Mode::empty())?;
    let name = CString::new(path.file_name().unwrap().as_encoded_bytes())?;
    remove_tree(&parent, &name, None, &mut pace).map(|_| ())
}
