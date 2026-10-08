//! Panics the runtime catches and reports live, instead of losing the task.
//!
//! [`catch_panic`] marks each poll of the wrapped future as a catching scope. A panic
//! hook that calls [`record_if_caught`] (installed by [`install`], and also by the
//! crash spool) stores the panic's location and backtrace for that scope, which a
//! `catch_unwind` payload alone does not carry. Without such a hook only the payload
//! message is known.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::panic::{AssertUnwindSafe, PanicHookInfo, catch_unwind};
use std::sync::Once;
use std::task::Poll;

use crate::self_improvement::{bound_text, compact_backtrace};

const MESSAGE_MAX_BYTES: usize = 4096;
const BACKTRACE_MAX_BYTES: usize = 8192;

thread_local! {
    static CATCHING: Cell<bool> = const { Cell::new(false) };
    static CAUGHT: RefCell<Option<CaughtPanic>> = const { RefCell::new(None) };
}

/// A panic caught by [`catch_panic`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaughtPanic {
    pub message: String,
    /// `file:line:col`; `None` when no recording hook was installed.
    pub location: Option<String>,
    /// [`compact_backtrace`] of the panicking thread; empty without a recording hook.
    pub backtrace: String,
}

/// Wraps the process panic hook once so caught panics keep their location and
/// backtrace; the previous hook still runs. The composition owner decides whether
/// to install it.
pub fn install() {
    static INSTALLED: Once = Once::new();
    if std::thread::panicking() {
        return;
    }
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            record_if_caught(info);
            previous(info);
        }));
    });
}

/// Records `info` when the panic happens inside [`catch_panic`]; returns whether it did.
pub(crate) fn record_if_caught(info: &PanicHookInfo<'_>) -> bool {
    if !CATCHING.get() {
        return false;
    }
    let caught = CaughtPanic {
        message: bound_text(payload_message(info.payload()), MESSAGE_MAX_BYTES),
        location: info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
        backtrace: bound_text(
            &compact_backtrace(&std::backtrace::Backtrace::force_capture().to_string()),
            BACKTRACE_MAX_BYTES,
        ),
    };
    CAUGHT.replace(Some(caught));
    true
}

/// Runs `future`, turning a panic in any of its polls into `Err`.
pub(crate) async fn catch_panic<F: Future>(future: F) -> Result<F::Output, CaughtPanic> {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(move |cx| {
        let outer = CATCHING.replace(true);
        let polled = catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx)));
        CATCHING.set(outer);
        match polled {
            Ok(poll) => poll.map(Ok),
            Err(payload) => Poll::Ready(Err(CAUGHT.take().unwrap_or_else(|| CaughtPanic {
                message: bound_text(payload_message(&*payload), MESSAGE_MAX_BYTES),
                location: None,
                backtrace: String::new(),
            }))),
        }
    })
    .await
}

pub(crate) fn payload_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_output_and_leaves_no_scope_behind() {
        assert_eq!(catch_panic(async { 7 }).await, Ok(7));
        assert!(!CATCHING.get());
    }

    #[tokio::test]
    async fn turns_a_panic_after_a_suspension_into_err() {
        let caught = catch_panic(async {
            tokio::task::yield_now().await;
            panic!("boom {}", 1);
        })
        .await
        .unwrap_err();
        assert_eq!(caught.message, "boom 1");
        assert!(!CATCHING.get());
    }
}
