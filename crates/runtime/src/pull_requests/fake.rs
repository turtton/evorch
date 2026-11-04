//! Scriptable stand-ins for the pull request adapters, public so that the services built on
//! them (the tracker, the review supply) can be tested without `gh` or a network.
//!
//! - [`FakePullRequestSource`] answers [`PullRequestSource::fetch`] from a script and records
//!   the calls. It honours the deadline it is given, so a scripted hang ends in
//!   [`FetchError::Timeout`] under a paused clock.
//! - [`FakeProcessRunner`] stands in for the process layer under [`GhSource`], so the real
//!   argument vector, environment and answer parsing run against canned process output.
//!
//! Neither uses a timer of its own: a test waits on the events it causes.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use storage::pull_requests::PullRequestKey;

use super::source::{ProcessError, ProcessOutput, ProcessRunner, ProcessSpec};
use super::{FetchError, FetchedPullRequest, PullRequestSource};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One scripted answer.
#[derive(Debug, Clone)]
enum Step<T> {
    Answer(T),
    /// Never answers; the caller's deadline ends it.
    Hang,
}

/// A [`PullRequestSource`] that answers from a script.
///
/// Answers are queued per pull request and consumed in order; the last one is repeated
/// when the queue would otherwise run dry only if [`Self::repeat_last`] was asked for. A
/// pull request with nothing scripted fails with [`FetchError::NotFound`].
#[derive(Debug, Default, Clone)]
pub struct FakePullRequestSource {
    state: Arc<Mutex<FakeSourceState>>,
}

#[derive(Debug, Default)]
struct FakeSourceState {
    script: HashMap<PullRequestKey, VecDeque<Step<Result<FetchedPullRequest, FetchError>>>>,
    repeat_last: bool,
    calls: Vec<(PullRequestKey, Duration)>,
}

impl FakePullRequestSource {
    /// An empty script.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a successful answer for `key`.
    pub fn push_ok(&self, key: &PullRequestKey, fetched: FetchedPullRequest) {
        self.push(key, Step::Answer(Ok(fetched)));
    }

    /// Queues a failure for `key`.
    pub fn push_err(&self, key: &PullRequestKey, error: FetchError) {
        self.push(key, Step::Answer(Err(error)));
    }

    /// Queues a fetch of `key` that never finishes on its own.
    pub fn push_hang(&self, key: &PullRequestKey) {
        self.push(key, Step::Hang);
    }

    /// Keeps answering with the last scripted answer once the queue is down to it.
    pub fn repeat_last(&self) {
        lock(&self.state).repeat_last = true;
    }

    /// Every fetch so far, in order: the key and the deadline it was given.
    #[must_use]
    pub fn calls(&self) -> Vec<(PullRequestKey, Duration)> {
        lock(&self.state).calls.clone()
    }

    fn push(&self, key: &PullRequestKey, step: Step<Result<FetchedPullRequest, FetchError>>) {
        let key = key.normalized().unwrap_or_else(|_| key.clone());
        lock(&self.state)
            .script
            .entry(key)
            .or_default()
            .push_back(step);
    }
}

#[async_trait]
impl PullRequestSource for FakePullRequestSource {
    async fn fetch(
        &self,
        key: &PullRequestKey,
        deadline: Duration,
    ) -> Result<FetchedPullRequest, FetchError> {
        let key = key
            .normalized()
            .map_err(|_| FetchError::InvalidReference("malformed key".into()))?;
        let step = {
            let mut state = lock(&self.state);
            state.calls.push((key.clone(), deadline));
            let repeat = state.repeat_last;
            state.script.get_mut(&key).and_then(|queue| {
                if repeat && queue.len() == 1 {
                    queue.front().cloned()
                } else {
                    queue.pop_front()
                }
            })
        };
        match step {
            None => Err(FetchError::not_found(
                "nothing scripted for this pull request",
            )),
            Some(Step::Answer(answer)) => answer,
            Some(Step::Hang) => {
                let _ = tokio::time::timeout(deadline, std::future::pending::<()>()).await;
                Err(FetchError::Timeout(deadline))
            }
        }
    }
}

/// A [`ProcessRunner`] that answers from a script and records the specs it was given.
#[derive(Debug, Default, Clone)]
pub struct FakeProcessRunner {
    state: Arc<Mutex<FakeRunnerState>>,
}

#[derive(Debug, Default)]
struct FakeRunnerState {
    script: VecDeque<Step<Result<ProcessOutput, ProcessError>>>,
    specs: Vec<ProcessSpec>,
}

impl FakeProcessRunner {
    /// An empty script.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a finished process.
    pub fn push_output(&self, output: ProcessOutput) {
        lock(&self.state).script.push_back(Step::Answer(Ok(output)));
    }

    /// Queues a process that exited with `code` after printing `stdout` and `stderr`.
    pub fn push_exit(&self, code: i32, stdout: &str, stderr: &str) {
        self.push_output(ProcessOutput {
            exit_code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stdout_truncated: false,
            stderr: stderr.as_bytes().to_vec(),
        });
    }

    /// Queues a failure to run the process at all.
    pub fn push_error(&self, error: ProcessError) {
        lock(&self.state).script.push_back(Step::Answer(Err(error)));
    }

    /// Queues a process that never finishes on its own.
    pub fn push_hang(&self) {
        lock(&self.state).script.push_back(Step::Hang);
    }

    /// Every process asked for so far, in order.
    #[must_use]
    pub fn specs(&self) -> Vec<ProcessSpec> {
        lock(&self.state).specs.clone()
    }
}

#[async_trait]
impl ProcessRunner for FakeProcessRunner {
    async fn run(&self, spec: ProcessSpec) -> Result<ProcessOutput, ProcessError> {
        let step = {
            let mut state = lock(&self.state);
            state.specs.push(spec);
            state.script.pop_front()
        };
        match step {
            Some(Step::Answer(answer)) => answer,
            Some(Step::Hang) => std::future::pending().await,
            None => Err(ProcessError::Spawn("nothing scripted".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use storage::pull_requests::{PullRequestFacts, PullRequestState, ReviewDecision};

    use super::*;

    fn key(number: u64) -> PullRequestKey {
        PullRequestKey::github("turtton/evorch", number).unwrap()
    }

    fn fetched(number: u64, state: PullRequestState) -> FetchedPullRequest {
        FetchedPullRequest {
            requested: key(number),
            canonical: key(number),
            url: format!("https://github.com/turtton/evorch/pull/{number}"),
            facts: PullRequestFacts {
                state,
                base_branch: "main".into(),
                head_repository: None,
                head_branch: "feat".into(),
                head_sha: "a".repeat(40),
                checks: Vec::new(),
                checks_complete: true,
                review_decision: ReviewDecision::None,
            },
            fetched_at_ns: 1,
        }
    }

    // Given: a script with two answers for one pull request / When: fetched three times /
    // Then: answers come in order, the calls are recorded, and an unscripted key is NotFound
    #[tokio::test]
    async fn answers_come_in_order_and_calls_are_recorded() {
        let source = FakePullRequestSource::new();
        source.push_ok(&key(1), fetched(1, PullRequestState::Open));
        source.push_err(&key(1), FetchError::GhNotFound);
        let deadline = Duration::from_secs(7);

        let first = source.fetch(&key(1), deadline).await.unwrap();
        let second = source.fetch(&key(1), deadline).await.unwrap_err();
        let third = source.fetch(&key(1), deadline).await.unwrap_err();
        let other = source.fetch(&key(2), deadline).await.unwrap_err();

        assert_eq!(first.facts.state, PullRequestState::Open);
        assert_eq!(second, FetchError::GhNotFound);
        assert!(matches!(third, FetchError::NotFound(_)), "{third:?}");
        assert!(matches!(other, FetchError::NotFound(_)), "{other:?}");
        assert_eq!(
            source.calls(),
            vec![
                (key(1), deadline),
                (key(1), deadline),
                (key(1), deadline),
                (key(2), deadline)
            ]
        );
    }

    // Given: repeat_last / When: fetched again and again / Then: the last answer stays
    #[tokio::test]
    async fn the_last_answer_can_repeat() {
        let source = FakePullRequestSource::new();
        source.push_ok(&key(1), fetched(1, PullRequestState::Merged));
        source.repeat_last();

        for _ in 0..3 {
            let answer = source.fetch(&key(1), Duration::from_secs(1)).await.unwrap();
            assert_eq!(answer.facts.state, PullRequestState::Merged);
        }
    }

    // Given: a scripted hang / When: fetched under a paused clock / Then: it ends at the deadline
    #[tokio::test(start_paused = true)]
    async fn a_scripted_hang_ends_at_the_deadline() {
        let source = FakePullRequestSource::new();
        source.push_hang(&key(1));
        let deadline = Duration::from_secs(5);

        let error = source.fetch(&key(1), deadline).await.unwrap_err();

        assert_eq!(error, FetchError::Timeout(deadline));
    }
}
