//! 引数が JSON として解釈できなかったツール呼び出しの診断を発行する。

use event_bus::event::diagnostic_codes;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event};

use super::AttemptObserver;
use crate::stream::MalformedToolCall;

impl AttemptObserver {
    /// モデル出力の崩れかストリーム組み立ての不具合かは区別できないため、
    /// 記録のみ（自己改善では TransientOrExternal）とする。引数本文は載せない。
    pub(crate) fn emit_malformed_tool_call(&self, call: &MalformedToolCall) {
        let Some(bus) = self.bus.as_ref() else {
            return;
        };
        bus.emit(Event::new(DiagnosticEvent {
            source: "providers.stream".into(),
            severity: DiagnosticSeverity::Warning,
            code: diagnostic_codes::TOOL_ARGUMENTS_MALFORMED.into(),
            detail: format!(
                "provider={} profile={:?} protocol={} model={} request_id={} tool={} arguments_len={} arguments_sha256={} error={}",
                self.provider,
                self.profile,
                self.protocol,
                self.model,
                self.request_id,
                call.name,
                call.arguments_len,
                call.arguments_sha256,
                call.error,
            ),
            run_id: self.observation_run_id(),
            thread_id: None,
            call_id: Some(call.id.clone()),
        }));
    }
}
