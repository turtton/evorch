use super::*;

/// Schema-validated call bound to its immutable executor and exact arguments.
pub struct ValidatedToolCall {
    executor: Arc<ToolExecutor>,
    ctx: ToolExecutionContext,
    name: String,
    id: String,
    args: serde_json::Value,
    action: Action,
}

/// Single-use call whose pre-execution approval has been resolved.
pub struct PreparedToolCall(ValidatedToolCall);

impl ToolExecutor {
    /// Validate a runtime-owned tool schema without executing its dispatcher.
    ///
    /// # Errors
    /// Returns InvalidSchema or InvalidArgs when validation fails.
    pub fn validate_schema(
        name: &str,
        schema: &serde_json::Value,
        args: &serde_json::Value,
    ) -> Result<(), ToolError> {
        schema::validate_args(&schema::compile(name, schema)?, args)
    }

    /// Publish a preflight validation failure at its original execution position.
    /// This preserves legacy Started/Completed event pairs without running a tool.
    pub fn report_invalid_call(
        &self,
        ctx: &ToolExecutionContext,
        name: &str,
        id: &str,
        args: serde_json::Value,
        error: &ToolError,
    ) {
        if !self.tools.contains_key(name) {
            return;
        }
        self.event_bus.emit(Event::new(ToolEvent::ToolStarted {
            tool_name: name.to_owned(),
            call_id: id.to_owned(),
            input: Some(args),
            run_id: Some(ctx.run_id.clone()),
        }));
        self.emit_completed(ctx, name, id, Err(error));
    }

    /// Validate without executing or requesting approval. No ToolStarted is emitted.
    ///
    /// # Errors
    /// Returns UnknownTool or InvalidArgs for invalid calls.
    pub fn validate_call(
        self: &Arc<Self>,
        ctx: ToolExecutionContext,
        name: String,
        id: String,
        args: serde_json::Value,
    ) -> Result<ValidatedToolCall, ToolError> {
        let registered = self
            .tools
            .get(name.as_str())
            .ok_or_else(|| ToolError::UnknownTool { name: name.clone() })?;
        schema::validate_args(&registered.validator, &args)?;
        let action = resolve(
            self.policy
                .classify(&name, &capabilities_of(&registered.tool.permissions())),
            self.policy.mode(),
        );
        Ok(ValidatedToolCall {
            executor: Arc::clone(self),
            ctx,
            name,
            id,
            args,
            action,
        })
    }
}

impl ValidatedToolCall {
    /// Resolve only pre-execution gates. AskOnFailure deliberately remains post-hoc.
    /// Approval IDs are run-scoped; cancelling this future never executes the tool.
    ///
    /// # Errors
    /// Returns ExecutionDenied on denial, timeout, or a missing approval gate.
    pub async fn authorize(mut self) -> Result<PreparedToolCall, ToolError> {
        let reason = match self.action {
            Action::Proceed | Action::AskOnFailure => None,
            Action::Deny => Some("policy により拒否されました"),
            Action::AskFirst => match &self.executor.gate {
                None => Some("承認ゲートが未設定のため拒否されました"),
                Some(gate) => match gate
                    .request(&self.name, &format!("{}:{}", self.ctx.run_id, self.id))
                    .await
                {
                    ApprovalOutcome::Approved => {
                        self.action = Action::Proceed;
                        None
                    }
                    ApprovalOutcome::Denied => Some("承認要求が拒否されました"),
                    ApprovalOutcome::TimedOut => Some("承認応答がタイムアウトしました"),
                },
            },
        };
        if let Some(reason) = reason {
            return self.executor.deny(&self.ctx, &self.name, &self.id, reason);
        }
        Ok(PreparedToolCall(self))
    }
}

impl PreparedToolCall {
    /// Execute once, preserving executor events, sanitation and AskOnFailure retries.
    ///
    /// # Errors
    /// Returns the underlying tool execution error.
    pub async fn execute(self) -> Result<ToolResult, ToolError> {
        let call = self.0;
        call.executor
            .execute_inner(
                &call.ctx,
                &call.name,
                &call.id,
                call.args,
                Some(call.action),
            )
            .await
    }
}
