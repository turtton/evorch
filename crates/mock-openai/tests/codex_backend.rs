//! The Codex subscription backend paths serve the same scripts and prompt cache as
//! `/v1/responses`, including official compaction replies.

use std::sync::Arc;

use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{
    ChatRequest, Compactor, ContentBlock, FinishReason, Message, ProviderAuth, ProviderClient,
    Role, ToolResultContent,
};
use serde_json::json;

/// Unexpired (2100-01-01) ID token carrying the account claim Codex requires.
const ID_TOKEN: &str = "e30.eyJleHAiOjQxMDI0NDQ4MDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2MifX0.signature";
const MODEL: &str = "gpt-5.5";

fn codex_client(server: &StreamingMockOpenAi) -> CodexClient {
    let store = Arc::new(InMemoryTokenStore::new());
    store
        .save(&TokenBundle {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            id_token: ID_TOKEN.into(),
        })
        .unwrap();
    CodexClient::with_config(
        CodexConfig {
            base_url: server.codex_base_url(),
            auth_base_url: server.codex_base_url(),
            ..CodexConfig::default()
        },
        store,
    )
    .unwrap()
}

// Given: the real Codex client pointed at the fixture's Codex backend paths
// When: it lists models, receives a freeform apply_patch call, replays it with its output
//       and then requests official compaction
// Then: the catalog lists the fixture model, the call and the compaction item decode, the
//       custom history is replayed append-only, and the compaction request reports the
//       whole earlier input as cached
#[tokio::test(flavor = "multi_thread")]
async fn codex_backend_serves_catalog_custom_calls_and_compaction_with_prompt_cache() {
    let patch = [
        "*** Begin Patch\n",
        "*** Add File: 日本語.txt\n+a\n",
        "*** End Patch",
    ];
    let server = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::custom_tool_call(
                "resp-1",
                MODEL,
                0,
                "call-1",
                apply_patch::tool_contract::NAME,
                patch,
            ),
            ScriptedResponse::compaction("resp-2", MODEL, "opaque-state"),
        ],
        vec![MODEL.into()],
    );
    let client = codex_client(&server);
    let auth = ProviderAuth::new("");
    assert_eq!(
        client.list_models(&auth).await.unwrap(),
        Some(vec![MODEL.to_owned()])
    );
    let mut request: ChatRequest = serde_json::from_value(json!({
        "model": MODEL, "messages": [
            {"role": "system", "content": [{"type": "text", "text": "stable"}]},
            {"role": "user", "content": [{"type": "text", "text": "patch"}]}
        ], "tools": [{"name": apply_patch::tool_contract::NAME, "description": "patch",
            "input_schema": apply_patch::tool_contract::json_schema()}]
    }))
    .unwrap();

    let first = client.send(&auth, &request).await.unwrap();
    assert_eq!(first.finish_reason, FinishReason::ToolUse);
    assert_eq!(
        first.message.content,
        vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: apply_patch::tool_contract::NAME.into(),
            input: json!({"input": patch.concat()}),
        }]
    );
    assert_eq!(first.usage.cache_read_tokens, 0);
    request.messages.push(first.message);
    request.messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_call_id: "call-1".into(),
            content: vec![ToolResultContent::Text { text: "ok".into() }],
            is_error: false,
        }],
    });
    let compacted = client.compact(&request).await.unwrap();
    assert_eq!(compacted.encrypted_content, "opaque-state");
    assert_eq!(compacted.usage.cache_read_tokens, first.usage.input_tokens);

    let requests = server.recorded_requests();
    assert!(
        requests[0]
            .path
            .starts_with("/backend-api/codex/models?client_version="),
        "{}",
        requests[0].path
    );
    let posts: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "POST")
        .collect();
    assert_eq!(posts.len(), 2);
    assert!(
        posts
            .iter()
            .all(|request| request.path == "/backend-api/codex/responses")
    );
    let mut replayed = posts[1].body.clone();
    let trigger = replayed["input"].as_array_mut().unwrap().pop().unwrap();
    assert_eq!(trigger, json!({"type": "compaction_trigger"}));
    // B-3: ordinary generation always asks for encrypted reasoning, and the official
    // compaction request keeps the same `include` (Codex CLI 0.166.0). Pin it, then compare
    // every setting and the replayed input as one prefix.
    assert_eq!(
        posts[0].body["include"],
        json!(["reasoning.encrypted_content"])
    );
    assert_eq!(replayed["include"], posts[0].body["include"]);
    let input = replayed["input"].as_array().unwrap();
    let call = input.len() - 2;
    assert_eq!(
        input[call..],
        [
            json!({"type": "custom_tool_call", "call_id": "call-1",
                "name": apply_patch::tool_contract::NAME, "input": patch.concat()}),
            json!({"type": "custom_tool_call_output", "call_id": "call-1", "output": "ok"}),
        ]
    );
    assert_append_only(CacheProtocol::Codex, &posts[0].body, &replayed).unwrap();
    assert_eq!(server.remaining_scripts(), 0);
}
