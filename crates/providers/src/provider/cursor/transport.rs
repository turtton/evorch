//! HTTP/1 Connect RunSSE + ordered unary BidiAppend; drop cancels all work.
use super::{
    CursorConfig,
    history::{self, Conversation},
    rpc,
    wire::{self, Proto},
};
use crate::http::{UsageEmitter, map_request_error};
use crate::observe::AttemptObserver;
use crate::{
    ChatRequest, ChatResponse, ContentBlock, DeltaStream, FinishReason, Message, ProviderError,
    Role, StreamEvent, Usage,
};
use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use std::{
    collections::{HashSet, VecDeque},
    pin::Pin,
    time::Duration,
};
mod arguments;
mod handlers;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;
struct Transport {
    http: reqwest::Client,
    config: CursorConfig,
    token: String,
    request_id: String,
    sequence: u64,
}
impl Transport {
    async fn append(&mut self, message: Proto) -> Result<(), ProviderError> {
        let body = Proto::new()
            .message(2, Proto::new().string(1, &self.request_id))
            .integer(3, self.sequence)
            .bytes(4, message.0);
        self.sequence += 1;
        let response = rpc(
            &self.http,
            &self.config.base_url,
            &self.token,
            "/aiserver.v1.BidiService/BidiAppend",
            "application/proto",
        )
        .header("x-request-id", &self.request_id)
        .timeout(Duration::from_secs(30))
        .body(body.0)
        .send()
        .await
        .map_err(map_request_error)?;
        if !response.status().is_success() {
            return Err(super::status_error(
                response.status().as_u16(),
                "Cursor BidiAppend failed",
            ));
        }
        Ok(())
    }
}
struct Run {
    transport: Transport,
    bytes: ByteStream,
    buffer: Vec<u8>,
    events: VecDeque<StreamEvent>,
    request: ChatRequest,
    conversation: tokio::sync::OwnedMutexGuard<Conversation>,
    heartbeat: tokio::time::Interval,
    content: Vec<ContentBlock>,
    usage: Usage,
    turn_ended: bool,
    ended: bool,
    completed: bool,
    arguments_finalized: bool,
    seen_execs: HashSet<u64>,
    tool_ids: HashSet<String>,
    mcp_arguments: arguments::McpArguments,
    observer: AttemptObserver,
    emitter: UsageEmitter,
}
pub(super) async fn start(
    http: reqwest::Client,
    config: CursorConfig,
    profile: Option<String>,
    token: String,
    request: ChatRequest,
    mut conversation: tokio::sync::OwnedMutexGuard<Conversation>,
    route: super::models::ModelRoute,
) -> Result<DeltaStream, ProviderError> {
    let body = history::build_run(&request, &mut conversation, &route)?;
    let mut observer = AttemptObserver::new(
        config.event_bus.clone(),
        "cursor",
        profile,
        "cursor-agent",
        &request.model,
        true,
        request.observation.clone(),
    );
    let request_id = uuid::Uuid::new_v4().to_string();
    let run_request = rpc(
        &http,
        &config.base_url,
        &token,
        "/agent.v1.AgentService/RunSSE",
        "application/connect+proto",
    )
    .header("x-request-id", &request_id)
    .header("connect-accept-encoding", "identity")
    .body(wire::frame(&Proto::new().string(1, &request_id).0));
    let emitter = UsageEmitter::new(config.event_bus.clone(), "cursor");
    let mut transport = Transport {
        http,
        config,
        token,
        request_id,
        sequence: 0,
    };
    observer.emit_started();
    // RunSSE waits for the first BidiAppend before producing response headers.
    let response = tokio::try_join!(
        async {
            let response = run_request.send().await.map_err(map_request_error)?;
            if !response.status().is_success() {
                return Err(super::status_error(
                    response.status().as_u16(),
                    "Cursor RunSSE failed",
                ));
            }
            Ok(response)
        },
        transport.append(body)
    );
    let response = match response {
        Ok((response, ())) => response,
        Err(error) => {
            observer.emit_failed(&error);
            return Err(error);
        }
    };
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;
    let run = Run {
        transport,
        bytes: Box::pin(response.bytes_stream()),
        buffer: Vec::new(),
        events: VecDeque::new(),
        request,
        conversation,
        heartbeat,
        content: Vec::new(),
        usage: Usage::default(),
        turn_ended: false,
        ended: false,
        completed: false,
        arguments_finalized: false,
        seen_execs: HashSet::new(),
        tool_ids: HashSet::new(),
        mcp_arguments: arguments::McpArguments::default(),
        observer,
        emitter,
    };
    Ok(Box::pin(futures_util::stream::try_unfold(
        run,
        |mut run| async move {
            let event = match run.next().await {
                Ok(event) => event,
                Err(error) => {
                    run.observer.emit_failed(&error);
                    return Err(error);
                }
            };
            Ok(event.map(|event| (event, run)))
        },
    )))
}
impl Run {
    async fn next(&mut self) -> Result<Option<StreamEvent>, ProviderError> {
        loop {
            if let Some(event) = self.events.pop_front() {
                self.observer.note_delta(&event);
                return Ok(Some(event));
            }
            if self.completed {
                return Ok(None);
            }
            if self.ended {
                if !self.turn_ended {
                    return Err(protocol_error("Cursor stream ended before turnEnded"));
                }
                if !self.arguments_finalized {
                    self.arguments_finalized = true;
                    for (index, block) in self
                        .content
                        .iter_mut()
                        .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                        .enumerate()
                    {
                        if let ContentBlock::ToolUse { id, input, .. } = block {
                            *input = self.mcp_arguments.finish(id)?;
                            self.events.push_back(StreamEvent::ToolCallDelta {
                                index,
                                id: None,
                                name: None,
                                arguments_delta: input.to_string(),
                            });
                        }
                    }
                    if !self.events.is_empty() {
                        continue;
                    }
                }
                self.completed = true;
                let finish_reason = if self.tool_ids.is_empty() {
                    FinishReason::Stop
                } else {
                    FinishReason::ToolUse
                };
                let response = ChatResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: std::mem::take(&mut self.content),
                    },
                    usage: self.usage,
                    finish_reason,
                };
                self.observer
                    .emit_completed(&self.usage, response.finish_reason.clone());
                self.emitter.emit_usage(&self.request.model, &self.usage);
                return Ok(Some(StreamEvent::Completed { response }));
            }
            if self.buffer.len() >= 5 {
                let length =
                    u32::from_be_bytes(self.buffer[1..5].try_into().expect("length")) as usize;
                if length > wire::MAX_FRAME {
                    return Err(protocol_error("Cursor frame exceeds size limit"));
                }
                if self.buffer.len() >= length + 5 {
                    let flags = self.buffer[0];
                    let payload = self.buffer[5..5 + length].to_vec();
                    self.buffer.drain(..5 + length);
                    if flags == 2 {
                        let end: serde_json::Value = serde_json::from_slice(&payload)
                            .map_err(|_| protocol_error("Invalid Cursor Connect end frame"))?;
                        if let Some(error) = end.get("error").filter(|error| !error.is_null()) {
                            return Err(connect_error(error));
                        }
                        if !self.buffer.is_empty() {
                            return Err(protocol_error("Cursor data after Connect end frame"));
                        }
                        self.ended = true;
                    } else if flags == 0 {
                        self.message(&payload).await?;
                    } else {
                        return Err(protocol_error("Unsupported Cursor Connect frame flags"));
                    }
                    continue;
                }
            }
            tokio::select! {
                biased;
                bytes = self.bytes.next() => match bytes {
                    Some(Ok(bytes)) => {
                        if self.buffer.len().saturating_add(bytes.len()) > wire::MAX_FRAME * 2 { return Err(protocol_error("Cursor stream buffer exceeds size limit")); }
                        self.buffer.extend_from_slice(&bytes);
                    }
                    Some(Err(error)) => return Err(map_request_error(error)),
                    None => { if !self.buffer.is_empty() { return Err(protocol_error("Truncated Cursor Connect frame")); } self.ended = true; }
                },
                _ = self.heartbeat.tick() => { self.transport.append(Proto::new().message(7, Proto::new())).await?; }
            }
        }
    }
    async fn message(&mut self, message: &[u8]) -> Result<(), ProviderError> {
        for field in wire::fields(message)?.into_iter().filter(|f| f.wire == 2) {
            match field.number {
                1 => self.interaction(field.data)?,
                2 => self.exec(field.data).await?,
                3 => {
                    wire::fields(field.data)?;
                    self.conversation.checkpoint = Proto(field.data.to_vec());
                    if let Some(tokens) = wire::nested(field.data, 5)?
                        && self.usage.input_tokens == 0
                    {
                        self.usage.input_tokens = wire::integer(tokens, 1)?.unwrap_or(0);
                    }
                }
                4 => self.kv(field.data).await?,
                5 => {} // No local executions run, so there is nothing to abort.
                7 => self.query(field.data).await?,
                _ => return Err(protocol_error("Unsupported Cursor server message")),
            }
        }
        Ok(())
    }
    fn interaction(&mut self, update: &[u8]) -> Result<(), ProviderError> {
        for field in wire::fields(update)?.into_iter().filter(|f| f.wire == 2) {
            match field.number {
                1 | 4 => {
                    let text = wire::string(field.data, 1)?;
                    if !text.is_empty() {
                        let reasoning = field.number == 4;
                        match self.content.last_mut() {
                            Some(ContentBlock::Reasoning { text: previous }) if reasoning => {
                                previous.push_str(&text)
                            }
                            Some(ContentBlock::Text { text: previous }) if !reasoning => {
                                previous.push_str(&text)
                            }
                            _ => self.content.push(if reasoning {
                                ContentBlock::Reasoning { text: text.clone() }
                            } else {
                                ContentBlock::Text { text: text.clone() }
                            }),
                        }
                        self.events.push_back(if reasoning {
                            StreamEvent::ReasoningDelta { text }
                        } else {
                            StreamEvent::TextDelta { text }
                        });
                    }
                }
                2 | 3 | 7 => self.mcp_arguments.observe(field.number, field.data)?,
                8 => {
                    self.usage.output_tokens = self
                        .usage
                        .output_tokens
                        .saturating_add(wire::integer(field.data, 1)?.unwrap_or(0));
                }
                14 => {
                    self.turn_ended = true;
                    if let Some(n) = wire::integer(field.data, 1)? {
                        self.usage.input_tokens = n;
                    }
                    if let Some(n) = wire::integer(field.data, 2)? {
                        self.usage.output_tokens = n;
                    }
                    self.usage.cache_read_tokens = wire::integer(field.data, 3)?.unwrap_or(0);
                    self.usage.cache_write_tokens = wire::integer(field.data, 4)?.unwrap_or(0);
                    self.usage.reasoning_tokens = wire::integer(field.data, 5)?;
                }
                _ => {} // UI-only step/tool status, summary, and heartbeat events.
            }
        }
        Ok(())
    }
}
fn protocol_error(detail: &str) -> ProviderError {
    ProviderError::InvalidSse {
        detail: detail.into(),
    }
}
fn connect_error(value: &serde_json::Value) -> ProviderError {
    let code = value
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    // Server error text is untrusted and may echo authentication or prompts.
    let message = "Cursor Connect request failed";
    match code {
        "resource_exhausted" => ProviderError::RateLimited { retry_after: None },
        "unauthenticated" => ProviderError::Http {
            status: 401,
            body: message.into(),
        },
        "permission_denied" => ProviderError::Http {
            status: 403,
            body: message.into(),
        },
        "unavailable" => ProviderError::Http {
            status: 503,
            body: message.into(),
        },
        "deadline_exceeded" => ProviderError::Timeout,
        _ => ProviderError::Request(message.into()),
    }
}
