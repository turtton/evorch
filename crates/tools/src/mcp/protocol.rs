//! Session-local era/version state. Transport generations never share request IDs.
pub use super::transport::api::{ProtocolEra, ProtocolVersion};
use super::{
    McpClientInfo, McpError, McpErrorKind,
    codec::matches_response,
    headers::{CAPABILITIES_META, INFO_META, VERSION_META},
    transport::api::*,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
pub const MODERN_VERSION: &str = "2026-07-28";
pub const LEGACY_VERSION: &str = "2025-11-25";
pub fn version_string(version: ProtocolVersion) -> &'static str {
    match version {
        ProtocolVersion::V2026_07_28 => MODERN_VERSION,
        ProtocolVersion::V2025_11_25 => LEGACY_VERSION,
        ProtocolVersion::V2025_06_18 => "2025-06-18",
        ProtocolVersion::V2025_03_26 => "2025-03-26",
        ProtocolVersion::V2024_11_05 => "2024-11-05",
    }
}
pub fn legacy_version(value: &str, kind: TransportKind) -> Option<ProtocolVersion> {
    match value {
        "2025-11-25" => Some(ProtocolVersion::V2025_11_25),
        "2025-06-18" => Some(ProtocolVersion::V2025_06_18),
        "2025-03-26" => Some(ProtocolVersion::V2025_03_26),
        "2024-11-05" if kind == TransportKind::Stdio => Some(ProtocolVersion::V2024_11_05),
        _ => None,
    }
}
pub fn is_modern_error(error: &RpcError) -> bool {
    matches!(error.code, -32022..=-32020)
}
pub fn unsupported_request(id: RpcId) -> RpcFrame {
    RpcFrame::Error {
        jsonrpc: "2.0".into(),
        id: Some(id),
        error: RpcError {
            code: -32601,
            message: "Client capability not supported".into(),
            data: None,
        },
    }
}

pub struct Session {
    transport: Arc<dyn McpTransport>,
    label: String,
    kind: TransportKind,
    info: McpClientInfo,
    timeout: Duration,
    cancel: CancelContext,
    era: ProtocolEra,
    version: ProtocolVersion,
    next_id: i64,
    pub last_received_bytes: usize,
}
impl Session {
    pub async fn connect(
        transport: Arc<dyn McpTransport>,
        label: String,
        kind: TransportKind,
        info: McpClientInfo,
        timeout: Duration,
        cancel: CancelContext,
    ) -> Result<Self, McpError> {
        let mut session = Self {
            transport,
            label,
            kind,
            info,
            timeout,
            cancel,
            era: ProtocolEra::Modern,
            version: ProtocolVersion::V2026_07_28,
            next_id: 1,
            last_received_bytes: 0,
        };
        if session.label.is_empty() || session.label.len() > 64 || session.timeout.is_zero() {
            return Err(session.error("server/discover", None, McpErrorKind::Configuration));
        }
        let result = session.discover().await;
        if let Err(error) = result {
            let _ = session
                .transport
                .shutdown(ShutdownReason::InitializationFailed)
                .await;
            return Err(error);
        }
        Ok(session)
    }
    pub(crate) fn set_request_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }
    pub fn era(&self) -> ProtocolEra {
        self.era
    }
    pub fn version(&self) -> ProtocolVersion {
        self.version
    }
    pub fn last_id(&self) -> i64 {
        self.next_id - 1
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn error(&self, method: &'static str, id: Option<i64>, kind: McpErrorKind) -> McpError {
        McpError {
            server: self.label.clone(),
            method,
            request_id: id,
            kind,
        }
    }
    async fn discover(&mut self) -> Result<(), McpError> {
        for attempt in 0..2 {
            let (id, response) = match self.exchange("server/discover", json!({})).await {
                Ok(value) => value,
                Err(error)
                    if self.kind == TransportKind::Stdio && error.kind == McpErrorKind::Timeout =>
                {
                    return self.initialize().await;
                }
                Err(error) => return Err(error),
            };
            let frame = self.correlated(&response, id)?;
            if let Some(status) = response.status.filter(|s| !(200..300).contains(s)) {
                if status != 400 {
                    return Err(self.error(
                        "server/discover",
                        Some(id),
                        McpErrorKind::HttpStatus(status),
                    ));
                }
                if !matches!(frame,Some(RpcFrame::Error {error,..}) if is_modern_error(error)) {
                    return self.initialize().await;
                }
            }
            match frame {
                Some(RpcFrame::Result { result, .. }) => {
                    self.validate_result("server/discover", result)?;
                    if !result
                        .get("supportedVersions")
                        .and_then(Value::as_array)
                        .is_some_and(|vs| {
                            !vs.is_empty()
                                && vs.iter().all(Value::is_string)
                                && vs.iter().any(|v| v == MODERN_VERSION)
                        })
                        || !result
                            .get("capabilities")
                            .and_then(Value::as_object)
                            .is_some_and(|c| c.get("tools").is_some_and(Value::is_object))
                    {
                        return Err(self.error(
                            "server/discover",
                            Some(id),
                            McpErrorKind::Protocol,
                        ));
                    }
                    return Ok(());
                }
                Some(RpcFrame::Error { error, .. }) if is_modern_error(error) => {
                    if error.code == -32022 {
                        let compatible = error.data.as_ref().is_some_and(|d| {
                            d.get("requested").is_some_and(Value::is_string)
                                && d.get("supported")
                                    .and_then(Value::as_array)
                                    .is_some_and(|vs| {
                                        !vs.is_empty()
                                            && vs.iter().all(Value::is_string)
                                            && vs.iter().any(|v| v == MODERN_VERSION)
                                    })
                        });
                        if compatible && attempt == 0 {
                            continue;
                        }
                    }
                    return Err(self.error(
                        "server/discover",
                        Some(id),
                        McpErrorKind::ServerRejected,
                    ));
                }
                Some(RpcFrame::Error { .. }) if self.kind == TransportKind::Stdio => {
                    return self.initialize().await;
                }
                Some(RpcFrame::Error { .. }) => {
                    return Err(self.error(
                        "server/discover",
                        Some(id),
                        McpErrorKind::ServerRejected,
                    ));
                }
                _ => return Err(self.error("server/discover", Some(id), McpErrorKind::Protocol)),
            }
        }
        unreachable!("finite discovery negotiation")
    }
    async fn initialize(&mut self) -> Result<(), McpError> {
        self.era = ProtocolEra::Legacy;
        self.version = ProtocolVersion::V2025_11_25;
        let result = self
            .rpc(
                "initialize",
                json!({"protocolVersion":LEGACY_VERSION,"capabilities":{},"clientInfo":self.info}),
            )
            .await?;
        self.version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .and_then(|v| legacy_version(v, self.kind))
            .ok_or_else(|| {
                self.error("initialize", Some(self.last_id()), McpErrorKind::Protocol)
            })?;
        if !result
            .get("capabilities")
            .and_then(Value::as_object)
            .is_some_and(|c| c.get("tools").is_some_and(Value::is_object))
            || !result
                .pointer("/serverInfo/name")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            || !result
                .pointer("/serverInfo/version")
                .is_some_and(Value::is_string)
        {
            return Err(self.error("initialize", Some(self.last_id()), McpErrorKind::Protocol));
        }
        let notify = self.transport.notify(RpcNotification {
            jsonrpc: "2.0".into(),
            method: "notifications/initialized".into(),
            params: None,
        });
        tokio::select! {biased;
            _=self.cancel.cancelled()=>{
                let _=self.transport.shutdown(ShutdownReason::Cancelled).await;
                Err(self.error("notifications/initialized",None,McpErrorKind::Cancelled))
            }
            result=tokio::time::timeout(self.timeout,notify)=>result.map_err(|_|self.error("notifications/initialized",None,McpErrorKind::Timeout))?,
        }
    }
    fn correlated<'a>(
        &self,
        response: &'a McpResponseEnvelope,
        id: i64,
    ) -> Result<Option<&'a RpcFrame>, McpError> {
        let mut found = None;
        for frame in response.frames() {
            if matches_response(frame, &RpcId::Number(id))
                || matches!(frame, RpcFrame::Error { id: None, .. })
            {
                if found.is_some() {
                    return Err(self.error("server/discover", Some(id), McpErrorKind::Protocol));
                }
                found = Some(frame);
            }
        }
        Ok(found)
    }
    async fn exchange(
        &mut self,
        method: &'static str,
        mut params: Value,
    ) -> Result<(i64, McpResponseEnvelope), McpError> {
        if self.cancel.is_cancelled() {
            return Err(self.error(method, None, McpErrorKind::Cancelled));
        }
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| self.error(method, None, McpErrorKind::Protocol))?;
        if self.era == ProtocolEra::Modern {
            params["_meta"] = json!({VERSION_META:version_string(self.version),CAPABILITIES_META:{},INFO_META:self.info});
        }
        let request = RpcRequest {
            jsonrpc: "2.0".into(),
            id: RpcId::Number(id),
            method: method.into(),
            params,
        };
        if serde_json::to_vec(&request).map_or(true, |b| b.len() > super::types::MAX_REQUEST_BYTES)
        {
            return Err(self.error(method, Some(id), McpErrorKind::LimitExceeded));
        }
        let deadline = tokio::time::timeout(
            self.timeout,
            self.transport.exchange(request, self.cancel.clone()),
        );
        tokio::pin!(deadline);
        let response = tokio::select! {biased;
            _=self.cancel.cancelled()=>{
                let cancelling=self.transport.cancel(CancelRequest {id:RpcId::Number(id),era:self.era,transport:self.kind,phase:if method=="initialize" {RequestPhase::Initialize} else {RequestPhase::Request},issued:true,in_flight:true});
                tokio::pin!(cancelling);
                if self.kind==TransportKind::Http {
                    // Start the side channel before dropping the active registration,
                    // but keep polling the cancelled exchange so its stream closes
                    // even when a legacy peer waits for EOF before acknowledging it.
                    tokio::select! {biased;
                        _=&mut cancelling=>{},
                        _=&mut deadline=>{let _=cancelling.await;},
                    }
                } else {let _=cancelling.await;}
                return Err(self.error(method,Some(id),McpErrorKind::Cancelled));
            }
            response=&mut deadline=>match response {
                Ok(Ok(response))=>response,
                Ok(Err(error))=>return Err(self.error(method,Some(id),if method=="tools/call"&&matches!(error.kind,McpErrorKind::Transport|McpErrorKind::Timeout) {McpErrorKind::OutcomeUnknown} else {error.kind})),
                Err(_)=>return Err(self.error(method,Some(id),if method=="tools/call" {McpErrorKind::OutcomeUnknown} else {McpErrorKind::Timeout})),
            },
        };
        self.last_received_bytes = response.received_bytes;
        for frame in response.frames() {
            if let RpcFrame::Request(request) = frame {
                if self.era == ProtocolEra::Modern {
                    return Err(self.error(method, Some(id), McpErrorKind::Protocol));
                }
                self.transport
                    .respond(unsupported_request(request.id.clone()))
                    .await?;
            }
        }
        Ok((id, response))
    }
    pub async fn rpc(&mut self, method: &'static str, params: Value) -> Result<Value, McpError> {
        for attempt in 0..2 {
            let (id, response) = self.exchange(method, params.clone()).await?;
            let frame = self.correlated(&response, id)?;
            // A version rejection explicitly says the call was not executed. Only
            // this definite rejection is retryable; unknown outcomes never are.
            if self.era == ProtocolEra::Modern
                && response
                    .status
                    .is_none_or(|s| s == 400 || (200..300).contains(&s))
                && let Some(RpcFrame::Error { error, .. }) = frame
                && error.code == -32022
            {
                let compatible = error.data.as_ref().is_some_and(|d| {
                    d.get("requested").is_some_and(Value::is_string)
                        && d.get("supported")
                            .and_then(Value::as_array)
                            .is_some_and(|vs| {
                                !vs.is_empty()
                                    && vs.iter().all(Value::is_string)
                                    && vs.iter().any(|v| v == MODERN_VERSION)
                            })
                });
                if compatible && attempt == 0 {
                    continue;
                }
                return Err(self.error(method, Some(id), McpErrorKind::ServerRejected));
            }
            if let Some(status) = response.status.filter(|s| !(200..300).contains(s)) {
                return Err(self.error(method, Some(id), McpErrorKind::HttpStatus(status)));
            }
            return match frame {
                Some(RpcFrame::Result { result, .. }) => {
                    self.validate_result(method, result)?;
                    Ok(result.clone())
                }
                Some(RpcFrame::Error { .. }) => {
                    Err(self.error(method, Some(id), McpErrorKind::ServerRejected))
                }
                _ => Err(self.error(method, Some(id), McpErrorKind::Protocol)),
            };
        }
        unreachable!("finite per-request negotiation")
    }
    fn validate_result(&self, method: &'static str, result: &Value) -> Result<(), McpError> {
        let fail = || self.error(method, Some(self.last_id()), McpErrorKind::Protocol);
        let object = result.as_object().ok_or_else(fail)?;
        match object.get("resultType").and_then(Value::as_str) {
            Some("input_required") => {
                return Err(self.error(method, Some(self.last_id()), McpErrorKind::InputRequired));
            }
            Some("complete") => {}
            None if !object.contains_key("resultType") && self.era == ProtocolEra::Legacy => {}
            _ => return Err(fail()),
        }
        let cache_required =
            self.era == ProtocolEra::Modern && matches!(method, "server/discover" | "tools/list");
        let cache_present =
            cache_required || object.contains_key("ttlMs") || object.contains_key("cacheScope");
        let cache_valid = object
            .get("ttlMs")
            .and_then(Value::as_f64)
            .is_some_and(|v| v.is_finite() && v >= 0.0)
            && matches!(
                object.get("cacheScope").and_then(Value::as_str),
                Some("private" | "public")
            );
        if cache_present && !cache_valid {
            return Err(fail());
        }
        Ok(())
    }
    pub async fn shutdown(&self, reason: ShutdownReason) -> Result<(), McpError> {
        self.transport.shutdown(reason).await
    }
}
