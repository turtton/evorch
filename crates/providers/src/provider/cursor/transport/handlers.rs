use super::{Proto, Run, history, protocol_error, wire};
use crate::{ContentBlock, ProviderError, StreamEvent};
use serde_json::Value;

const HANDOFF: &str = "Tool call received and handed off to the external client for execution. Do not retry or call it again; end the turn. The result will be provided in the next request.";
impl Run {
    pub(super) async fn exec(&mut self, message: &[u8]) -> Result<(), ProviderError> {
        let id = wire::integer(message, 1)?.unwrap_or(0);
        if !self.seen_execs.insert(id) {
            return Ok(());
        }
        let exec_id = wire::string(message, 15)?;
        if wire::nested(message, 10)?.is_some() {
            let result = Proto::new().message(
                1,
                Proto::new().message(1, history::request_context(&self.request)),
            );
            return self.exec_result(id, &exec_id, 10, result).await;
        }
        if let Some(args) = wire::nested(message, 11)? {
            return self.mcp(id, &exec_id, args).await;
        }
        if wire::nested(message, 36)?.is_some() {
            let mut server = Proto::new()
                .string(1, "evorch")
                .string(2, "evorch")
                .string(7, "connected");
            for tool in &self.request.tools {
                server = server.message(5, history::tool_definition(tool));
            }
            return self
                .exec_result(
                    id,
                    &exec_id,
                    36,
                    Proto::new().message(1, Proto::new().message(1, server)),
                )
                .await;
        }
        for field in [41, 42, 43] {
            if wire::nested(message, field)?.is_some() {
                // Permissions belong to evorch's canonical tool loop.
                return self
                    .exec_result(id, &exec_id, field, Proto::new().integer(1, 0))
                    .await;
            }
        }
        // Native read/write/shell/subagents are never run within a provider.
        // Throw + streamClose is Cursor's documented unsupported-exec response.
        let error = "Native execution is unsupported by evorch. Use the advertised evorch MCP tools; they execute in the external client.";
        self.transport
            .append(
                Proto::new().message(
                    5,
                    Proto::new().message(
                        2,
                        Proto::new()
                            .integer(1, id)
                            .string(2, error)
                            .string(4, "UNSUPPORTED_EXEC"),
                    ),
                ),
            )
            .await?;
        self.transport
            .append(Proto::new().message(5, Proto::new().message(1, Proto::new().integer(1, id))))
            .await
    }
    async fn exec_result(
        &mut self,
        id: u64,
        exec_id: &str,
        field: u32,
        result: Proto,
    ) -> Result<(), ProviderError> {
        self.transport
            .append(
                Proto::new().message(
                    2,
                    Proto::new()
                        .integer(1, id)
                        .string(15, exec_id)
                        .message(field, result),
                ),
            )
            .await
    }
    async fn mcp(&mut self, exec: u64, exec_id: &str, args: &[u8]) -> Result<(), ProviderError> {
        let name = wire::string(args, 5)?;
        let name = if name.is_empty() {
            wire::string(args, 1)?
        } else {
            name
        };
        if !self.request.tools.iter().any(|t| t.name == name) {
            let result = Proto::new().message(5, Proto::new().string(1, &name));
            return self.exec_result(exec, exec_id, 11, result).await;
        }
        if wire::integer(args, 7)? == Some(1) {
            // This is a permission probe, not a call to execute.
            return self
                .exec_result(
                    exec,
                    exec_id,
                    11,
                    Proto::new().message(
                        3,
                        Proto::new().string(1, "Approval is performed by the external client"),
                    ),
                )
                .await;
        }
        let id = wire::string(args, 3)?;
        let id = if id.is_empty() {
            format!("cursor_{exec}")
        } else {
            id
        };
        self.mcp_arguments.record_exec(&id, args)?;
        if self.tool_ids.insert(id.clone()) {
            self.events.push_back(StreamEvent::ToolCallDelta {
                index: self.tool_ids.len() - 1,
                id: Some(id.clone()),
                name: Some(name.clone()),
                // Completion frames may still contribute or correct arguments.
                // Publish the final JSON once at the protocol terminal boundary.
                arguments_delta: String::new(),
            });
            self.content.push(ContentBlock::ToolUse {
                id,
                name,
                input: Value::Object(Default::default()),
            });
        }
        self.exec_result(exec, exec_id, 11, history::mcp_text_result(HANDOFF))
            .await
    }
    pub(super) async fn kv(&mut self, message: &[u8]) -> Result<(), ProviderError> {
        let id = wire::integer(message, 1)?.unwrap_or(0);
        let mut response = Proto::new().integer(1, id);
        if let Some(get) = wire::nested(message, 2)? {
            let key = wire::nested(get, 1)?.unwrap_or_default();
            let result = match self.conversation.blobs.get(key) {
                Some(data) => Proto::new().bytes(1, data),
                None => Proto::new(),
            };
            response = response.message(2, result);
        } else if let Some(set) = wire::nested(message, 3)? {
            let key = wire::nested(set, 1)?.unwrap_or_default();
            let data = wire::nested(set, 2)?.unwrap_or_default();
            self.conversation.set(key.to_vec(), data.to_vec())?;
            response = response.message(3, Proto::new());
        } else {
            return Err(protocol_error("Unsupported Cursor blob request"));
        }
        self.transport
            .append(Proto::new().message(3, response))
            .await
    }
    pub(super) async fn query(&mut self, query: &[u8]) -> Result<(), ProviderError> {
        let id = wire::integer(query, 1)?.unwrap_or(0);
        let field = wire::fields(query)?
            .into_iter()
            .find(|f| f.number >= 2 && f.wire == 2)
            .ok_or_else(|| protocol_error("Invalid Cursor interaction query"))?;
        let reason = Proto::new().string(1, "Use the external client's tools for this operation");
        let result = match field.number {
            2 | 4 | 5 | 6 | 9 => Proto::new().message(2, reason),
            3 => Proto::new().message(1, Proto::new().message(3, reason)),
            7 => Proto::new().message(1, Proto::new().message(2, reason)),
            // VM setup has only a success schema. Fail promptly rather than
            // inventing success or leaving an unanswered keepalive loop.
            _ => return Err(protocol_error("Unsupported Cursor interaction query")),
        };
        self.transport
            .append(
                Proto::new().message(6, Proto::new().integer(1, id).message(field.number, result)),
            )
            .await
    }
}
