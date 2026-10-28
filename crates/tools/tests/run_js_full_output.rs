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
    run_observed(shell_command, as_child).await.0
}

/// [`run`], with the events the call published.
async fn run_observed(shell_command: &str, as_child: bool) -> (ToolResult, Vec<event_bus::Event>) {
    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let executor = ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    let ctx = ToolExecutionContext {
        run_id: "run-full".into(),
        parent_call_id: as_child.then(|| "parent".to_owned()),
        ..Default::default()
    };
    let result = executor
        .execute(&ctx, "shell", "parent/1", json!({"command": shell_command}))
        .await
        .unwrap();
    (result, receiver.drain_pending_snapshot())
}

/// A secret and a control marker the model must never be shown as they are.
fn secret() -> String {
    format!("sk-{}", "a".repeat(48))
}

fn marker() -> String {
    format!("<{}>", "system-reminder")
}

fn escaped_marker() -> String {
    format!("<\\{}>", "system-reminder")
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

/// Runs `test` again with `EVORCH_OUTPUT_DIR` set, as `bounded_output` does: the
/// process environment is shared by the tests of this binary.
fn isolated_output_process(test: &str, directory: &std::path::Path) -> bool {
    const CHILD: &str = "EVORCH_FULL_OUTPUT_TEST";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return false;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .env("EVORCH_OUTPUT_DIR", directory)
        .status()
        .unwrap();
    assert!(status.success(), "isolated output test failed: {test}");
    true
}

#[tokio::test]
async fn a_result_already_cut_is_not_cut_again_into_a_second_artifact_or_a_whole_text() {
    // An output directory with a very long path: the reference after the preview is long
    // enough to push the content over the ceiling a second time.
    let root = tempfile::tempdir().unwrap();
    let mut long = root.path().to_path_buf();
    for index in 0..14 {
        long.push(format!("{index:02}{}", "d".repeat(200)));
    }
    std::fs::create_dir_all(&long).unwrap();
    if isolated_output_process(
        "a_result_already_cut_is_not_cut_again_into_a_second_artifact_or_a_whole_text",
        &long,
    ) {
        return;
    }

    // More than a program gets whole, in one line.
    let size = FULL_CONTENT_BYTES + 5000;
    let child = run(
        &format!(
            "printf '%s\\n%s\\n' '{}' '{}'; head -c {size} /dev/zero | tr '\\0' x",
            secret(),
            marker()
        ),
        true,
    )
    .await;

    let artifact = child.detail.as_ref().unwrap()["output_artifact"]["path"]
        .as_str()
        .expect("the artifact of the output")
        .to_owned();
    assert!(
        artifact.len() > 2500,
        "a long reference: {}",
        artifact.len()
    );
    assert_eq!(child.full_content, None, "the preview is not the output");
    assert!(
        child.content.len() <= 16 * 1024,
        "{} bytes",
        child.content.len()
    );
    assert!(child.content.contains(&artifact), "the reference survives");
    let saved = std::fs::read_to_string(&artifact).unwrap();
    assert!(
        saved.len() > size,
        "the artifact is the output, not the preview"
    );
    // The artifact is redacted; a later `read` of it is escaped like any read.
    assert!(!saved.contains(&secret()));
    assert!(!child.content.contains(&secret()) && !child.content.contains(&marker()));

    let normalized = tools::tools::run_js::normalize::normalize("shell", &json!({}), &child);
    assert!(normalized.truncated);
    assert_eq!(
        normalized.next,
        Some(json!({"tool": "read", "path": artifact}))
    );
    // The head of the preview, with the `exit_code:` line, was trimmed to make room for
    // the long reference; the status is still there, structured.
    assert!(
        !child.content.starts_with("exit_code:"),
        "the header was trimmed"
    );
    assert_eq!(
        normalized.detail,
        json!({"exit_code": 0, "timed_out_ms": null})
    );
    assert_eq!(
        child.detail.as_ref().unwrap()["output_artifact"]["path"],
        json!(artifact),
        "and so is the artifact"
    );
    assert!(normalized.text.len() < size);
}

#[tokio::test]
async fn the_whole_text_a_program_gets_is_redacted_and_escaped_like_the_preview() {
    // The secret and the marker are at the head, which the tail preview does not show.
    let command = format!(
        "printf 'token {} here\\n{}\\n'; seq 1 3000",
        secret(),
        marker()
    );
    let (child, events) = run_observed(&command, true).await;

    let whole = child.full_content.as_ref().expect("kept").as_str();
    assert!(
        whole.contains("token [REDACTED:openai-style-key] here"),
        "redacted"
    );
    assert!(!whole.contains(&secret()));
    assert!(whole.contains(&escaped_marker()), "escaped");
    assert!(!whole.contains(&marker()));
    assert!(whole.ends_with("2999\n3000\n"), "and it is the whole text");

    // None of it, nor the whole text, reaches content, detail or an event.
    let detail = child.detail.as_ref().unwrap().to_string();
    let mut shown = vec![child.content.clone(), detail];
    for event in &events {
        if let event_bus::EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
            output,
            detail,
            ..
        }) = &event.kind
        {
            shown.push(output.clone().unwrap_or_default());
            shown.push(detail.as_ref().map(ToString::to_string).unwrap_or_default());
        }
    }
    assert!(shown.len() >= 4, "the child's completion was observed");
    for text in shown {
        assert!(
            !text.contains(&secret()) && !text.contains(&marker()),
            "{text}"
        );
        assert!(
            !text.contains("token ["),
            "the head is not in what the model sees"
        );
        assert!(
            !text.contains("\n1500\n"),
            "the middle of the output is not either"
        );
    }
    // What the program gets of it through normalize is the same text.
    let normalized = tools::tools::run_js::normalize::normalize("shell", &json!({}), &child);
    assert!(!normalized.text.contains(&secret()));
    assert!(
        normalized
            .text
            .contains("token [REDACTED:openai-style-key] here")
    );
    assert!(!normalized.truncated);
}
