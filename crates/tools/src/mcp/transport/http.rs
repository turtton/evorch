//! Streamable HTTP only: per-request POST, no GET/replay/redirect/session in modern era.
use super::api::*;
use crate::{
    NetworkGuard,
    mcp::{
        McpError, McpErrorKind, McpToolDefinition,
        codec::{SseDecoder, matches_response, parse_frame},
        headers::{self, ParameterHeader},
        protocol::{legacy_version, unsupported_request, version_string},
        types::{ApprovalPermit, MAX_FRAMES_PER_EXCHANGE, ResolvedMcpServer, ResolvedMcpTransport},
    },
};
use async_trait::async_trait;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use sandbox::Secret;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
struct State {
    era: ProtocolEra,
    version: Option<ProtocolVersion>,
    session: Option<HeaderValue>,
    mappings: HashMap<String, Vec<ParameterHeader>>,
    active: HashMap<RpcId, CancelContext>,
}
pub struct HttpTransport {
    guard: Arc<NetworkGuard>,
    label: String,
    url: Secret,
    headers: HeaderMap,
    destination: crate::network_guard::mcp::ApprovedMcpDestination,
    permit: ApprovalPermit,
    state: Arc<Mutex<State>>,
    closed: CancelContext,
    owner_cancel: CancelContext,
    timeout: Duration,
}
struct Active {
    state: Arc<Mutex<State>>,
    id: RpcId,
}
impl Drop for Active {
    fn drop(&mut self) {
        self.state
            .lock()
            .expect("HTTP state")
            .active
            .remove(&self.id);
    }
}
impl HttpTransport {
    /// Requires an approval-bound destination. Construction sends no request.
    pub fn connect(
        guard: Arc<NetworkGuard>,
        server: ResolvedMcpServer,
        permit: ApprovalPermit,
        cancel: CancelContext,
    ) -> Result<Arc<Self>, McpError> {
        let label = server.scope.server_id.clone();
        let error = |kind| McpError {
            server: label.clone(),
            method: "server/discover",
            request_id: None,
            kind,
        };
        if permit.is_revoked() {
            return Err(error(McpErrorKind::ApprovalRevoked));
        }
        if permit.scope() != &server.scope || permit.fingerprint() != server.fingerprint {
            return Err(error(McpErrorKind::NeedsApproval));
        }
        let ResolvedMcpTransport::Http {
            url,
            headers,
            token,
            destination,
        } = server.transport
        else {
            return Err(error(McpErrorKind::Configuration));
        };
        let mut extra = HeaderMap::new();
        for (name, value) in headers {
            let key = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| error(McpErrorKind::Configuration))?;
            if managed_header(key.as_str()) || extra.contains_key(&key) {
                return Err(error(McpErrorKind::Configuration));
            }
            let mut value = HeaderValue::from_str(value.expose())
                .map_err(|_| error(McpErrorKind::Configuration))?;
            value.set_sensitive(true);
            extra.insert(key, value);
        }
        if let Some(token) = token {
            if extra.contains_key("authorization") {
                return Err(error(McpErrorKind::Configuration));
            }
            let mut value = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
                .map_err(|_| error(McpErrorKind::Configuration))?;
            value.set_sensitive(true);
            extra.insert("authorization", value);
        }
        Ok(Arc::new(Self {
            guard,
            label,
            url,
            headers: extra,
            destination,
            permit,
            state: Arc::new(Mutex::new(State {
                era: ProtocolEra::Modern,
                version: None,
                session: None,
                mappings: HashMap::new(),
                active: HashMap::new(),
            })),
            closed: CancelContext::default(),
            owner_cancel: cancel,
            timeout: server.request_timeout,
        }))
    }
    fn error(&self, method: &'static str, id: Option<&RpcId>, kind: McpErrorKind) -> McpError {
        McpError {
            server: self.label.clone(),
            method,
            request_id: match id {
                Some(RpcId::Number(id)) => Some(*id),
                _ => None,
            },
            kind,
        }
    }
    pub fn register_tools(&self, tools: &[McpToolDefinition]) -> Result<(), McpErrorKind> {
        let mut mappings = HashMap::new();
        for tool in tools {
            mappings.insert(tool.name.clone(), headers::parameter_headers(tool)?);
        }
        self.state.lock().expect("HTTP state").mappings = mappings;
        Ok(())
    }
    fn outgoing_headers(&self, body: &Value) -> Result<HeaderMap, McpErrorKind> {
        let state = self.state.lock().expect("HTTP state");
        let mapping = body
            .pointer("/params/name")
            .and_then(Value::as_str)
            .and_then(|n| state.mappings.get(n))
            .map_or(&[][..], Vec::as_slice);
        let mut out = self.headers.clone();
        out.extend(headers::request_headers(body, mapping)?);
        out.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        if state.era == ProtocolEra::Legacy {
            if let Some(version) = state.version.filter(|v| {
                matches!(
                    v,
                    ProtocolVersion::V2025_11_25 | ProtocolVersion::V2025_06_18
                )
            }) {
                out.insert(
                    "mcp-protocol-version",
                    HeaderValue::from_static(version_string(version)),
                );
            }
            if let Some(session) = &state.session {
                out.insert("mcp-session-id", session.clone());
            }
        }
        Ok(out)
    }
    async fn post(
        &self,
        body: &Value,
        id: Option<&RpcId>,
    ) -> Result<McpResponseEnvelope, McpErrorKind> {
        let headers = self.outgoing_headers(body)?;
        let mut stream = self
            .guard
            .post_mcp_stream(
                self.url.expose(),
                &self.destination,
                &self.permit,
                headers,
                body,
            )
            .await?;
        let status = stream.status().as_u16();
        let content_type = stream
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let response_headers = McpResponseHeaders {
            protocol_version: stream
                .headers()
                .get("mcp-protocol-version")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            session_id: stream
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| Secret::from(s.to_owned())),
            content_type: content_type.clone(),
        };
        if id.is_none() && status == 202 {
            return McpResponseEnvelope::new(Some(status), response_headers, vec![], 0);
        }
        let sse = content_type.as_deref().is_some_and(|s| {
            s.split(';')
                .next()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("text/event-stream"))
        });
        let json = content_type.as_deref().is_some_and(|s| {
            s.split(';')
                .next()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
        });
        if (200..300).contains(&status) && !sse && !json {
            return Err(McpErrorKind::Protocol);
        }
        let mut frames = Vec::new();
        let mut raw = Vec::new();
        let mut decoder = SseDecoder::default();
        while let Some(chunk) = stream.chunk().await? {
            if sse {
                for frame in decoder.push(&chunk)? {
                    if let RpcFrame::Request(request) = &frame {
                        let legacy =
                            self.state.lock().expect("HTTP state").era == ProtocolEra::Legacy;
                        if !legacy {
                            return Err(McpErrorKind::Protocol);
                        }
                        // Capabilities are empty; answer on another POST before waiting
                        // for the final response (never hold the state mutex across I/O).
                        let body = serde_json::to_value(unsupported_request(request.id.clone()))
                            .map_err(|_| McpErrorKind::Protocol)?;
                        self.send_one_way(&body).await?;
                        continue;
                    }
                    let complete = id.is_some_and(|id| matches_response(&frame, id))
                        || matches!(&frame, RpcFrame::Error { id: None, .. });
                    frames.push(frame);
                    if frames.len() > MAX_FRAMES_PER_EXCHANGE {
                        return Err(McpErrorKind::LimitExceeded);
                    }
                    if complete {
                        return McpResponseEnvelope::new(
                            Some(status),
                            response_headers,
                            frames,
                            decoder.received_bytes(),
                        );
                    }
                }
            } else {
                raw.extend_from_slice(&chunk);
            }
        }
        if !sse {
            match parse_frame(&raw) {
                Ok(frame) => frames.push(frame),
                Err(_) if !(200..300).contains(&status) => {}
                Err(error) => return Err(error),
            }
        }
        McpResponseEnvelope::new(
            Some(status),
            response_headers,
            frames,
            if sse {
                decoder.received_bytes()
            } else {
                raw.len()
            },
        )
    }
    async fn send_one_way(&self, body: &Value) -> Result<(), McpErrorKind> {
        let headers = self.outgoing_headers(body)?;
        let stream = self
            .guard
            .post_mcp_stream(
                self.url.expose(),
                &self.destination,
                &self.permit,
                headers,
                body,
            )
            .await?;
        if stream.status().as_u16() != 202 {
            return Err(McpErrorKind::HttpStatus(stream.status().as_u16()));
        }
        Ok(())
    }
    fn remember(&self, method: &str, response: &McpResponseEnvelope) -> Result<(), McpErrorKind> {
        let mut state = self.state.lock().expect("HTTP state");
        if method == "initialize" && response.status.is_some_and(|s| (200..300).contains(&s)) {
            if let Some(session) = &response.headers.session_id {
                if session.expose().is_empty()
                    || !session.expose().bytes().all(|b| (0x21..=0x7e).contains(&b))
                {
                    return Err(McpErrorKind::Protocol);
                }
                let mut value =
                    HeaderValue::from_str(session.expose()).map_err(|_| McpErrorKind::Protocol)?;
                value.set_sensitive(true);
                state.session = Some(value);
            }
            for frame in response.frames() {
                if let RpcFrame::Result { result, .. } = frame {
                    state.version = result
                        .get("protocolVersion")
                        .and_then(Value::as_str)
                        .and_then(|v| legacy_version(v, TransportKind::Http));
                }
            }
        }
        if method == "tools/list" && state.era == ProtocolEra::Modern {
            for frame in response.frames() {
                if let RpcFrame::Result { result, .. } = frame
                    && let Some(tools) = result.get("tools").and_then(Value::as_array)
                {
                    for value in tools {
                        if let Ok(tool) = serde_json::from_value::<McpToolDefinition>(value.clone())
                            && let Ok(mapping) = headers::parameter_headers(&tool)
                        {
                            state.mappings.insert(tool.name, mapping);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
fn managed_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-length"
            | "content-type"
            | "accept"
            | "accept-encoding"
            | "connection"
            | "transfer-encoding"
            | "mcp-protocol-version"
            | "mcp-method"
            | "mcp-name"
            | "mcp-session-id"
            | "last-event-id"
    ) || name.starts_with("mcp-param-")
}
#[async_trait]
impl McpTransport for HttpTransport {
    async fn exchange(
        &self,
        request: RpcRequest,
        cancel: CancelContext,
    ) -> Result<McpResponseEnvelope, McpError> {
        let method = match request.method.as_str() {
            "server/discover" => "server/discover",
            "initialize" => "initialize",
            "tools/list" => "tools/list",
            "tools/call" => "tools/call",
            _ => "request",
        };
        let id = request.id.clone();
        let local = CancelContext::default();
        {
            let mut state = self.state.lock().expect("HTTP state");
            if method == "initialize" {
                state.era = ProtocolEra::Legacy;
                state.version = None;
                state.session = None;
            }
            state.active.insert(id.clone(), local.clone());
        }
        let _active = Active {
            state: self.state.clone(),
            id: id.clone(),
        };
        let body = serde_json::to_value(&request)
            .map_err(|_| self.error(method, Some(&id), McpErrorKind::Configuration))?;
        let outcome = tokio::select! {biased;
            _=cancel.cancelled()=>Err(McpErrorKind::Cancelled),_=local.cancelled()=>Err(McpErrorKind::Cancelled),_=self.closed.cancelled()=>Err(McpErrorKind::Cancelled),_=self.owner_cancel.cancelled()=>Err(McpErrorKind::Cancelled),
            result=self.post(&body,Some(&id))=>result,
        };
        let response = outcome.map_err(|kind| self.error(method, Some(&id), kind))?;
        self.remember(method, &response)
            .map_err(|kind| self.error(method, Some(&id), kind))?;
        Ok(response)
    }
    async fn notify(&self, notification: RpcNotification) -> Result<(), McpError> {
        let body = serde_json::to_value(notification)
            .map_err(|_| self.error("notification", None, McpErrorKind::Configuration))?;
        tokio::time::timeout(self.timeout, self.send_one_way(&body))
            .await
            .map_err(|_| self.error("notification", None, McpErrorKind::Timeout))?
            .map_err(|kind| self.error("notification", None, kind))
    }
    async fn respond(&self, response: RpcFrame) -> Result<(), McpError> {
        if self.state.lock().expect("HTTP state").era != ProtocolEra::Legacy {
            return Err(self.error("response", None, McpErrorKind::Protocol));
        }
        let body = serde_json::to_value(response)
            .map_err(|_| self.error("response", None, McpErrorKind::Protocol))?;
        self.send_one_way(&body)
            .await
            .map_err(|kind| self.error("response", None, kind))
    }
    async fn cancel(&self, request: CancelRequest) -> Result<(), McpError> {
        let active = self
            .state
            .lock()
            .expect("HTTP state")
            .active
            .get(&request.id)
            .cloned();
        let Some(active) = active else {
            return Ok(());
        };
        active.cancel();
        if cancellation_action(&request) == CancellationAction::NotifyBestEffort {
            self.notify(RpcNotification {
                jsonrpc: "2.0".into(),
                method: "notifications/cancelled".into(),
                params: Some(json!({"requestId":request.id})),
            })
            .await?;
        }
        Ok(())
    }
    async fn shutdown(&self, _reason: ShutdownReason) -> Result<(), McpError> {
        self.closed.cancel();
        Ok(())
    }
}
pub struct HttpTransportFactory {
    guard: Arc<NetworkGuard>,
}
impl HttpTransportFactory {
    pub fn new(guard: Arc<NetworkGuard>) -> Self {
        Self { guard }
    }
}
#[async_trait]
impl McpTransportFactory for HttpTransportFactory {
    async fn connect(
        &self,
        server: ResolvedMcpServer,
        permit: ApprovalPermit,
        _limiter: Arc<dyn McpProcessLimiter>,
        cancel: CancelContext,
    ) -> Result<Arc<dyn McpTransport>, McpError> {
        Ok(HttpTransport::connect(
            self.guard.clone(),
            server,
            permit,
            cancel,
        )?)
    }
}
