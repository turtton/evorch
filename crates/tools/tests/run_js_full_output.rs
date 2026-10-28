//! What a `run_js` program gets of a child call's output (ADR 0039): the whole text
//! up to a bound, while the model-facing content, the events and the GUI keep the
//! bounded preview.

use std::sync::Arc;

use event_bus::EventBus;
use sandbox::DirectSandbox;
use serde_json::json;
use tools::output::FULL_CONTENT_BYTES;
use tools::{ToolExecutionContext, ToolExecutor, ToolResult};

async fn run(shell_command: &str, as_child: bool) -> ToolResult {
    let executor = ToolExecutor::with_standard_tools(
        Arc::new(EventBus::new(64)),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    let ctx = ToolExecutionContext {
        run_id: "run-full".into(),
        parent_call_id: as_child.then(|| "parent".to_owned()),
        ..Default::default()
    };
    executor
        .execute(&ctx, "shell", "parent/1", json!({"command": shell_command}))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_child_call_keeps_the_whole_output_next_to_the_bounded_preview() {
    let child = run("seq 1 5000", true).await;
    let model = run("seq 1 5000", false).await;

    for result in [&child, &model] {
        assert!(result.content.contains("[Output artifact: "), "bounded");
        assert!(result.content.len() < 20 * 1024);
    }
    let whole: String = (1..=5000).map(|n| format!("{n}\n")).collect();
    assert_eq!(
        child
            .full_content
            .as_deref()
            .map(tools::FullContent::as_str),
        Some(format!("exit_code: 0\n{whole}").as_str())
    );
    assert_eq!(model.full_content, None, "only a child call keeps it");
}

#[tokio::test]
async fn nul_bytes_survive_in_the_whole_output() {
    let child = run("seq 1 5000 | tr '\\n' '\\0'", true).await;

    let whole = child.full_content.expect("kept");
    let whole = whole.as_str();
    assert_eq!(whole.matches('\0').count(), 5000);
    assert!(whole.starts_with("exit_code: 0\n1\u{0}2\u{0}3\u{0}"));
}

#[tokio::test]
async fn output_beyond_the_bound_is_not_kept_whole() {
    let bound = FULL_CONTENT_BYTES;
    let child = run(
        &format!("head -c {} /dev/zero | tr '\\0' x", bound + 1000),
        true,
    )
    .await;

    assert!(child.content.contains("[Output artifact: "));
    assert_eq!(child.full_content, None);
    let small = run("echo short", true).await;
    assert_eq!(
        small.full_content, None,
        "nothing was cut, so content is whole"
    );
    assert_eq!(small.content, "exit_code: 0\nshort\n");
}
