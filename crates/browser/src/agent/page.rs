use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use super::{AgentError, CdpTarget, Snapshot, keys, snapshot};

const BLOCKED_LOG: usize = 16;
const ACTIVITY_LOG: usize = 32;
const TYPED_PREVIEW: usize = 40;
const DIALOG_LOG: usize = 8;
const DIALOG_PREVIEW: usize = 200;
const SELECT_OPTIONS: &str = r#"function(wanted) {
  if (this.localName !== "select") return "is not a <select> element";
  if (this.disabled) return "is disabled";
  if (!this.multiple && wanted.length !== 1) return "takes exactly one value";
  const options = Array.from(this.options);
  const picked = [];
  for (const want of wanted) {
    const option = options.find(o => o.value === want) || options.find(o => o.label.trim() === want.trim());
    if (!option) return `has no option with value or label ${JSON.stringify(want)}; options: ${JSON.stringify(options.slice(0, 50).map(o => o.label.trim()))}`;
    if (option.disabled || option.closest("optgroup")?.disabled) return `option ${JSON.stringify(want)} is disabled`;
    picked.push(option);
  }
  for (const option of options) option.selected = picked.includes(option);
  this.dispatchEvent(new Event("input", {bubbles: true}));
  this.dispatchEvent(new Event("change", {bubbles: true}));
  return null;
}"#;

/// One entry of a page's recent history, shown to observers such as the GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Action {
        label: String,
        error: Option<String>,
    },
    Blocked(String),
    Dialog(String),
}

/// A JavaScript dialog a page opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt,
    BeforeUnload,
}

/// How to close a confirm or prompt dialog. `prompt_text` answers a prompt;
/// without it an accepted prompt keeps its default text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogAnswer {
    pub accept: bool,
    pub prompt_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadTarget {
    pub page_url: String,
    pub frame_url: String,
}

/// Ref-addressed operations on one page. Refs come from the latest
/// [`PageAgent::snapshot`]; others are rejected so the model re-reads the page.
pub struct PageAgent {
    target: Arc<dyn CdpTarget>,
    refs: Mutex<BTreeSet<i64>>,
    blocked: Mutex<VecDeque<String>>,
    activity: Mutex<VecDeque<Activity>>,
    dialogs: Mutex<VecDeque<String>>,
    dialog_answer: Mutex<Option<DialogAnswer>>,
}

impl PageAgent {
    pub fn new(target: Arc<dyn CdpTarget>) -> Self {
        Self {
            target,
            refs: Mutex::new(BTreeSet::new()),
            blocked: Mutex::default(),
            activity: Mutex::default(),
            dialogs: Mutex::default(),
            dialog_answer: Mutex::default(),
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

    /// Answers the first confirm or prompt dialog the next action opens.
    /// Without an answer they are dismissed, so nothing a page guards behind
    /// a confirmation runs unless the agent asked for it.
    pub fn answer_next_dialog(&self, answer: DialogAnswer) {
        self.push_activity(Activity::Action {
            label: format!(
                "answer next dialog: {}",
                if answer.accept { "accept" } else { "dismiss" }
            ),
            error: None,
        });
        *lock(&self.dialog_answer) = Some(answer);
    }

    /// Decides how to close a dialog the page opened, and notes it. Alerts
    /// only have OK, and leaving a page on `beforeunload` is what the agent's
    /// navigation asked for, so both are accepted.
    pub fn on_dialog(
        &self,
        kind: DialogKind,
        message: &str,
        default_prompt: Option<&str>,
    ) -> DialogAnswer {
        let answered = match kind {
            DialogKind::Alert | DialogKind::BeforeUnload => None,
            DialogKind::Confirm | DialogKind::Prompt => lock(&self.dialog_answer).take(),
        };
        let defaulted = answered.is_none();
        let mut answer = answered.unwrap_or(DialogAnswer {
            accept: matches!(kind, DialogKind::Alert | DialogKind::BeforeUnload),
            prompt_text: None,
        });
        answer.prompt_text = match kind {
            DialogKind::Prompt if answer.accept => answer
                .prompt_text
                .or_else(|| Some(default_prompt.unwrap_or_default().to_owned())),
            _ => None,
        };
        let mut preview: String = message.chars().take(DIALOG_PREVIEW).collect();
        if message.chars().nth(DIALOG_PREVIEW).is_some() {
            preview.push('…');
        }
        let outcome = match (kind, &answer.prompt_text) {
            (DialogKind::Alert, _) => "closed".to_owned(),
            (DialogKind::BeforeUnload, _) => "left the page".to_owned(),
            (_, Some(text)) => format!("answered {text:?}"),
            (_, None) if answer.accept => "accepted".to_owned(),
            (_, None) if defaulted => "dismissed; to accept it, call browser_handle_dialog with accept=true and repeat the action".to_owned(),
            (_, None) => "dismissed".to_owned(),
        };
        let name = match kind {
            DialogKind::Alert => "alert",
            DialogKind::Confirm => "confirm",
            DialogKind::Prompt => "prompt",
            DialogKind::BeforeUnload => "beforeunload",
        };
        let entry = format!("{name} {preview:?} {outcome}");
        let mut log = lock(&self.dialogs);
        if log.len() == DIALOG_LOG {
            log.pop_front();
        }
        log.push_back(entry.clone());
        drop(log);
        self.push_activity(Activity::Dialog(entry));
        answer
    }

    /// Dialogs closed since the last call.
    pub fn take_dialogs(&self) -> Vec<String> {
        lock(&self.dialogs).drain(..).collect()
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

    /// Runs an input action. A dialog answer covers only the action after it.
    async fn act<T>(
        &self,
        label: String,
        action: impl std::future::Future<Output = Result<T, AgentError>>,
    ) -> Result<T, AgentError> {
        let result = self.logged(label, action).await;
        lock(&self.dialog_answer).take();
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
        self.act(format!("navigate {url}"), self.target.navigate(url))
            .await
    }

    pub async fn click(&self, reference: &str) -> Result<(), AgentError> {
        self.act(format!("click {reference}"), self.click_ref(reference))
            .await
    }

    async fn click_ref(&self, reference: &str) -> Result<(), AgentError> {
        let node = self.node(reference)?;
        let (x, y) = self.center(reference, node).await?;
        self.click_point(x, y).await
    }

    /// Clicks viewport coordinates in CSS pixels, for targets an outline
    /// cannot name.
    pub async fn click_at(&self, x: f64, y: f64) -> Result<(), AgentError> {
        self.act(format!("click at ({x}, {y})"), async {
            if !(x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0) {
                return Err(AgentError::Navigation(format!(
                    "({x}, {y}) is not a point in the viewport"
                )));
            }
            self.click_point(x, y).await
        })
        .await
    }

    async fn click_point(&self, x: f64, y: f64) -> Result<(), AgentError> {
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
        self.act(label, self.type_into(reference, text, submit))
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
        self.act(format!("press {combo}"), self.press_keys(combo))
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
        self.act(label, self.scroll_by(reference, direction)).await
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

    pub async fn go_back(&self) -> Result<(), AgentError> {
        self.act("back".into(), self.traverse(-1)).await
    }

    pub async fn go_forward(&self) -> Result<(), AgentError> {
        self.act("forward".into(), self.traverse(1)).await
    }

    async fn traverse(&self, delta: i64) -> Result<(), AgentError> {
        let history = self
            .target
            .call("Page.getNavigationHistory", json!({}))
            .await?;
        let current = history
            .get("currentIndex")
            .and_then(Value::as_i64)
            .ok_or_else(|| response("Page.getNavigationHistory", "missing currentIndex"))?;
        let entry = usize::try_from(current + delta)
            .ok()
            .and_then(|index| history.get("entries")?.as_array()?.get(index))
            .ok_or_else(|| {
                AgentError::Refused(format!(
                    "there is no {} page in this page's history",
                    if delta < 0 { "previous" } else { "next" }
                ))
            })?;
        let id = entry
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| response("Page.getNavigationHistory", "missing entry id"))?;
        lock(&self.refs).clear();
        self.target.go_to_history_entry(id).await
    }

    pub async fn hover(&self, reference: &str) -> Result<(), AgentError> {
        self.act(format!("hover {reference}"), async {
            let node = self.node(reference)?;
            let (x, y) = self.center(reference, node).await?;
            self.target
                .call(
                    "Input.dispatchMouseEvent",
                    json!({"type": "mouseMoved", "x": x, "y": y, "buttons": 0}),
                )
                .await?;
            Ok(())
        })
        .await
    }

    /// Selects the options of a `<select>` whose value or visible label is in
    /// `values` and deselects the rest, then fires `input` and `change`.
    pub async fn select_option(
        &self,
        reference: &str,
        values: &[String],
    ) -> Result<(), AgentError> {
        self.act(
            format!("select {reference} {values:?}"),
            self.select_values(reference, values),
        )
        .await
    }

    /// Sets the files of an `<input type="file">` as a user's choice would,
    /// firing `input` and `change`. Chromium reads `files`, so callers decide
    /// which host paths may be passed.
    pub async fn upload_files(&self, reference: &str, files: &[String]) -> Result<(), AgentError> {
        let names: Vec<&str> = files
            .iter()
            .map(|file| file.rsplit('/').next().unwrap_or(file))
            .collect();
        self.act(format!("upload {reference} {names:?}"), async {
            let node = self.node(reference)?;
            self.target
                .call(
                    "DOM.setFileInputFiles",
                    json!({"files": files, "backendNodeId": node}),
                )
                .await?;
            self.target.settle().await
        })
        .await
    }

    /// Where files uploaded to the ref would go: the URL of the document that
    /// holds the file input (a frame may differ from the page) and of the page.
    pub async fn upload_target(&self, reference: &str) -> Result<UploadTarget, AgentError> {
        let node = self.node(reference)?;
        let object = self
            .target
            .call("DOM.resolveNode", json!({"backendNodeId": node}))
            .await?;
        let object_id = object
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| response("DOM.resolveNode", "missing objectId"))?;
        let frame = self
            .target
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": "function() { return this.localName === \"input\" && this.type === \"file\" ? this.ownerDocument.location?.href ?? null : null; }",
                    "returnByValue": true,
                }),
            )
            .await?;
        let frame_url = frame
            .pointer("/result/value")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::Refused(format!("{reference} is not a file input")))?
            .to_owned();
        let page = self
            .target
            .call(
                "Runtime.evaluate",
                json!({"expression": "location.href", "returnByValue": true}),
            )
            .await?;
        let page_url = page
            .pointer("/result/value")
            .and_then(Value::as_str)
            .ok_or_else(|| response("Runtime.evaluate", "missing location"))?
            .to_owned();
        Ok(UploadTarget {
            page_url,
            frame_url,
        })
    }

    async fn select_values(&self, reference: &str, values: &[String]) -> Result<(), AgentError> {
        let node = self.node(reference)?;
        let object = self
            .target
            .call("DOM.resolveNode", json!({"backendNodeId": node}))
            .await?;
        let object_id = object
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| response("DOM.resolveNode", "missing objectId"))?;
        let outcome = self
            .target
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": SELECT_OPTIONS,
                    "arguments": [{"value": values}],
                    "returnByValue": true,
                }),
            )
            .await?;
        if let Some(exception) = outcome.get("exceptionDetails") {
            return Err(response("Runtime.callFunctionOn", &exception.to_string()));
        }
        match outcome.pointer("/result/value").and_then(Value::as_str) {
            Some(refusal) => Err(AgentError::Refused(format!("{reference}: {refusal}"))),
            None => Ok(()),
        }
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
        responses: Mutex<std::collections::HashMap<&'static str, Value>>,
    }

    #[async_trait::async_trait]
    impl CdpTarget for Recorder {
        async fn call(&self, method: &str, params: Value) -> Result<Value, AgentError> {
            lock(&self.calls).push((method.to_owned(), params));
            if let Some(response) = lock(&self.responses).get(method) {
                return Ok(response.clone());
            }
            Ok(match method {
                "Accessibility.getFullAXTree" => json!({"nodes": [{
                    "nodeId": "1", "role": {"value": "button"}, "name": {"value": "Go"},
                    "backendDOMNodeId": 7
                }]}),
                "DOM.getContentQuads" => json!({"quads": [[10, 20, 30, 20, 30, 40, 10, 40]]}),
                "DOM.resolveNode" => json!({"object": {"objectId": "obj-7"}}),
                "Page.getNavigationHistory" => json!({
                    "currentIndex": 1,
                    "entries": [{"id": 10, "url": "https://a.test/"}, {"id": 11, "url": "https://b.test/"}],
                }),
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

    #[tokio::test]
    async fn click_at_sends_a_trusted_click_at_the_point() {
        let target = Arc::new(Recorder::default());
        let page = PageAgent::new(target.clone());

        page.click_at(40.0, 12.5).await.unwrap();
        let refused = page.click_at(f64::NAN, 1.0).await;

        let calls = lock(&target.calls).clone();
        assert_eq!(calls.len(), 3);
        for (method, params) in &calls {
            assert_eq!(method, "Input.dispatchMouseEvent");
            assert_eq!(
                (params["x"].as_f64(), params["y"].as_f64()),
                (Some(40.0), Some(12.5))
            );
        }
        assert_eq!(calls[1].1["type"], "mousePressed");
        assert!(matches!(refused, Err(AgentError::Navigation(_))));
        assert!(matches!(
            page.activity().last(),
            Some(Activity::Action { label, error: Some(_) }) if label == "click at (NaN, 1)"
        ));
    }

    #[tokio::test]
    async fn history_moves_one_entry_and_refuses_past_either_end() {
        let target = Arc::new(Recorder::default());
        let page = PageAgent::new(target.clone());
        page.snapshot().await.unwrap();

        page.go_back().await.unwrap();
        let past_end = page.go_forward().await;

        let calls = lock(&target.calls).clone();
        let moved: Vec<_> = calls
            .iter()
            .filter(|(method, _)| method == "Page.navigateToHistoryEntry")
            .map(|(_, params)| params["entryId"].clone())
            .collect();
        assert_eq!(moved, [json!(10)]);
        assert!(
            matches!(&past_end, Err(AgentError::Refused(reason)) if reason.contains("no next page")),
            "{past_end:?}"
        );
        assert!(matches!(
            page.click("e7").await,
            Err(AgentError::UnknownRef(_))
        ));
    }

    #[tokio::test]
    async fn hover_moves_the_mouse_onto_the_element_without_pressing() {
        let target = Arc::new(Recorder::default());
        let page = PageAgent::new(target.clone());
        page.snapshot().await.unwrap();
        lock(&target.calls).clear();

        page.hover("e7").await.unwrap();

        let calls = lock(&target.calls).clone();
        let (method, event) = calls.last().unwrap();
        assert_eq!(method, "Input.dispatchMouseEvent");
        assert_eq!(event["type"], "mouseMoved");
        assert_eq!(
            (event["x"].as_f64(), event["y"].as_f64()),
            (Some(20.0), Some(30.0))
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "Input.dispatchMouseEvent")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn select_option_hands_the_values_to_the_element_and_reports_its_refusal() {
        let target = Arc::new(Recorder::default());
        let page = PageAgent::new(target.clone());
        page.snapshot().await.unwrap();

        page.select_option("e7", &["Large".into()]).await.unwrap();
        lock(&target.responses).insert(
            "Runtime.callFunctionOn",
            json!({"result": {"type": "string", "value": "takes exactly one value"}}),
        );
        let refused = page.select_option("e7", &[]).await;

        let calls = lock(&target.calls).clone();
        let (_, call) = calls
            .iter()
            .find(|(method, _)| method == "Runtime.callFunctionOn")
            .unwrap();
        assert_eq!(call["objectId"], "obj-7");
        assert_eq!(call["arguments"], json!([{"value": ["Large"]}]));
        assert_eq!(call["returnByValue"], true);
        assert!(
            matches!(&refused, Err(AgentError::Refused(reason)) if reason == "e7: takes exactly one value"),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn dialogs_are_dismissed_unless_answered_for_the_next_action() {
        let page = PageAgent::new(Arc::new(Recorder::default()));
        let dismiss = DialogAnswer {
            accept: false,
            prompt_text: None,
        };
        let accept = DialogAnswer {
            accept: true,
            prompt_text: None,
        };

        let alert = page.on_dialog(DialogKind::Alert, "Saved", None);
        let leave = page.on_dialog(DialogKind::BeforeUnload, "", None);
        let unanswered = page.on_dialog(DialogKind::Confirm, "Delete it?", None);
        page.answer_next_dialog(DialogAnswer {
            accept: true,
            prompt_text: Some("Ada".into()),
        });
        let answered = page.on_dialog(DialogKind::Prompt, "Name?", Some("nobody"));
        let consumed = page.on_dialog(DialogKind::Confirm, "Again?", None);
        page.answer_next_dialog(accept.clone());
        let kept_default = page.on_dialog(DialogKind::Prompt, "Name?", Some("nobody"));
        page.answer_next_dialog(accept.clone());
        page.click_at(1.0, 1.0).await.unwrap();
        let expired = page.on_dialog(DialogKind::Confirm, "Late?", None);

        assert_eq!((alert, leave), (accept.clone(), accept));
        assert_eq!(unanswered, dismiss);
        assert_eq!(
            answered,
            DialogAnswer {
                accept: true,
                prompt_text: Some("Ada".into())
            }
        );
        assert_eq!(consumed, dismiss, "an answer covers one dialog");
        assert_eq!(kept_default.prompt_text.as_deref(), Some("nobody"));
        assert_eq!(expired, dismiss, "an answer covers only the next action");
        let dialogs = page.take_dialogs();
        assert_eq!(dialogs[0], "alert \"Saved\" closed");
        assert!(
            dialogs[2].starts_with(
                "confirm \"Delete it?\" dismissed; to accept it, call browser_handle_dialog"
            ),
            "{dialogs:?}"
        );
        assert_eq!(dialogs[3], "prompt \"Name?\" answered \"Ada\"");
        assert!(page.take_dialogs().is_empty());
        assert!(
            page.activity()
                .iter()
                .any(|entry| *entry == Activity::Dialog(dialogs[0].clone()))
        );
    }

    #[tokio::test]
    async fn upload_sets_the_files_of_the_ref_input() {
        let target = Arc::new(Recorder::default());
        let page = PageAgent::new(target.clone());
        let files = ["/work/report.pdf".to_owned(), "/work/notes.txt".to_owned()];
        assert!(matches!(
            page.upload_files("e7", &files).await,
            Err(AgentError::UnknownRef(_))
        ));
        page.snapshot().await.unwrap();
        lock(&target.calls).clear();

        page.upload_files("e7", &files).await.unwrap();

        let calls = lock(&target.calls).clone();
        assert_eq!(
            calls,
            [(
                "DOM.setFileInputFiles".to_owned(),
                json!({"files": files, "backendNodeId": 7})
            )]
        );
        assert!(matches!(
            page.activity().last(),
            Some(Activity::Action { label, error: None }) if label == r#"upload e7 ["report.pdf", "notes.txt"]"#
        ));
    }

    #[tokio::test]
    async fn upload_target_names_the_inputs_document_and_refuses_other_elements() {
        let target = Arc::new(Recorder::default());
        let page = PageAgent::new(target.clone());
        page.snapshot().await.unwrap();
        lock(&target.responses).insert(
            "Runtime.callFunctionOn",
            json!({"result": {"type": "string", "value": "https://forms.test/upload"}}),
        );
        lock(&target.responses).insert(
            "Runtime.evaluate",
            json!({"result": {"type": "string", "value": "https://site.test/apply"}}),
        );

        let found = page.upload_target("e7").await.unwrap();
        lock(&target.responses).insert(
            "Runtime.callFunctionOn",
            json!({"result": {"type": "object", "subtype": "null", "value": null}}),
        );
        let refused = page.upload_target("e7").await;

        assert_eq!(
            found,
            UploadTarget {
                page_url: "https://site.test/apply".into(),
                frame_url: "https://forms.test/upload".into(),
            }
        );
        assert!(
            matches!(&refused, Err(AgentError::Refused(reason)) if reason == "e7 is not a file input"),
            "{refused:?}"
        );
        assert!(
            !lock(&target.calls)
                .iter()
                .any(|(method, _)| method == "DOM.setFileInputFiles")
        );
    }
}
