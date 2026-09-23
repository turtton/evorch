mod support;

use providers::{ChatResponse, ContentBlock, FinishReason};
use runtime::escalation_review::{QuickModelReviewer, ReviewError, ReviewVerdict};
use std::sync::Arc;
use support::{ScriptedModel, text_response};

async fn assert_invalid(response: ChatResponse) {
    // Given: a provider response outside the verdict contract.
    let reviewer = QuickModelReviewer::new(Arc::new(ScriptedModel::new([Ok(response)])));
    // When: reviewing a shell request.
    let result = reviewer.review("strict", "pwd", "inspect").await;
    // Then: the response cannot authorize escalation.
    assert_eq!(result, Err(ReviewError::InvalidVerdict));
}

#[tokio::test]
async fn reviewer_rejects_unknown_field_verdict_as_invalid() {
    assert_invalid(text_response(
        r#"{"approve":true,"extra":true}"#,
        FinishReason::Stop,
    ))
    .await;
}

#[tokio::test]
async fn reviewer_uses_only_final_text_for_approval_and_denial() {
    for (text, expected) in [
        (r#"{"approve":true}"#, ReviewVerdict::Approve),
        (
            r#"{"approve":false,"reason":"unsafe"}"#,
            ReviewVerdict::Deny {
                reason: "unsafe".into(),
            },
        ),
    ] {
        // Given: reasoning disagrees with the final verdict and surrounds it.
        let mut response = support::reasoning_response(
            r#"{"approve":true} or {"approve":false}: intermediate reasoning"#,
            text,
            FinishReason::Stop,
        );
        response.message.content.push(ContentBlock::Reasoning {
            text: "Additional reasoning must not change the decision".into(),
        });
        let reviewer = QuickModelReviewer::new(Arc::new(ScriptedModel::new([Ok(response)])));
        // When / Then: only the final text can approve or deny escalation.
        assert_eq!(
            reviewer.review("strict", "pwd", "inspect").await,
            Ok(expected)
        );
    }
}

#[tokio::test]
async fn reviewer_never_treats_reasoning_as_a_verdict() {
    let mut response = text_response("", FinishReason::Stop);
    response.message.content = vec![ContentBlock::Reasoning {
        text: r#"{"approve":true}"#.into(),
    }];
    assert_invalid(response).await;
    assert_invalid(support::reasoning_response(
        r#"{"approve":true}"#,
        "invalid final answer",
        FinishReason::Stop,
    ))
    .await;
}

#[tokio::test]
async fn reviewer_rejects_multiple_text_blocks_as_invalid() {
    let mut response = text_response(r#"{"approve":"#, FinishReason::Stop);
    response.message.content.push(ContentBlock::Text {
        text: "true}".into(),
    });
    assert_invalid(response).await;
}

#[tokio::test]
async fn reviewer_rejects_empty_content_as_invalid() {
    let mut response = text_response("", FinishReason::Stop);
    response.message.content.clear();
    assert_invalid(response).await;
    for text in ["", " \n\t"] {
        assert_invalid(text_response(text, FinishReason::Stop)).await;
    }
}

#[tokio::test]
async fn reviewer_rejects_abnormal_finish_reason_as_invalid() {
    for reason in [
        FinishReason::Length,
        FinishReason::ToolUse,
        FinishReason::ContentFilter,
        FinishReason::Other("unknown".into()),
    ] {
        assert_invalid(text_response(r#"{"approve":true}"#, reason)).await;
    }
}

#[tokio::test]
async fn reviewer_rejects_text_plus_tool_use_as_invalid() {
    let mut response = text_response(r#"{"approve":true}"#, FinishReason::Stop);
    response.message.content.push(ContentBlock::ToolUse {
        id: "call".into(),
        name: "shell".into(),
        input: serde_json::json!({}),
    });
    assert_invalid(response).await;
}
