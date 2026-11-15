//! One login: what a flow is, and the session that keeps its single outcome.
//!
//! A flow is what differs between providers (where the browser is sent, how a
//! callback is read, how the code is exchanged). The session is what must not
//! differ: the callback is accepted once, whichever of the loopback listener
//! and a pasted input arrives first, a pasted input is checked against the
//! `state` this login started with, and the outcome is claimed once.
//!
//! Nothing here logs, publishes or stores a callback, a code or a pasted text.
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tokio::sync::Notify;
use workbench_protocol::{
    ids::OperationId,
    namespaces::auth::{
        LoginInputOutcome, LoginInputRejection, LoginMethod, LoginPhase, LoginView,
    },
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a callback carries to the exchange: the code, or the text the flow
/// needs to make it. Never printed.
pub struct Callback(String);

impl Callback {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Callback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Callback(<redacted>)")
    }
}

/// Why a login ended without credentials. Fixed sentences only: the text of a
/// provider error can carry the code it rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginFailure {
    PortBusy,
    Network,
    Rejected,
    Unavailable,
    StoreUnavailable,
}

impl LoginFailure {
    pub const fn message(self) -> &'static str {
        match self {
            Self::PortBusy => "the login callback port is in use; close the other login and retry",
            Self::Network => "the provider could not be reached; retry the login",
            Self::Rejected => "the provider rejected the authorization; start the login again",
            Self::Unavailable => "the login could not run; start it again",
            Self::StoreUnavailable => "the credentials could not be saved",
        }
    }
}

/// The provider side of one login.
pub trait LoginFlow: Send + Sync {
    /// Where the person is sent. Shown by `get_login` only.
    fn authorize_url(&self) -> &str;

    /// Whether a pasted redirect URL or code can finish this login.
    fn accepts_input(&self) -> bool;

    /// Read a pasted input against this login's own `state`.
    ///
    /// # Errors
    /// `Malformed` for text that is no callback of this flow, `StateMismatch`
    /// for one whose `state` is not this login's. Nothing is consumed.
    fn check_input(&self, input: &str) -> Result<Callback, LoginInputRejection>;

    /// Resolves with the callback the loopback listener receives, never for a
    /// flow without one. The listener stops once `stop` is set.
    fn wait_loopback(&self, stop: Arc<AtomicBool>)
    -> BoxFuture<'_, Result<Callback, LoginFailure>>;

    /// Exchange `callback` for credentials and store them.
    fn complete(&self, callback: Callback) -> BoxFuture<'_, Result<Option<u64>, LoginFailure>>;
}

/// Starts flows. Production drives the real providers; a test supplies its own.
pub trait LoginStarter: Send + Sync {
    /// Prepare a login for `method`, whose credentials live under `account`.
    /// Cheap and synchronous: it binds a loopback port and builds the URL, and
    /// waits for nothing.
    ///
    /// # Errors
    /// `PortBusy` or `Unavailable` when the login cannot be offered at all.
    fn begin(&self, method: LoginMethod, account: &str)
    -> Result<Arc<dyn LoginFlow>, LoginFailure>;
}

struct Inner {
    phase: LoginPhase,
    /// A callback was committed (from either source).
    input_used: bool,
    callback: Option<Callback>,
}

pub(super) struct Session {
    pub(super) login_id: OperationId,
    pub(super) profile: String,
    pub(super) method: LoginMethod,
    pub(super) flow: Arc<dyn LoginFlow>,
    deadline: Instant,
    inner: Mutex<Inner>,
    pub(super) wake: Notify,
    /// Tells a blocking loopback listener to stop.
    pub(super) stop_listener: Arc<AtomicBool>,
}

impl Session {
    pub(super) fn new(
        login_id: OperationId,
        profile: String,
        method: LoginMethod,
        flow: Arc<dyn LoginFlow>,
        deadline: Instant,
    ) -> Self {
        Self {
            login_id,
            profile,
            method,
            flow,
            deadline,
            inner: Mutex::new(Inner {
                phase: LoginPhase::Waiting,
                input_used: false,
                callback: None,
            }),
            wake: Notify::new(),
            stop_listener: Arc::new(AtomicBool::new(false)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn phase(&self) -> LoginPhase {
        self.lock().phase
    }

    pub(super) fn view(&self) -> LoginView {
        let inner = self.lock();
        let live = inner.phase != LoginPhase::Settled;
        LoginView {
            login_id: self.login_id.clone(),
            profile: self.profile.clone(),
            method: self.method,
            phase: inner.phase,
            authorize_url: live.then(|| self.flow.authorize_url().to_owned()),
            accepts_input: inner.phase == LoginPhase::Waiting && self.flow.accepts_input(),
        }
    }

    /// A pasted input. Only the first callback of this login is committed.
    pub(super) fn submit(&self, input: &str, now: Instant) -> LoginInputOutcome {
        let reject = |reason| LoginInputOutcome::Rejected { reason };
        let mut inner = self.lock();
        if inner.input_used {
            return reject(LoginInputRejection::AlreadyUsed);
        }
        if inner.phase != LoginPhase::Waiting {
            return reject(LoginInputRejection::NotWaiting);
        }
        if now >= self.deadline {
            return reject(LoginInputRejection::Expired);
        }
        if !self.flow.accepts_input() {
            return reject(LoginInputRejection::NotWaiting);
        }
        match self.flow.check_input(input) {
            Ok(callback) => {
                inner.phase = LoginPhase::Completing;
                inner.input_used = true;
                inner.callback = Some(callback);
                drop(inner);
                self.wake.notify_one();
                LoginInputOutcome::Accepted
            }
            Err(reason) => reject(reason),
        }
    }

    /// The loopback listener's callback. `false` when another source won.
    pub(super) fn commit_loopback(&self, callback: Callback) -> bool {
        let mut inner = self.lock();
        if inner.input_used || inner.phase != LoginPhase::Waiting {
            return false;
        }
        inner.phase = LoginPhase::Completing;
        inner.input_used = true;
        inner.callback = Some(callback);
        true
    }

    pub(super) fn take_callback(&self) -> Option<Callback> {
        self.lock().callback.take()
    }

    /// Claim the outcome while nothing was committed yet (a person's cancel, a
    /// restart, the deadline). `false` when a callback was committed or the
    /// outcome was claimed already.
    pub(super) fn claim_while_waiting(&self) -> bool {
        let mut inner = self.lock();
        if inner.phase != LoginPhase::Waiting {
            return false;
        }
        inner.phase = LoginPhase::Settled;
        inner.callback = None;
        drop(inner);
        self.stop_listener.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        true
    }

    /// Claim the outcome from any phase; `false` when it was claimed already.
    pub(super) fn claim(&self) -> bool {
        let mut inner = self.lock();
        if inner.phase == LoginPhase::Settled {
            return false;
        }
        inner.phase = LoginPhase::Settled;
        inner.callback = None;
        drop(inner);
        self.stop_listener.store(true, Ordering::SeqCst);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Fixed {
        state: &'static str,
    }

    impl LoginFlow for Fixed {
        fn authorize_url(&self) -> &str {
            "https://example.invalid/authorize"
        }

        fn accepts_input(&self) -> bool {
            true
        }

        fn check_input(&self, input: &str) -> Result<Callback, LoginInputRejection> {
            match input.split_once('#') {
                None => Err(LoginInputRejection::Malformed),
                Some((_, state)) if state != self.state => Err(LoginInputRejection::StateMismatch),
                Some((code, _)) => Ok(Callback::new(code)),
            }
        }

        fn wait_loopback(
            &self,
            _stop: Arc<AtomicBool>,
        ) -> BoxFuture<'_, Result<Callback, LoginFailure>> {
            Box::pin(std::future::pending())
        }

        fn complete(&self, _: Callback) -> BoxFuture<'_, Result<Option<u64>, LoginFailure>> {
            Box::pin(async { Ok(None) })
        }
    }

    fn session(deadline: Instant) -> Session {
        Session::new(
            OperationId::new("login-1").unwrap(),
            "codex".into(),
            LoginMethod::Claude,
            Arc::new(Fixed { state: "s1" }),
            deadline,
        )
    }

    fn rejected(outcome: LoginInputOutcome) -> LoginInputRejection {
        match outcome {
            LoginInputOutcome::Rejected { reason } => reason,
            LoginInputOutcome::Accepted => panic!("accepted"),
        }
    }

    #[test]
    fn a_pasted_input_must_carry_this_logins_state_and_is_used_once() {
        let now = Instant::now();
        let session = session(now + Duration::from_secs(60));
        assert_eq!(
            rejected(session.submit("code-only", now)),
            LoginInputRejection::Malformed
        );
        assert_eq!(
            rejected(session.submit("code#other", now)),
            LoginInputRejection::StateMismatch
        );
        assert_eq!(session.phase(), LoginPhase::Waiting);
        assert_eq!(session.submit("code#s1", now), LoginInputOutcome::Accepted);
        assert_eq!(
            rejected(session.submit("code#s1", now)),
            LoginInputRejection::AlreadyUsed
        );
        assert_eq!(session.take_callback().unwrap().expose(), "code");
    }

    #[test]
    fn the_first_of_loopback_and_paste_is_committed() {
        let now = Instant::now();
        let loopback_first = session(now + Duration::from_secs(60));
        assert!(loopback_first.commit_loopback(Callback::new("from-loopback")));
        assert_eq!(
            rejected(loopback_first.submit("late#s1", now)),
            LoginInputRejection::AlreadyUsed
        );
        assert_eq!(
            loopback_first.take_callback().unwrap().expose(),
            "from-loopback"
        );

        let paste_first = session(now + Duration::from_secs(60));
        assert_eq!(
            paste_first.submit("pasted#s1", now),
            LoginInputOutcome::Accepted
        );
        assert!(!paste_first.commit_loopback(Callback::new("from-loopback")));
        assert_eq!(paste_first.take_callback().unwrap().expose(), "pasted");
    }

    #[test]
    fn an_input_after_the_deadline_or_the_end_is_refused() {
        let now = Instant::now();
        let expired = session(now);
        assert_eq!(
            rejected(expired.submit("code#s1", now)),
            LoginInputRejection::Expired
        );
        let ended = session(now + Duration::from_secs(60));
        assert!(ended.claim_while_waiting());
        assert_eq!(
            rejected(ended.submit("code#s1", now)),
            LoginInputRejection::NotWaiting
        );
        assert!(ended.view().authorize_url.is_none());
    }

    #[test]
    fn a_committed_callback_is_not_cancelled_as_waiting() {
        let session = session(Instant::now() + Duration::from_secs(60));
        assert!(session.commit_loopback(Callback::new("c")));
        assert!(!session.claim_while_waiting());
        assert!(session.claim());
        assert!(!session.claim());
    }
}
