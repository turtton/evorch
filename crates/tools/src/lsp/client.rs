use std::{path::Path, sync::Arc, time::Duration};

use sandbox::{CommandSpec, Sandbox, StdioSession};
use serde_json::json;
use tokio::{
    io::BufReader,
    process::{ChildStderr, ChildStdin, ChildStdout},
    sync::mpsc,
    task::JoinSet,
};

use super::{
    LspError,
    protocol::{self, Batch, Message},
};

pub(super) struct Connection {
    session: StdioSession,
    stdin: ChildStdin,
    incoming: mpsc::Receiver<Result<Message, LspError>>,
    _tasks: JoinSet<()>,
}

impl Connection {
    pub async fn spawn(sandbox: Arc<dyn Sandbox>, command: CommandSpec) -> Result<Self, LspError> {
        let mut session =
            tokio::task::spawn_blocking(move || StdioSession::spawn(sandbox.as_ref(), command))
                .await
                .map_err(|_| LspError::Worker)??;
        let stdin = ChildStdin::from_std(session.take_stdin().ok_or(LspError::Closed)?)?;
        let stdout = ChildStdout::from_std(session.take_stdout().ok_or(LspError::Closed)?)?;
        let mut stderr = ChildStderr::from_std(session.take_stderr().ok_or(LspError::Closed)?)?;
        let (sender, incoming) = mpsc::channel(32);
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let result = protocol::read(&mut reader).await;
                let failed = result.is_err();
                if sender.send(result).await.is_err() || failed {
                    break;
                }
            }
        });
        tasks.spawn(async move {
            // Server stderr is untrusted; drain without retaining or logging it.
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        });
        Ok(Self {
            session,
            stdin,
            incoming,
            _tasks: tasks,
        })
    }

    async fn receive(&mut self) -> Result<Message, LspError> {
        let message = self.incoming.recv().await.ok_or(LspError::Closed)??;
        if message.method.is_some()
            && let Some(id) = &message.id
        {
            protocol::write(&mut self.stdin, &json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32601,"message":"Method not supported"}})).await?;
        }
        Ok(message)
    }

    async fn response(&mut self, id: u8) -> Result<(), LspError> {
        loop {
            let message = self.receive().await?;
            if message.method.is_none() && message.id == Some(json!(id)) {
                return match message.error {
                    Some(_) => Err(LspError::Server),
                    None => Ok(()),
                };
            }
        }
    }

    pub async fn open(&mut self, path: &Path, language: &str) -> Result<Batch, LspError> {
        let uri = reqwest::Url::from_file_path(path).map_err(|_| LspError::Path)?;
        let path = path.to_owned();
        let text = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let file = std::fs::File::open(path)?;
            if !file.metadata()?.is_file() {
                return Err(LspError::Path);
            }
            let mut text = String::new();
            file.take(1_048_577).read_to_string(&mut text)?;
            if text.len() > 1_048_576 {
                return Err(LspError::Protocol);
            }
            Ok(text)
        })
        .await
        .map_err(|_| LspError::Worker)??;
        protocol::write(&mut self.stdin, &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":null,"rootUri":null,"capabilities":{}}})).await?;
        self.response(1).await?;
        protocol::write(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
        )
        .await?;
        protocol::write(&mut self.stdin, &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri.as_str(),"languageId":language,"version":1,"text":text}}})).await?;
        loop {
            let message = self.receive().await?;
            if message.method.as_deref() == Some("textDocument/publishDiagnostics")
                && message.id.is_none()
            {
                let batch: Batch =
                    serde_json::from_value(message.params.ok_or(LspError::Protocol)?)
                        .map_err(|_| LspError::Protocol)?;
                if batch.uri == uri.as_str() {
                    return Ok(batch);
                }
            }
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), LspError> {
        protocol::write(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
        )
        .await?;
        self.response(2).await?;
        protocol::write(&mut self.stdin, &json!({"jsonrpc":"2.0","method":"exit"})).await?;
        loop {
            if let Some(status) = self.session.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(LspError::Server)
                };
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}
