use workbench_protocol::namespaces::{ApprovalsQuery, ConversationQuery};
use workbench_protocol::{
    handshake::{Capability, WireLimits},
    ids::ApprovalId,
    queries::*,
    wire::*,
};

fn request() -> GetApprovalInput {
    GetApprovalInput {
        approval_id: ApprovalId::new("approval-1").unwrap(),
        core_epoch: InstanceEpoch(7),
        approval_revision: Revision(2),
        offset: ByteOffset(0),
        max_bytes: 4,
    }
}
#[test]
fn approval_chunks_share_body_utf8_and_limit_contracts() {
    let mut q = request();
    let input = "é🙂界x";
    let mut text = String::new();
    loop {
        let chunk = ApprovalInputChunk::slice(&q, input).unwrap();
        chunk.validate(4).unwrap();
        assert_eq!(chunk.offset.get(), text.len() as u64);
        text.push_str(&chunk.text);
        let Some(next) = chunk.next_offset else { break };
        q.offset = next;
    }
    assert_eq!(text, input);
    for offset in [1, 3, u64::MAX] {
        q.offset = ByteOffset(offset);
        assert!(ApprovalInputChunk::slice(&q, input).is_err());
    }
    q.offset = ByteOffset(0);
    for max in [0, 3, 65_537] {
        q.max_bytes = max;
        assert!(
            Query::Approvals(ApprovalsQuery::GetApprovalInput(q.clone()))
                .validate(&WireLimits::default())
                .is_err()
        );
    }
    q.max_bytes = 65_536;
    assert_eq!(
        Query::Approvals(ApprovalsQuery::GetApprovalInput(q.clone())).required_capability(),
        Capability::Approvals
    );
    Query::Approvals(ApprovalsQuery::GetApprovalInput(q.clone()))
        .validate(&WireLimits::default())
        .unwrap();
    let largest = "a".repeat(1024 * 1024);
    let chunk = ApprovalInputChunk::slice(&q, &largest).unwrap();
    assert_eq!(chunk.text.len(), 65_536);
    let mut invalid = chunk.clone();
    invalid.next_offset = Some(ByteOffset(1));
    assert!(invalid.validate(65_536).is_err());
    let mut invalid = chunk;
    invalid.total_bytes = ByteOffset(1024 * 1024 + 1);
    assert!(invalid.validate(65_536).is_err());
    assert!(ApprovalInputChunk::slice(&q, &(largest + "a")).is_err());
}
#[test]
fn omitted_history_direction_is_forward_and_backward_is_explicit() {
    let query: Query = serde_json::from_value(
        serde_json::json!({"type":"conversation","payload":{"type":"read_committed_history","payload":{"thread":"t-1"}}}),
    )
    .unwrap();
    let Query::Conversation(ConversationQuery::ReadCommittedHistory(mut q)) = query else {
        panic!()
    };
    assert_eq!(q.direction, HistoryDirection::Forward);
    q.direction = HistoryDirection::Backward;
    let value = serde_json::to_value(Query::Conversation(
        ConversationQuery::ReadCommittedHistory(q),
    ))
    .unwrap();
    assert_eq!(value["payload"]["payload"]["direction"], "backward");
}
