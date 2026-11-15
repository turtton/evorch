//! The table of read operations the four read areas of PR 13 share
//! (`diagnostics`, `memory`, `improvement`, `evaluation`). It lives here
//! because an area may only add files of its own; the other three areas use it
//! as `super::diagnostics::read_ops`.
//!
//! A query handler runs under the host's mutex and must not wait, so a read
//! that opens a database or a file is an *operation*: the handler registers it
//! and returns its view at once, and the read itself runs on a blocking worker
//! that an [`AreaTasks`] task awaits.
//!
//! * The task is a *counted* one, entered through the host's producer gate: a
//!   planned restart does not seal while a read runs, and a paused gate holds
//!   a read back instead of losing it. It writes nothing, so it needs no
//!   command permit of its own (a query has none).
//! * An accepted operation is never dropped silently. If its task ends or is
//!   aborted without a result (shutdown, a closed gate), a guard settles the
//!   operation as `Cancelled(Shutdown)`.
//! * The same id with the same request is answered from the table, which keeps
//!   the data of a ready operation; the same id with another request is a
//!   `Conflict`. The table is bounded: the oldest settled operation is
//!   dropped, and a table full of running operations refuses before accepting.
//! * Nothing is persisted. After a restart the table is empty and a client
//!   asks again (a read has no effect to repeat).
use crate::areas::AreaTasks;
use crate::queries::error;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};
use workbench_protocol::{
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::OperationId,
    operation::{CancelReason, OperationState, OperationView},
    wire::Revision,
};

/// Operations one area keeps (running and settled).
pub const MAX_READ_OPERATIONS: usize = 64;

/// Identity of a request: the digest of its wire form without paging.
pub fn digest_of<T: Serialize>(request: &T) -> [u8; 32] {
    Sha256::digest(serde_json::to_vec(request).expect("serializable request")).into()
}

/// An operation and, once it is ready, what it read.
pub struct Snapshot<T> {
    pub view: OperationView,
    pub data: Option<T>,
}

struct Entry<T> {
    digest: [u8; 32],
    view: OperationView,
    data: Option<T>,
}

struct Table<T> {
    entries: HashMap<OperationId, Entry<T>>,
    /// Oldest first.
    order: VecDeque<OperationId>,
}

type Publish = Box<dyn Fn(OperationView) + Send + Sync>;

struct Inner<T> {
    tasks: AreaTasks,
    publish: Publish,
    table: Mutex<Table<T>>,
}

/// The operations of one area. `Default` is the unwired form: nothing runs.
pub struct ReadOps<T>(Option<Arc<Inner<T>>>);

impl<T> Default for ReadOps<T> {
    fn default() -> Self {
        Self(None)
    }
}

fn lock<T>(table: &Mutex<Table<T>>) -> MutexGuard<'_, Table<T>> {
    table.lock().unwrap_or_else(PoisonError::into_inner)
}

fn view_of(id: &OperationId, state: OperationState, revision: u64) -> OperationView {
    OperationView {
        operation_id: id.clone(),
        state,
        revision: Revision(revision),
        result: None,
    }
}

impl<T: Clone + Send + 'static> ReadOps<T> {
    /// `publish` announces a settled operation (the area's event).
    pub fn new(tasks: AreaTasks, publish: impl Fn(OperationView) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(Inner {
            tasks,
            publish: Box::new(publish),
            table: Mutex::new(Table {
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
        })))
    }

    fn inner(&self) -> Result<&Arc<Inner<T>>, WorkbenchError> {
        self.0.as_ref().ok_or_else(|| {
            error(
                ErrorCode::UnsupportedCapability,
                "this read is not available in this host",
            )
        })
    }

    /// Start `work` as operation `id`, or join the operation that has this id
    /// and this `digest`.
    ///
    /// # Errors
    /// `Conflict` for an id used by another request, `LimitExceeded` when every
    /// retained operation is still running or the area has too many tasks,
    /// `Draining` once the host is stopping, `UnsupportedCapability` unwired.
    pub fn submit(
        &self,
        id: &OperationId,
        digest: [u8; 32],
        work: impl FnOnce() -> Result<T, WorkbenchError> + Send + 'static,
    ) -> Result<Snapshot<T>, WorkbenchError> {
        let inner = self.inner()?;
        {
            let mut table = lock(&inner.table);
            if let Some(entry) = table.entries.get(id) {
                return if entry.digest == digest {
                    Ok(Snapshot {
                        view: entry.view.clone(),
                        data: entry.data.clone(),
                    })
                } else {
                    Err(WorkbenchError::new(
                        ErrorCode::Conflict,
                        "this operation id was used for a different request",
                        RetryAdvice::Never,
                    ))
                };
            }
            if table.entries.len() >= MAX_READ_OPERATIONS {
                let oldest_settled = table
                    .order
                    .iter()
                    .position(|old| table.entries[old].view.state.is_final());
                let Some(position) = oldest_settled else {
                    return Err(WorkbenchError::new(
                        ErrorCode::LimitExceeded,
                        "too many reads are running",
                        RetryAdvice::SameRequest,
                    ));
                };
                let old = table.order.remove(position).expect("position is in range");
                table.entries.remove(&old);
            }
            let view = view_of(id, OperationState::Pending, 1);
            table.order.push_back(id.clone());
            table.entries.insert(
                id.clone(),
                Entry {
                    digest,
                    view,
                    data: None,
                },
            );
        }
        let started = {
            let shared = Arc::clone(inner);
            let id = id.clone();
            inner.tasks.spawn_counted("read-operation", move |_stop| {
                // Created here, not in the future's body: a task that never
                // starts (closed gate, shutdown) drops the future with the guard.
                let guard = Settle {
                    inner: shared,
                    id,
                    settled: false,
                };
                async move {
                    let result = match tokio::task::spawn_blocking(work).await {
                        Ok(result) => result,
                        Err(_) => Err(error(ErrorCode::Internal, "the read did not finish")),
                    };
                    guard.settle(result);
                    Ok(())
                }
            })
        };
        if let Err(spawn) = started {
            let mut table = lock(&inner.table);
            table.entries.remove(id);
            table.order.retain(|known| known != id);
            return Err(match spawn {
                crate::areas::SpawnError::Closed => WorkbenchError::new(
                    ErrorCode::Draining,
                    "the host is shutting down",
                    RetryAdvice::AfterReconnect,
                ),
                crate::areas::SpawnError::Full => WorkbenchError::new(
                    ErrorCode::LimitExceeded,
                    "too many tasks are running",
                    RetryAdvice::SameRequest,
                ),
            });
        }
        // The task may have settled already; answer with what the table holds.
        let table = lock(&inner.table);
        let entry = table
            .entries
            .get(id)
            .ok_or_else(|| error(ErrorCode::Internal, "the operation was dropped"))?;
        Ok(Snapshot {
            view: entry.view.clone(),
            data: entry.data.clone(),
        })
    }

    /// The view of an operation.
    ///
    /// # Errors
    /// `NotFound` for an id the table does not know (never asked, dropped, or
    /// asked before a restart).
    pub fn view(&self, id: &OperationId) -> Result<OperationView, WorkbenchError> {
        let inner = self.inner()?;
        lock(&inner.table)
            .entries
            .get(id)
            .map(|entry| entry.view.clone())
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown operation"))
    }
}

/// Settles an operation exactly once; without a result it settles it as
/// cancelled by the shutdown.
struct Settle<T: Clone + Send + 'static> {
    inner: Arc<Inner<T>>,
    id: OperationId,
    settled: bool,
}

impl<T: Clone + Send + 'static> Settle<T> {
    fn settle(mut self, result: Result<T, WorkbenchError>) {
        self.settled = true;
        let (state, data) = match result {
            Ok(data) => (OperationState::Ready, Some(data)),
            Err(failure) => (OperationState::Failed { code: failure.code }, None),
        };
        self.apply(state, data);
    }

    fn apply(&self, state: OperationState, data: Option<T>) {
        let view = {
            let mut table = lock(&self.inner.table);
            let Some(entry) = table.entries.get_mut(&self.id) else {
                return;
            };
            if entry.view.state.is_final() {
                return;
            }
            entry.view = view_of(&self.id, state, entry.view.revision.0 + 1);
            entry.data = data;
            entry.view.clone()
        };
        (self.inner.publish)(view);
    }
}

impl<T: Clone + Send + 'static> Drop for Settle<T> {
    fn drop(&mut self) {
        if !self.settled {
            self.apply(
                OperationState::Cancelled {
                    reason: CancelReason::Shutdown,
                },
                None,
            );
        }
    }
}

/// `text` as a reply carries it: secrets masked, then cut to
/// [`workbench_protocol::namespaces::diagnostics::MAX_TEXT_BYTES`] on a
/// character boundary. `total_bytes` is the size of the masked whole.
pub fn bounded(text: &str) -> workbench_protocol::namespaces::diagnostics::BoundedText {
    cut_masked(crate::queries::masked(text))
}

/// How much of a file to read for a body that is shown with [`bounded_head`]: a little
/// less than the reply limit, so that the cut marker is normally still inside it.
/// (If masking makes the text longer than it was, [`cut_masked`] cuts it at the
/// limit, which can cut the marker, but never shows more.)
pub const HEAD_BYTES: u64 = workbench_protocol::namespaces::diagnostics::MAX_TEXT_BYTES as u64 - 64;

/// What follows the kept part of a head that was cut.
const CUT_MARKER: &str = " [truncated]";

/// [`bounded`] for text that is only the *head* of a longer one (a file read up to a
/// byte limit). A secret that straddles the end of the head would not match whole,
/// and the detector cannot be relied on to recognise a fragment (it looks at a word's
/// opening, which depends on the character before it). So the unfinished word at the
/// end of a head that was cut is dropped altogether: a credential holds no
/// whitespace, so no part of one that the cut falls inside is left. The drop reaches
/// back to the last whitespace, but never further than the longest credential the
/// redactor must mask (plus some room); if there is none in that range, all of the
/// range goes. Then what is kept is masked in the usual way, and [`CUT_MARKER`] ends it.
pub fn bounded_head(
    head: &str,
    cut: bool,
) -> workbench_protocol::namespaces::diagnostics::BoundedText {
    static REDACTOR: std::sync::OnceLock<secret_guard::SecretRedactor> = std::sync::OnceLock::new();
    if !cut {
        return bounded(head);
    }
    let window = REDACTOR
        .get_or_init(secret_guard::SecretRedactor::from_env)
        .max_credential_len()
        + 256;
    let mut from = head.len().saturating_sub(window);
    while !head.is_char_boundary(from) {
        from += 1;
    }
    let keep = head[from..]
        .rfind(char::is_whitespace)
        .map_or(from, |at| from + at);
    let mut text = crate::queries::masked(&head[..keep]);
    text.push_str(CUT_MARKER);
    cut_masked(text)
}

fn cut_masked(masked: String) -> workbench_protocol::namespaces::diagnostics::BoundedText {
    let max = workbench_protocol::namespaces::diagnostics::MAX_TEXT_BYTES;
    let mut end = masked.len().min(max);
    while !masked.is_char_boundary(end) {
        end -= 1;
    }
    workbench_protocol::namespaces::diagnostics::BoundedText {
        text: masked[..end].to_owned(),
        total_bytes: masked.len() as u64,
        truncated: end < masked.len(),
    }
}

/// The first non-blank line of `text`, masked and cut to `max` bytes.
pub fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    crate::queries::text(
        &line.chars().filter(|c| !c.is_control()).collect::<String>(),
        max,
    )
}

/// A page of `items` for the read `id`, cut with `pager`. The cursor is bound to the
/// operation's own snapshot, so a later page never mixes two reads.
///
/// # Errors
/// `Conflict` for a cursor that is not this operation's, `InvalidRequest` for a
/// bad page size.
pub(crate) fn page_of<T: Clone + Serialize>(
    pager: &crate::queries::Pager,
    id: &OperationId,
    items: &[T],
    request: &workbench_protocol::queries::PageRequest,
) -> Result<workbench_protocol::views::Page<T>, WorkbenchError> {
    pager.page(
        &format!("read:{id}"),
        items,
        request,
        crate::queries::revision(&items),
    )
}

/// Names the lesson project (the repository slug) of a project directory. It
/// may run `git`, so it is only called on a blocking worker.
pub type SlugOf = std::sync::Arc<dyn Fn(&std::path::Path) -> String + Send + Sync>;

/// The slug the runtime files lessons and candidates under.
pub fn runtime_slug(runtime: runtime::AgentRuntime) -> SlugOf {
    std::sync::Arc::new(move |root| {
        runtime
            .project_slug(root)
            .unwrap_or_else(|| runtime::repository::derive_repo_slug(root))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET_REST: &str = "Zq8Wm3Nv7Xc2Rb5Ty9Uh4Jk6Lp1Sd0FgAaBbCcDd";

    /// The head of a file `limit` bytes long whose credential the cut falls inside, with
    /// `before` right in front of the credential (nothing, or a character the detector
    /// might read the opening of the word through).
    fn head_with_credential(
        limit: usize,
        before: &str,
        prefix: &str,
        keep: usize,
        spaced: bool,
    ) -> String {
        let credential = format!("{prefix}{SECRET_REST}");
        let shown = &credential[..keep - before.len()];
        let lead = limit - keep - usize::from(spaced);
        format!(
            "{}{}{before}{shown}",
            "x".repeat(lead),
            if spaced { " " } else { "" }
        )
    }

    #[test]
    fn a_credential_the_cut_falls_inside_leaves_no_fragment_whatever_precedes_it() {
        for prefix in ["sk-", "ghp_", "AKIA", "eyJ", "xoxb-", "github_pat_"] {
            for before in ["", ".", "-", "_", "a", "7", "=", ":"] {
                for spaced in [true, false] {
                    for keep in [prefix.len() + before.len(), 15, 22, 33] {
                        let head = head_with_credential(4096, before, prefix, keep, spaced);
                        assert_eq!(head.len(), 4096);
                        let out = bounded_head(&head, true);
                        let label = format!("{prefix:?} {before:?} spaced={spaced} keep={keep}");
                        assert!(!out.text.contains("Zq8W"), "{label}");
                        assert!(!out.text.contains(prefix), "{label}");
                        assert!(out.text.ends_with(CUT_MARKER), "{label}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_head_without_whitespace_loses_the_whole_range_the_credential_could_start_in() {
        let head = "y".repeat(4096);
        let out = bounded_head(&head, true);
        assert_eq!(out.text, CUT_MARKER, "nothing of an unbroken run is kept");
    }

    #[test]
    fn a_whole_head_and_the_text_before_the_last_word_are_kept() {
        assert_eq!(bounded_head("a short file", false).text, "a short file");
        let out = bounded_head("first line\nsecond line and a word", true);
        assert_eq!(
            out.text,
            format!("first line\nsecond line and a{CUT_MARKER}")
        );
    }
    use std::sync::{
        Condvar,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::mpsc;

    fn id(n: usize) -> OperationId {
        OperationId::new(format!("read-{n}")).unwrap()
    }

    /// Operations of `u32`, and the views their settling publishes.
    fn ops() -> (
        AreaTasks,
        ReadOps<u32>,
        mpsc::UnboundedReceiver<OperationView>,
    ) {
        let tasks = AreaTasks::new(tokio::runtime::Handle::current());
        let (publish, published) = mpsc::unbounded_channel();
        let ops = ReadOps::new(tasks.clone(), move |view| {
            let _ = publish.send(view);
        });
        (tasks, ops, published)
    }

    async fn settled(published: &mut mpsc::UnboundedReceiver<OperationView>) -> OperationView {
        published.recv().await.expect("the table publishes")
    }

    /// A latch a blocking read waits on, released by the test.
    #[derive(Default)]
    struct Latch(Mutex<bool>, Condvar);
    impl Latch {
        fn wait(&self) {
            let mut open = self.0.lock().unwrap();
            while !*open {
                open = self.1.wait(open).unwrap();
            }
        }
        fn open(&self) {
            *self.0.lock().unwrap() = true;
            self.1.notify_all();
        }
    }

    #[tokio::test]
    async fn the_same_id_and_request_joins_and_another_request_conflicts() {
        let (tasks, ops, mut published) = ops();
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let first = ops
            .submit(&id(1), digest_of(&"a"), move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(7)
            })
            .unwrap();
        assert_eq!(first.view.operation_id, id(1));
        let done = settled(&mut published).await;
        assert_eq!(done.state, OperationState::Ready);

        let again = ops
            .submit(&id(1), digest_of(&"a"), || {
                panic!("a joined read does not run again")
            })
            .unwrap();
        assert_eq!(
            (again.view.state, again.data),
            (OperationState::Ready, Some(7))
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let other = ops.submit(&id(1), digest_of(&"b"), || Ok(8)).err().unwrap();
        assert_eq!(other.code, ErrorCode::Conflict);
        assert_eq!(ops.view(&id(1)).unwrap().state, OperationState::Ready);
        assert_eq!(ops.view(&id(2)).unwrap_err().code, ErrorCode::NotFound);
        tasks.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_read_settles_with_its_code_and_no_data() {
        let (tasks, ops, mut published) = ops();
        ops.submit(&id(1), digest_of(&"a"), || {
            Err(error(ErrorCode::StorageUnavailable, "no database"))
        })
        .unwrap();
        let done = settled(&mut published).await;
        assert_eq!(
            done.state,
            OperationState::Failed {
                code: ErrorCode::StorageUnavailable
            }
        );
        let kept = ops.submit(&id(1), digest_of(&"a"), || Ok(1)).unwrap();
        assert_eq!(kept.data, None);
        tasks.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_read_that_never_started_is_cancelled_by_the_shutdown() {
        let (tasks, ops, mut published) = ops();
        // A paused gate holds the read back, as a sealing restart does.
        let _pause = tasks.producer_gate().pause();
        let accepted = ops
            .submit(&id(1), digest_of(&"a"), || {
                panic!("a held read must not run")
            })
            .unwrap();
        assert_eq!(accepted.view.state, OperationState::Pending);
        tasks.shutdown().await.unwrap();
        let done = settled(&mut published).await;
        assert_eq!(
            done.state,
            OperationState::Cancelled {
                reason: CancelReason::Shutdown
            }
        );
        assert_eq!(ops.view(&id(1)).unwrap().state, done.state);
        let late = ops.submit(&id(2), digest_of(&"a"), || Ok(1)).err().unwrap();
        assert_eq!(late.code, ErrorCode::Draining);
        assert_eq!(ops.view(&id(2)).unwrap_err().code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn a_slow_read_does_not_hold_the_caller_and_a_full_table_refuses() {
        let (tasks, ops, mut published) = ops();
        let latch = Arc::new(Latch::default());
        for n in 0..MAX_READ_OPERATIONS {
            let latch = latch.clone();
            // `submit` returns while the read is still blocked.
            let snapshot = ops
                .submit(&id(n), digest_of(&n), move || {
                    latch.wait();
                    Ok(n as u32)
                })
                .unwrap();
            assert_eq!(snapshot.data, None);
        }
        let refused = ops
            .submit(&id(1000), digest_of(&"more"), || Ok(0))
            .err()
            .unwrap();
        assert_eq!(refused.code, ErrorCode::LimitExceeded);
        assert_eq!(ops.view(&id(1000)).unwrap_err().code, ErrorCode::NotFound);

        latch.open();
        for _ in 0..MAX_READ_OPERATIONS {
            assert_eq!(settled(&mut published).await.state, OperationState::Ready);
        }
        // Settled operations make room: the oldest one is dropped.
        ops.submit(&id(1000), digest_of(&"more"), || Ok(0)).unwrap();
        assert_eq!(settled(&mut published).await.state, OperationState::Ready);
        assert_eq!(ops.view(&id(0)).unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(ops.view(&id(1)).unwrap().state, OperationState::Ready);
        tasks.shutdown().await.unwrap();
    }

    #[test]
    fn unwired_reads_are_unsupported() {
        let ops = ReadOps::<u32>::default();
        let refused = ops.submit(&id(1), digest_of(&"a"), || Ok(1)).err().unwrap();
        assert_eq!(refused.code, ErrorCode::UnsupportedCapability);
        assert_eq!(
            ops.view(&id(1)).unwrap_err().code,
            ErrorCode::UnsupportedCapability
        );
    }

    #[test]
    fn text_is_masked_and_cut_on_a_character_boundary() {
        let secret = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0k1L2");
        let body = format!("token {secret} あ");
        let text = bounded(&body);
        assert!(!text.text.contains(&secret), "{}", text.text);
        assert!(!text.truncated);
        assert_eq!(text.total_bytes, text.text.len() as u64);

        let long = "あ".repeat(workbench_protocol::namespaces::diagnostics::MAX_TEXT_BYTES);
        let cut = bounded(&long);
        assert!(cut.truncated);
        assert!(cut.text.len() <= workbench_protocol::namespaces::diagnostics::MAX_TEXT_BYTES);
        assert_eq!(cut.total_bytes, long.len() as u64);
        assert!(long.starts_with(&cut.text));

        assert_eq!(first_line("\n  \nsecond line\nthird", 64), "second line");
        assert_eq!(first_line("", 64), "");
    }
}
