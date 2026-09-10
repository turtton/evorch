use base64::{Engine, engine::general_purpose::STANDARD};
use chromiumoxide::{
    Page, cdp::browser_protocol::page::CaptureScreenshotFormat, page::ScreenshotParams,
};
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventBus};

use super::{BrowserAction, BrowserError};

pub(super) fn emit(bus: &EventBus, code: &str, detail: &str, failed: bool) {
    bus.emit(Event::new(DiagnosticEvent {
        source: "browser".into(),
        severity: if failed {
            DiagnosticSeverity::Error
        } else {
            DiagnosticSeverity::Info
        },
        code: code.into(),
        detail: detail.into(),
        run_id: None,
        thread_id: None,
    }));
}

pub(super) async fn perform(
    page: &Page,
    action: BrowserAction,
    bus: &EventBus,
) -> Result<(), BrowserError> {
    let id = format!("{:?}", std::time::SystemTime::now());
    let action_name = match &action {
        BrowserAction::Navigate(_) => "navigate",
        BrowserAction::Click(_) => "click",
    };
    emit(
        bus,
        "browser.action",
        &serde_json::json!({
            "id": id, "action": action_name, "phase": "started",
        })
        .to_string(),
        false,
    );
    let before = page.content().await?;
    screenshot(page, bus, (&id, "before")).await?;
    let result = match action {
        BrowserAction::Navigate(url) => page.goto(url.as_str()).await.map(|_| ()),
        BrowserAction::Click(selector) => match page.find_element(selector).await {
            Ok(element) => element.click().await.map(|_| ()),
            Err(error) => Err(error),
        },
    };
    emit(
        bus,
        "browser.action",
        &serde_json::json!({
            "id": id, "action": action_name,
            "phase": if result.is_ok() { "completed" } else { "failed" },
            "error": result.as_ref().err().map(ToString::to_string),
        })
        .to_string(),
        result.is_err(),
    );
    screenshot(page, bus, (&id, "after")).await?;
    let after = page.content().await?;
    emit(
        bus,
        "browser.dom_diff",
        &serde_json::json!({
            "id": id, "diff": dom_diff(&before, &after),
        })
        .to_string(),
        false,
    );
    result?;
    Ok(())
}

async fn screenshot(
    page: &Page,
    bus: &EventBus,
    identity: (&str, &str),
) -> Result<(), BrowserError> {
    let bytes = page
        .screenshot(
            ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Jpeg)
                .quality(60)
                .full_page(false)
                .build(),
        )
        .await?;
    emit(
        bus,
        "browser.screenshot",
        &serde_json::json!({
            "id": identity.0, "phase": identity.1, "mime": "image/jpeg",
            "base64": STANDARD.encode(bytes),
        })
        .to_string(),
        false,
    );
    Ok(())
}

#[derive(Debug, serde::Serialize, PartialEq, Eq)]
pub(super) struct DomDiff {
    prefix_chars: usize,
    removed: String,
    inserted: String,
    truncated: bool,
}

pub(super) fn dom_diff(before: &str, after: &str) -> DomDiff {
    let prefix = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let old: Vec<_> = before.chars().skip(prefix).collect();
    let new: Vec<_> = after.chars().skip(prefix).collect();
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let removed = &old[..old.len() - suffix];
    let inserted = &new[..new.len() - suffix];
    const LIMIT: usize = 16_384;
    DomDiff {
        prefix_chars: prefix,
        removed: removed.iter().take(LIMIT).collect(),
        inserted: inserted.iter().take(LIMIT).collect(),
        truncated: removed.len() > LIMIT || inserted.len() > LIMIT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_preserves_unicode_and_common_suffix() {
        let diff = dom_diff("<p>前</p>", "<p>後</p>");
        assert_eq!(
            diff,
            DomDiff {
                prefix_chars: 3,
                removed: "前".into(),
                inserted: "後".into(),
                truncated: false
            }
        );
    }

    #[test]
    fn identical_dom_has_empty_change() {
        let diff = dom_diff("hello", "hello");
        assert!(diff.inserted.is_empty() && diff.removed.is_empty());
    }
}
