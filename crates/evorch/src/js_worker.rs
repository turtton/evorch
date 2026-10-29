//! `evorch js-worker`: the sandboxed process that runs `run_js` programs
//! (ADR 0039).
//!
//! It speaks the JSON Lines protocol of `tools::tools::run_js::protocol` on
//! stdin / stdout and does nothing else: the program it runs can reach the
//! outside world only through `tools.<name>()` requests that evorch answers.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use js_engine::{HostCalls, RunOptions};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};
use tools::tools::run_js::protocol::{
    HostMessage, JsError, WorkerMessage, read_message, write_message,
};

/// Entry point of the `js-worker` subcommand.
pub fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("js-worker: cannot start the runtime: {error}");
            return ExitCode::from(1);
        }
    };
    let local = tokio::task::LocalSet::new();
    let input = BufReader::new(tokio::io::stdin());
    match local.block_on(&runtime, serve(input, tokio::io::stdout())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("js-worker: {error}");
            ExitCode::from(1)
        }
    }
}

type Reply = Result<Value, String>;

struct Bridge {
    next_id: Cell<u64>,
    pending: RefCell<HashMap<u64, oneshot::Sender<Reply>>>,
    out: mpsc::UnboundedSender<WorkerMessage>,
}

#[async_trait(?Send)]
impl HostCalls for Bridge {
    async fn call_tool(&self, name: &str, args: Value) -> Reply {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        let (reply, answer) = oneshot::channel();
        self.pending.borrow_mut().insert(id, reply);
        let call = WorkerMessage::ToolCall {
            id,
            name: name.to_owned(),
            args,
        };
        if self.out.send(call).is_err() {
            self.pending.borrow_mut().remove(&id);
            return Err("evorch closed the connection".to_owned());
        }
        answer
            .await
            .unwrap_or_else(|_| Err("evorch closed the connection".to_owned()))
    }

    fn output(&self, text: &str) {
        let _ = self.out.send(WorkerMessage::Output {
            text: text.to_owned(),
        });
    }
}

/// Runs one program: reads `run`, answers `tool_call`s through `input`, and
/// ends with `done` on `output`. Returns when the program has finished.
///
/// # Errors
/// The first message is not `run`, or the protocol streams fail.
pub async fn serve<R, W>(mut input: R, mut output: W) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let Some(HostMessage::Run {
        code,
        timeout_ms,
        memory_limit_bytes,
        tools,
    }) = read_message(&mut input).await?
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the first message must be run",
        ));
    };

    let (out, mut queue) = mpsc::unbounded_channel();
    let bridge = Rc::new(Bridge {
        next_id: Cell::new(0),
        pending: RefCell::new(HashMap::new()),
        out: out.clone(),
    });

    let writer = tokio::task::spawn_local(async move {
        while let Some(message) = queue.recv().await {
            write_message(&mut output, &message).await?;
        }
        Ok::<(), std::io::Error>(())
    });

    let answers = Rc::clone(&bridge);
    let reader = tokio::task::spawn_local(async move {
        while let Ok(Some(message)) = read_message::<HostMessage, _>(&mut input).await {
            if let HostMessage::ToolResult { id, result, error } = message
                && let Some(reply) = answers.pending.borrow_mut().remove(&id)
            {
                let _ = reply.send(match error {
                    Some(error) => Err(error),
                    None => Ok(result.unwrap_or(Value::Null)),
                });
            }
        }
        // evorch went away: calls still waiting for an answer fail instead of hanging.
        answers.pending.borrow_mut().clear();
    });

    let outcome = js_engine::run(
        &code,
        RunOptions {
            timeout: Duration::from_millis(timeout_ms),
            memory_limit_bytes: usize::try_from(memory_limit_bytes).unwrap_or(usize::MAX),
            tools,
        },
        bridge.clone(),
    )
    .await;

    let _ = out.send(WorkerMessage::Done {
        value: outcome.value,
        error: outcome.error.map(|error| JsError {
            message: error.message,
            stack: error.stack,
        }),
        timed_out: outcome.timed_out,
    });
    reader.abort();
    let _ = reader.await;
    drop(bridge);
    drop(out);
    writer
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncWriteExt, duplex};

    async fn host_run(
        code: &str,
        tools: &[&str],
    ) -> (tokio::io::DuplexStream, BufReader<tokio::io::DuplexStream>) {
        let (host_to_worker, worker_in) = duplex(1 << 20);
        let (worker_out, worker_to_host) = duplex(1 << 20);
        tokio::task::spawn_local(serve(BufReader::new(worker_in), worker_out));
        let mut sender = host_to_worker;
        write_message(
            &mut sender,
            &HostMessage::Run {
                code: code.to_owned(),
                timeout_ms: 60_000,
                memory_limit_bytes: 64 << 20,
                tools: tools.iter().map(|name| (*name).to_owned()).collect(),
            },
        )
        .await
        .unwrap();
        (sender, BufReader::new(worker_to_host))
    }

    #[tokio::test]
    async fn concurrent_tool_calls_are_multiplexed_by_id() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut host, mut worker) = host_run(
                    "const [a, b] = await Promise.all([tools.echo({n: 1}), tools.echo({n: 2})]); console.log('seen'); return [a, b];",
                    &["echo"],
                )
                .await;
                let mut calls = Vec::new();
                while calls.len() < 2 {
                    let Some(WorkerMessage::ToolCall { id, name, args }) =
                        read_message(&mut worker).await.unwrap()
                    else {
                        panic!("expected a tool_call");
                    };
                    assert_eq!(name, "echo");
                    calls.push((id, args));
                }
                assert_ne!(calls[0].0, calls[1].0);
                // Answer in reverse order: ids, not arrival order, pair the replies.
                for (id, args) in calls.iter().rev() {
                    write_message(
                        &mut host,
                        &HostMessage::ToolResult { id: *id, result: Some(json!({"echo": args["n"]})), error: None },
                    )
                    .await
                    .unwrap();
                }
                let mut output = String::new();
                let done = loop {
                    match read_message::<WorkerMessage, _>(&mut worker).await.unwrap().unwrap() {
                        WorkerMessage::Output { text } => output.push_str(&text),
                        WorkerMessage::Done { value, error, timed_out } => break (value, error, timed_out),
                        other => panic!("unexpected {other:?}"),
                    }
                };
                assert_eq!(output, "seen\n");
                assert_eq!(done, (json!([{"echo": 1}, {"echo": 2}]), None, false));
                host.shutdown().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn an_error_reply_rejects_the_call_and_the_stack_uses_user_lines() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut host, mut worker) =
                    host_run("const x = 1;\nawait tools.echo({});", &["echo"]).await;
                let Some(WorkerMessage::ToolCall { id, .. }) =
                    read_message(&mut worker).await.unwrap()
                else {
                    panic!("expected a tool_call");
                };
                write_message(
                    &mut host,
                    &HostMessage::ToolResult {
                        id,
                        result: None,
                        error: Some("denied".into()),
                    },
                )
                .await
                .unwrap();
                let Some(WorkerMessage::Done {
                    error: Some(error), ..
                }) = read_message(&mut worker).await.unwrap()
                else {
                    panic!("expected done with an error");
                };
                assert_eq!(error.message, "Error: denied");
                assert!(error.stack.contains("run_js:2"), "{}", error.stack);
            })
            .await;
    }

    #[tokio::test]
    async fn a_closed_connection_fails_waiting_calls_instead_of_hanging() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (host, mut worker) = host_run("await tools.echo({});", &["echo"]).await;
                let Some(WorkerMessage::ToolCall { .. }) = read_message(&mut worker).await.unwrap()
                else {
                    panic!("expected a tool_call");
                };
                drop(host);
                let Some(WorkerMessage::Done {
                    error: Some(error), ..
                }) = read_message(&mut worker).await.unwrap()
                else {
                    panic!("expected done with an error");
                };
                assert!(error.message.contains("closed the connection"));
            })
            .await;
    }
}
