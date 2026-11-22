#![cfg(target_os = "linux")]
//! The published restart must not disturb what the provider has already been
//! sent. The product is driven from outside (a desktop client's run, then a
//! management client's restart, then follow-ups on the successor) and the
//! assertions are made on the HTTP the product sent its provider, with the
//! checks of `mock_openai::cache_contract` and nothing the product reports about
//! itself. The provider derives its cache accounting from the input it receives.
//!
//! Same build: no cache boundary exists, so every request must extend the one
//! before it, settings and instructions unchanged, across the restart too. A
//! different build is a separate, already tested boundary
//! (`planned_exec_cache`); a ToolResult in the sent history is covered there
//! (this scenario has none: its turns are text).
#[path = "../shared-test-support/product_exec.rs"]
mod product_exec;
#[path = "../shared-test-support/public_restart_product.rs"]
mod public_restart_product;

use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use product_exec::artifact::ProductArtifact;
use serde_json::Value;

fn messages(request: &Value) -> &Vec<Value> {
    request["messages"].as_array().expect("chat messages")
}

fn check(in_flight: bool) {
    let artifact = ProductArtifact::require();
    let evidence = public_restart_product::drive_with(&artifact, in_flight);
    let requests = &evidence.requests;
    assert_eq!(
        requests.len(),
        3,
        "one provider request per turn: the restart repeats none"
    );
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1])
            .unwrap_or_else(|why| panic!("the history sent before the restart changed: {why}"));
    }
    // The request after the restart continues the captured conversation.
    assert!(
        messages(&requests[1]).len() > messages(&requests[0]).len(),
        "the successor's request extends the conversation instead of replacing it"
    );
    assert!(
        evidence.epoch_after > evidence.epoch_before,
        "the second and third requests were made by the successor"
    );
    // Chat Completions carries no wire affinity key; the same-build restart
    // must not introduce one.
    assert!(
        requests
            .iter()
            .all(|request| request.get("prompt_cache_key").is_none())
    );
    // Instructions, model and tool schemas in the same order, byte for byte.
    for request in &requests[1..] {
        assert_eq!(request["tools"], requests[0]["tools"]);
        assert_eq!(request["model"], requests[0]["model"]);
        assert_eq!(messages(request)[0], messages(&requests[0])[0]);
    }
}

#[test]
fn a_same_build_public_restart_preserves_the_sent_prefix_and_settings() {
    check(false);
}

#[test]
fn a_restart_asked_during_a_turn_preserves_the_sent_prefix_and_settings() {
    check(true);
}

#[test]
#[ignore = "role: the reaper of one supervised family (product_exec::reaper)"]
fn f4p_family_reaper() {
    product_exec::reaper::run();
}
