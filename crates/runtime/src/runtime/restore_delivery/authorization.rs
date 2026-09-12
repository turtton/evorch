use super::*;

impl AgentRuntime {
    pub(super) fn authorize_restore(
        &self,
        runs: &HashMap<RunId, RunEntry>,
        sender: RunId,
        recipient: (RunId, Option<RunId>),
        message: &AgentMessage,
    ) -> Result<(), RuntimeError> {
        let sender_entry = runs.get(&sender).ok_or_else(|| unknown_run(sender))?;
        let (recipient, parent) = recipient;
        let denied = |detail: &str| RuntimeError::MessageDenied {
            sender,
            recipient,
            detail: detail.into(),
        };
        if sender == recipient {
            return Err(denied("自己宛てのメッセージは許可されていません"));
        }
        if message.kind == AgentMessageKind::Steering && parent != Some(sender) {
            return Err(denied("steering は親から子へのみ許可されています"));
        }
        if sender_entry.parent != Some(recipient) && parent != Some(sender) {
            return Err(denied(
                "親子関係のない run 間のメッセージは許可されていません",
            ));
        }
        if message.kind == AgentMessageKind::Reply {
            let id = message
                .reply_to
                .as_ref()
                .ok_or_else(|| denied("Reply には reply_to が必要です"))?;
            let sent = self
                .shared
                .sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !sent
                .get(id)
                .is_some_and(|record| record.recipient == sender && record.sender == recipient)
            {
                return Err(RuntimeError::UnknownMessage {
                    message_id: id.clone(),
                });
            }
        }
        Ok(())
    }
}
