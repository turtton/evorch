//! What the offline policy refused and which dialogs an artifact page opened,
//! reported once each to whoever operates the page.

use crate::artifact::NetLog;
use crate::browser::Status;

/// How much of the page's refusal and dialog logs has been reported already.
#[derive(Default)]
pub(crate) struct NoticeCursor {
    requests: usize,
    navigations: usize,
    popups: usize,
    dialogs: usize,
}

impl NoticeCursor {
    /// What the offline policy refused since the last call.
    pub(crate) fn take_blocked(&mut self, log: &NetLog, status: &Status) -> Vec<String> {
        let requests = log.blocked.lock().unwrap_or_else(|e| e.into_inner());
        let mut blocked = Vec::new();
        for (label, entries, seen) in [
            ("request", &requests[..], &mut self.requests),
            (
                "navigation",
                &status.blocked_navigations[..],
                &mut self.navigations,
            ),
            ("popup", &status.blocked_popups[..], &mut self.popups),
        ] {
            blocked.extend(
                entries
                    .iter()
                    .skip(*seen)
                    .map(|url| format!("{label} {url} (artifacts are offline)")),
            );
            *seen = entries.len();
        }
        blocked
    }

    /// Dialogs the page opened since the last call.
    pub(crate) fn take_dialogs(&mut self, status: &Status) -> Vec<String> {
        let dialogs = status.suppressed_dialogs[self.dialogs..].to_vec();
        self.dialogs = status.suppressed_dialogs.len();
        dialogs
    }
}
