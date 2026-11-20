//! Egress guard for the management operations (restart, quarantine and owned
//! graph administration).
//!
//! Authorization is decided before a call reaches the backend
//! ([`WorkbenchBackend::authorize_management`]); this module is the second,
//! independent protection on the way back: a management reply or error that
//! carries something shaped like a file system path is withheld. Management
//! DTOs name targets and graphs by opaque ids only (`AdminId` cannot hold a
//! `/` or `\`), so a path in an answer is a backend defect that must not
//! reach a browser. The check is a backstop; it does not replace the
//! backend's own bounded DTOs.

use crate::backend::{BackendSubscription, WebSession, WorkbenchBackend};
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;
use workbench_authority::IngressContext;
use workbench_protocol::envelope::{ReplyEnvelope, ReplyResult, RequestEnvelope};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::events::Subscribe;
use workbench_protocol::handshake::ServerHello;
use workbench_protocol::ids::ClientId;

/// The text a management answer is replaced with when it was withheld.
const WITHHELD: &str = "management reply withheld";

/// Whether any string (or object key) in `value` looks like a path.
pub(crate) fn contains_path(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains(['/', '\\']),
        Value::Array(items) => items.iter().any(contains_path),
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key.contains(['/', '\\']) || contains_path(value)),
        _ => false,
    }
}

fn withheld(reply: ReplyEnvelope) -> ReplyEnvelope {
    ReplyEnvelope {
        request_id: reply.request_id,
        result: ReplyResult::Error(WorkbenchError::new(
            ErrorCode::Internal,
            WITHHELD,
            RetryAdvice::Never,
        )),
    }
}

/// Apply the guard to the answer of a management request.
pub(crate) fn guard_reply(reply: ReplyEnvelope) -> ReplyEnvelope {
    match serde_json::to_value(&reply) {
        Ok(value) if !contains_path(&value) => reply,
        _ => withheld(reply),
    }
}

/// The backend the adapter really talks to: the embedder's backend behind the
/// management egress guard. Every other call is forwarded unchanged.
pub(crate) struct Guarded<B> {
    inner: Arc<B>,
}

impl<B> Guarded<B> {
    pub(crate) fn new(inner: Arc<B>) -> Self {
        Self { inner }
    }
}

impl<B: WorkbenchBackend> WorkbenchBackend for Guarded<B> {
    fn authorize_management(
        &self,
        context: &IngressContext,
        action: workbench_protocol::namespaces::lifecycle::AdminAction,
    ) -> Result<(), WorkbenchError> {
        self.inner.authorize_management(context, action)
    }

    fn drain_for_restart(&self) -> impl Future<Output = Result<(), WorkbenchError>> + Send {
        self.inner.drain_for_restart()
    }

    fn protect_output(&self, output: Value) -> Value {
        self.inner.protect_output(output)
    }

    async fn handle_request(
        &self,
        session: &WebSession,
        request: RequestEnvelope,
        context: IngressContext,
    ) -> ReplyEnvelope {
        let management = request.body.admin_action().is_some();
        let reply = self.inner.handle_request(session, request, context).await;
        if management {
            guard_reply(reply)
        } else {
            reply
        }
    }

    fn subscribe(
        &self,
        client: ClientId,
        subscribe: Subscribe,
        context: IngressContext,
    ) -> impl Future<Output = Result<BackendSubscription, WorkbenchError>> + Send {
        self.inner.subscribe(client, subscribe, context)
    }

    fn server_hello(
        &self,
        context: &IngressContext,
    ) -> impl Future<Output = Result<ServerHello, WorkbenchError>> + Send {
        self.inner.server_hello(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_are_found_in_values_keys_and_nesting() {
        assert!(contains_path(&json!("/usr/bin/evorch")));
        assert!(contains_path(&json!({"a": [{"b": "C:\\evorch.exe"}]})));
        assert!(contains_path(&json!({"/etc/passwd": 1})));
        assert!(!contains_path(
            &json!({"id": "target-1.x_y", "n": "12", "ok": true, "none": null})
        ));
    }
}
