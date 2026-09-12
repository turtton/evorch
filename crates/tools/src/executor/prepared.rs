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
        if let ToolError::ExecutionDenied { reason, .. } = error {
            self.event_bus.emit(Event::new(ToolEvent::ExecutionDenied {
                tool_name: name.to_owned(),
                call_id: id.to_owned(),
                reason: reason.clone(),
            }));
        }
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
                    .request(&self.name, &approval_id(&self.ctx, &self.id))
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
            return Err(ToolError::ExecutionDenied {
                tool_name: self.name,
                reason: reason.into(),
            });
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

#[cfg(test)]
mod rework_tests {
    use super::*;
    use event_bus::EventKind;
    use sandbox::{ApprovalMode, PolicyDecision};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct Fails(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl Tool for Fails {
        fn name(&self) -> &'static str {
            "read"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn permissions(&self) -> Permissions {
            Permissions::read_only()
        }
        async fn execute(&self, _: serde_json::Value) -> Result<ToolResult, ToolError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::error("retryable"))
        }
    }

    #[tokio::test]
    async fn denial_preflight_is_silent_for_deny_and_timeout() {
        // Given: either unconditional denial or an unanswered AskFirst gate.
        for decision in [PolicyDecision::Deny, PolicyDecision::Ask] {
            let bus = Arc::new(EventBus::new(32));
            let mut events = bus.subscribe();
            let mut executor = ToolExecutor::new(bus.clone());
            executor
                .register(Arc::new(Fails(Arc::new(AtomicUsize::new(0)))))
                .expect("register");
            executor.set_policy(
                ApprovalPolicy::standard(ApprovalMode::OnRequest).with_override("read", decision),
            );
            executor.set_approval_gate(ApprovalGate::new(bus, Duration::from_millis(10)));
            // When: preflight resolves to a denial.
            let result = Arc::new(executor)
                .validate_call(
                    ToolExecutionContext {
                        run_id: "run".into(),
                    },
                    "read".into(),
                    "id".into(),
                    serde_json::json!({}),
                )
                .expect("validate")
                .authorize()
                .await;
            assert!(result.is_err());
            // Then: only approval requests, never execution lifecycle events, are observable.
            while let Ok(Ok(event)) =
                tokio::time::timeout(Duration::from_millis(10), events.recv()).await
            {
                assert!(matches!(
                    event.kind,
                    EventKind::Tool(ToolEvent::ApprovalRequested { .. })
                ));
            }
        }
    }

    #[tokio::test]
    async fn duplicate_ids_in_same_and_different_runs_have_independent_failure_approvals() {
        // Given: three failed calls sharing a model call ID, two also sharing a run.
        let bus = Arc::new(EventBus::new(64));
        let mut events = bus.subscribe();
        let count = Arc::new(AtomicUsize::new(0));
        let mut executor = ToolExecutor::new(bus.clone());
        executor
            .register(Arc::new(Fails(count.clone())))
            .expect("register");
        executor.set_policy(
            ApprovalPolicy::standard(ApprovalMode::OnFailure)
                .with_override("read", PolicyDecision::Ask),
        );
        executor.set_approval_gate(ApprovalGate::new(bus.clone(), Duration::from_secs(2)));
        let executor = Arc::new(executor);
        let mut tasks = tokio::task::JoinSet::new();
        for run in ["run-a", "run-a", "run-b"] {
            let prepared = executor
                .validate_call(
                    ToolExecutionContext { run_id: run.into() },
                    "read".into(),
                    "duplicate".into(),
                    serde_json::json!({}),
                )
                .expect("validate")
                .authorize()
                .await
                .expect("prepare");
            tasks.spawn(prepared.execute());
        }
        // When: approve one request and deny the remaining requests.
        let mut ids = Vec::new();
        while ids.len() < 3 {
            if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) =
                events.recv().await.expect("event").kind
            {
                ids.push(call_id);
            }
        }
        for (index, call_id) in ids.iter().enumerate() {
            bus.emit(Event::new(ToolEvent::ApprovalResolved {
                call_id: call_id.clone(),
                approved: index == 0,
            }));
        }
        while let Some(result) = tasks.join_next().await {
            result.expect("join").expect("tool");
        }
        // Then: exactly one retry, with distinct approval correlation IDs.
        assert_eq!(count.load(Ordering::SeqCst), 4);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3);
    }
}
