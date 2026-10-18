use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use super::{AgentError, CdpTarget, Snapshot, keys, snapshot};

const BLOCKED_LOG: usize = 16;
const ACTIVITY_LOG: usize = 32;
const TYPED_PREVIEW: usize = 40;

/// One entry of a page's recent history, shown to observers such as the GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Action {
        label: String,
        error: Option<String>,
    },
    Blocked(String),
}

/// Ref-addressed operations on one page. Refs come from the latest
/// [`PageAgent::snapshot`]; others are rejected so the model re-reads the page.
pub struct PageAgent {
    target: Arc<dyn CdpTarget>,
    refs: Mutex<BTreeSet<i64>>,
    blocked: Mutex<VecDeque<String>>,
    activity: Mutex<VecDeque<Activity>>,
}

impl PageAgent {
    pub fn new(target: Arc<dyn CdpTarget>) -> Self {
        Self {
            target,
            refs: Mutex::new(BTreeSet::new()),
            blocked: Mutex::default(),
            activity: Mutex::default(),
        }
    }

    pub fn target(&self) -> &Arc<dyn CdpTarget> {
        &self.target
    }

    /// Notes a request the page's policy refused; the log keeps the latest few.
    pub fn record_blocked(&self, entry: String) {
        let mut log = lock(&self.blocked);
        if log.len() == BLOCKED_LOG {
            log.pop_front();
        }
        log.push_back(entry.clone());
        drop(log);
        self.push_activity(Activity::Blocked(entry));
    }

    /// Requests refused since the last call.
    pub fn take_blocked(&self) -> Vec<String> {
        lock(&self.blocked).drain(..).collect()
    }

    /// The latest actions and refused requests, oldest first.
    pub fn activity(&self) -> Vec<Activity> {
        lock(&self.activity).iter().cloned().collect()
    }

    fn push_activity(&self, entry: Activity) {
        let mut log = lock(&self.activity);
        if log.len() == ACTIVITY_LOG {
            log.pop_front();
        }
        log.push_back(entry);
    }

    async fn logged<T>(
        &self,
        label: String,
        action: impl std::future::Future<Output = Result<T, AgentError>>,
    ) -> Result<T, AgentError> {
        let result = action.await;
        self.push_activity(Activity::Action {
            label,
            error: result.as_ref().err().map(ToString::to_string),
        });
        result
    }

    pub async fn snapshot(&self) -> Result<Snapshot, AgentError> {
        self.logged("snapshot".into(), self.read_snapshot()).await
    }

    async fn read_snapshot(&self) -> Result<Snapshot, AgentError> {
        self.target.call("Accessibility.enable", json!({})).await?;
        let tree = self
            .target
            .call("Accessibility.getFullAXTree", json!({}))
            .await?;
        let nodes = tree
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| response("Accessibility.getFullAXTree", "missing nodes"))?;
        let mut snapshot = snapshot::render(nodes);
        let location = self
            .target
            .call(
                "Runtime.evaluate",
                json!({"expression": "location.href", "returnByValue": true}),
            )
            .await?;
        if let Some(url) = location.pointer("/result/value").and_then(Value::as_str) {
            snapshot.text = format!("URL: {url}\n{}", snapshot.text);
        }
        lock(&self.refs).clone_from(&snapshot.refs);
        Ok(snapshot)
    }

    pub async fn navigate(&self, url: &str) -> Result<(), AgentError> {
        lock(&self.refs).clear();
        self.logged(format!("navigate {url}"), self.target.navigate(url))
            .await
    }

    pub async fn click(&self, reference: &str) -> Result<(), AgentError> {
        self.logged(format!("click {reference}"), self.click_ref(reference))
            .await
    }

    async fn click_ref(&self, reference: &str) -> Result<(), AgentError> {
        let node = self.node(reference)?;
        let (x, y) = self.center(reference, node).await?;
        for (kind, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            let mut event = json!({"type": kind, "x": x, "y": y, "buttons": buttons});
            if kind != "mouseMoved" {
                event["button"] = json!("left");
                event["clickCount"] = json!(1);
            }
            self.target.call("Input.dispatchMouseEvent", event).await?;
        }
        self.target.settle().await
    }

    /// Replaces the field's content as a user would, so input listeners see
    /// trusted events. `submit` presses Enter afterwards.
    pub async fn type_text(
        &self,
        reference: &str,
        text: &str,
        submit: bool,
    ) -> Result<(), AgentError> {
        let mut label = format!(
            "type {reference} {:?}",
            text.chars().take(TYPED_PREVIEW).collect::<String>()
        );
        if text.chars().nth(TYPED_PREVIEW).is_some() {
            label.push('…');
        }
        if submit {
            label.push_str(" + Enter");
        }
        self.logged(label, self.type_into(reference, text, submit))
            .await
    }

    async fn type_into(&self, reference: &str, text: &str, submit: bool) -> Result<(), AgentError> {
        let node = self.node(reference)?;
        self.target
            .call("DOM.focus", json!({"backendNodeId": node}))
            .await?;
        let object = self
            .target
            .call("DOM.resolveNode", json!({"backendNodeId": node}))
            .await?;
        let object_id = object
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| response("DOM.resolveNode", "missing objectId"))?;
        self.target
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": "function() { if (typeof this.select === 'function') { this.select(); } else if (this.isContentEditable) { const range = document.createRange(); range.selectNodeContents(this); const selection = getSelection(); selection.removeAllRanges(); selection.addRange(range); } }",
                }),
            )
            .await?;
        if text.is_empty() {
            self.press_keys("Backspace").await?;
        } else {
            self.target
                .call("Input.insertText", json!({"text": text}))
                .await?;
        }
        if submit {
            self.press_keys("Enter").await?;
        }
        Ok(())
    }

    pub async fn press(&self, combo: &str) -> Result<(), AgentError> {
        self.logged(format!("press {combo}"), self.press_keys(combo))
            .await
    }

    async fn press_keys(&self, combo: &str) -> Result<(), AgentError> {
        for event in keys::key_events(combo)? {
            self.target.call("Input.dispatchKeyEvent", event).await?;
        }
        self.target.settle().await
    }

    /// Scrolls the viewport, or the element under `reference`, by most of a
    /// viewport in `direction` (`up`, `down`, `left` or `right`).
    pub async fn scroll(&self, reference: Option<&str>, direction: &str) -> Result<(), AgentError> {
        let label = match reference {
            Some(reference) => format!("scroll {reference} {direction}"),
            None => format!("scroll {direction}"),
        };
        self.logged(label, self.scroll_by(reference, direction))
            .await
    }

    async fn scroll_by(&self, reference: Option<&str>, direction: &str) -> Result<(), AgentError> {
        let metrics = self.target.call("Page.getLayoutMetrics", json!({})).await?;
        let size = |axis: &str| {
            metrics
                .pointer(&format!("/cssLayoutViewport/{axis}"))
                .and_then(Value::as_f64)
                .unwrap_or(800.0)
        };
        let (width, height) = (size("clientWidth"), size("clientHeight"));
        let (x, y) = match reference {
            Some(reference) => {
                let node = self.node(reference)?;
                self.center(reference, node).await?
            }
            None => (width / 2.0, height / 2.0),
        };
        let (dx, dy) = match direction {
            "up" => (0.0, -height * 0.8),
            "down" => (0.0, height * 0.8),
            "left" => (-width * 0.8, 0.0),
            "right" => (width * 0.8, 0.0),
            other => {
                return Err(AgentError::Navigation(format!(
                    "unknown scroll direction {other:?}"
                )));
            }
        };
        self.target
            .call(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy}),
            )
            .await?;
        Ok(())
    }

    /// PNG of the visible viewport.
    pub async fn screenshot(&self) -> Result<Vec<u8>, AgentError> {
        self.logged("screenshot".into(), self.capture()).await
    }

    async fn capture(&self) -> Result<Vec<u8>, AgentError> {
        let shot = self
            .target
            .call("Page.captureScreenshot", json!({"format": "png"}))
            .await?;
        let data = shot
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| response("Page.captureScreenshot", "missing data"))?;
        STANDARD
            .decode(data)
            .map_err(|error| response("Page.captureScreenshot", &error.to_string()))
    }

    fn node(&self, reference: &str) -> Result<i64, AgentError> {
        reference
            .strip_prefix('e')
            .and_then(|id| id.parse().ok())
            .filter(|id| lock(&self.refs).contains(id))
            .ok_or_else(|| AgentError::UnknownRef(reference.to_owned()))
    }

    async fn center(&self, reference: &str, node: i64) -> Result<(f64, f64), AgentError> {
        self.target
            .call("DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": node}))
            .await?;
        let quads = self
            .target
            .call("DOM.getContentQuads", json!({"backendNodeId": node}))
            .await?;
        quads
            .get("quads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|quad| {
                let points: Vec<f64> = quad.as_array()?.iter().filter_map(Value::as_f64).collect();
                (points.len() == 8).then_some(points)
            })
            .find(|points| area(points) > 1.0)
            .map(|points| {
                let x = points.iter().step_by(2).sum::<f64>() / 4.0;
                let y = points.iter().skip(1).step_by(2).sum::<f64>() / 4.0;
                (x, y)
            })
            .ok_or_else(|| AgentError::NotVisible(reference.to_owned()))
    }
}

fn area(points: &[f64]) -> f64 {
    let twice: f64 = (0..4)
        .map(|i| {
            let j = (i + 1) % 4;
            points[2 * i] * points[2 * j + 1] - points[2 * j] * points[2 * i + 1]
        })
        .sum();
    twice.abs() / 2.0
}

fn response(method: &str, detail: &str) -> AgentError {
    AgentError::Response {
        method: method.to_owned(),
        detail: detail.to_owned(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait::async_trait]
    impl CdpTarget for Recorder {
        async fn call(&self, method: &str, params: Value) -> Result<Value, AgentError> {
            lock(&self.calls).push((method.to_owned(), params));
            Ok(match method {
                "Accessibility.getFullAXTree" => json!({"nodes": [{
                    "nodeId": "1", "role": {"value": "button"}, "name": {"value": "Go"},
                    "backendDOMNodeId": 7
                }]}),
                "DOM.getContentQuads" => json!({"quads": [[10, 20, 30, 20, 30, 40, 10, 40]]}),
                "DOM.resolveNode" => json!({"object": {"objectId": "obj-7"}}),
                _ => json!({}),
            })
        }

        async fn navigate(&self, url: &str) -> Result<(), AgentError> {
            lock(&self.calls).push(("navigate".into(), json!(url)));
            Ok(())
        }
    }

    fn methods(recorder: &Recorder) -> Vec<String> {
        lock(&recorder.calls)
            .iter()
            .map(|(method, _)| method.clone())
            .collect()
    }

    #[tokio::test]
    async fn refs_from_the_latest_snapshot_drive_trusted_input() {
        let target = Arc::new(Recorder::default());
        let page = PageAgent::new(target.clone());
        assert!(matches!(
            page.click("e7").await,
            Err(AgentError::UnknownRef(_))
        ));

        page.snapshot().await.unwrap();
        lock(&target.calls).clear();
        page.click("e7").await.unwrap();
        page.type_text("e7", "hello", true).await.unwrap();

        assert_eq!(
            methods(&target),
            [
                "DOM.scrollIntoViewIfNeeded",
                "DOM.getContentQuads",
                "Input.dispatchMouseEvent",
                "Input.dispatchMouseEvent",
                "Input.dispatchMouseEvent",
                "DOM.focus",
                "DOM.resolveNode",
                "Runtime.callFunctionOn",
                "Input.insertText",
                "Input.dispatchKeyEvent",
                "Input.dispatchKeyEvent",
            ]
        );
        let calls = lock(&target.calls).clone();
        assert_eq!(calls[3].1["x"], 20.0);
        assert_eq!(calls[3].1["y"], 30.0);
        assert_eq!(calls[8].1["text"], "hello");

        page.navigate("https://example.test/").await.unwrap();
        assert!(matches!(
            page.click("e7").await,
            Err(AgentError::UnknownRef(_))
        ));
    }

    #[tokio::test]
    async fn activity_keeps_recent_actions_with_outcomes_and_refused_requests() {
        let page = PageAgent::new(Arc::new(Recorder::default()));

        page.navigate("https://example.test/").await.unwrap();
        page.click("e7").await.unwrap_err();
        page.snapshot().await.unwrap();
        page.type_text("e7", &"x".repeat(TYPED_PREVIEW + 1), true)
            .await
            .unwrap();
        page.record_blocked("https://tracker.test/ (refused)".into());

        let activity = page.activity();
        let action = |label: &str| Activity::Action {
            label: label.into(),
            error: None,
        };
        assert_eq!(activity[0], action("navigate https://example.test/"));
        assert!(
            matches!(&activity[1], Activity::Action { label, error: Some(_) } if label == "click e7")
        );
        assert_eq!(activity[2], action("snapshot"));
        assert_eq!(
            activity[3],
            action(&format!("type e7 {:?}… + Enter", "x".repeat(TYPED_PREVIEW)))
        );
        assert_eq!(
            activity[4],
            Activity::Blocked("https://tracker.test/ (refused)".into())
        );
        assert_eq!(activity.len(), 5, "inner key presses are not logged twice");
        assert_eq!(page.take_blocked().len(), 1);
        assert_eq!(
            page.activity().len(),
            5,
            "draining blocked keeps the history"
        );
    }
}
