//! Test-only independent golden driver. No product builders, timers or services.
use serde::Deserialize;
use serde_json::Value;
use std::{fs, path::Path};
use tokio::{sync::mpsc, task::JoinHandle};
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Send { value: Value },
    Receive { value: Value },
    Gate { name: String },
    Signal { name: String },
    Close,
}
#[derive(Deserialize)]
struct Transcript {
    steps: Vec<Step>,
}
pub fn load(path: &Path) -> Vec<Step> {
    let content = fs::read_to_string(path).expect("fixture read");
    if path.extension().is_some_and(|ext| ext == "jsonl") {
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("JSONL step"))
            .collect()
    } else {
        serde_json::from_str::<Transcript>(&content)
            .expect("JSON transcript")
            .steps
    }
}
#[derive(Debug, PartialEq)]
pub enum PeerEvent {
    Received(Value),
    Signal(String),
    Closed,
}
pub struct Driver {
    pub input: mpsc::Sender<Value>,
    pub output: mpsc::Receiver<PeerEvent>,
    pub control: mpsc::Sender<String>,
    pub done: JoinHandle<Result<(), String>>,
}
/// A Gate only progresses after the test sends the named control event. This can
/// hold an SSE stream open until the client reports completion, without a sleep.
pub fn spawn(steps: Vec<Step>) -> Driver {
    let (input, mut incoming) = mpsc::channel::<Value>(8);
    let (outgoing, output) = mpsc::channel(8);
    let (control, mut controls) = mpsc::channel::<String>(8);
    let done = tokio::spawn(async move {
        for (index, step) in steps.into_iter().enumerate() {
            match step {
                Step::Send { value } => {
                    if incoming.recv().await.as_ref() != Some(&value) {
                        return Err(format!("golden input mismatch at step {index}"));
                    }
                }
                Step::Receive { value } => outgoing
                    .send(PeerEvent::Received(value))
                    .await
                    .map_err(|_| "fixture output closed")?,
                Step::Signal { name } => outgoing
                    .send(PeerEvent::Signal(name))
                    .await
                    .map_err(|_| "fixture signal closed")?,
                Step::Gate { name } => {
                    if controls.recv().await.as_deref() != Some(&name) {
                        return Err(format!("golden gate mismatch at step {index}"));
                    }
                }
                Step::Close => {
                    outgoing
                        .send(PeerEvent::Closed)
                        .await
                        .map_err(|_| "fixture close lost")?;
                    return Ok(());
                }
            }
        }
        Err("transcript lacks terminal close event".into())
    });
    Driver {
        input,
        output,
        control,
        done,
    }
}
