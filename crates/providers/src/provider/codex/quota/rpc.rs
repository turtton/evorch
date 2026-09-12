use super::{CodexQuota, QuotaConfig, QuotaError, wire::RpcLimits};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
};

fn io(error: std::io::Error) -> QuotaError {
    QuotaError::Process(error.kind())
}

#[derive(Deserialize)]
struct Envelope {
    id: Option<Value>,
    result: Option<Value>,
    error: Option<RpcError>,
}
#[derive(Deserialize)]
struct RpcError {
    code: i64,
}
#[derive(Deserialize)]
struct AccountResponse {
    account: Option<Account>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Account {
    plan_type: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LimitsResponse {
    rate_limits: RpcLimits,
}

struct Connection {
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Connection {
    async fn send(&mut self, message: Value) -> Result<(), QuotaError> {
        let mut bytes =
            serde_json::to_vec(&message).map_err(|_| QuotaError::Protocol("RPC serialization"))?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await.map_err(io)?;
        self.input.flush().await.map_err(io)
    }

    async fn request<T: DeserializeOwned>(&mut self, message: Value) -> Result<T, QuotaError> {
        let id = message["id"].clone();
        self.send(message).await?;
        loop {
            // Bound each line even when a corrupt server never emits a newline.
            let mut bytes = Vec::new();
            let count = (&mut self.output)
                .take(1024 * 1024)
                .read_until(b'\n', &mut bytes)
                .await
                .map_err(io)?;
            if count == 0 {
                return Err(QuotaError::Protocol("app-server EOF"));
            }
            if bytes.last() != Some(&b'\n') {
                return Err(QuotaError::Protocol("oversized or incomplete RPC frame"));
            }
            let frame: Envelope = serde_json::from_slice(&bytes)
                .map_err(|_| QuotaError::Protocol("invalid RPC frame"))?;
            if frame.id.as_ref() != Some(&id) {
                continue;
            }
            if let Some(error) = frame.error {
                return Err(QuotaError::Rpc(error.code));
            }
            return serde_json::from_value(
                frame
                    .result
                    .ok_or(QuotaError::Protocol("missing RPC result"))?,
            )
            .map_err(|_| QuotaError::Protocol("invalid RPC result"));
        }
    }

    async fn quota(&mut self) -> Result<CodexQuota, QuotaError> {
        let _: Value = self
            .request(json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"evorch","version":env!("CARGO_PKG_VERSION")}}}))
            .await?;
        self.send(json!({"method":"initialized","params":{}}))
            .await?;
        let account: AccountResponse = self
            .request(json!({"id":2,"method":"account/read","params":{"refreshToken":false}}))
            .await?;
        let limits: LimitsResponse = self
            .request(json!({"id":3,"method":"account/rateLimits/read","params":{}}))
            .await?;
        limits
            .rate_limits
            .convert(account.account.and_then(|account| account.plan_type))
    }
}

pub(super) async fn fetch(config: &QuotaConfig) -> Result<CodexQuota, QuotaError> {
    let mut child = Command::new(&config.app_server_program)
        .args(&config.app_server_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(io)?;
    let mut connection = Connection {
        input: child
            .stdin
            .take()
            .ok_or(QuotaError::Protocol("missing child stdin"))?,
        output: BufReader::new(
            child
                .stdout
                .take()
                .ok_or(QuotaError::Protocol("missing child stdout"))?,
        ),
    };
    let result = tokio::time::timeout(config.timeout, connection.quota())
        .await
        .map_err(|_| QuotaError::Timeout)?;
    drop(connection);
    child.kill().await.map_err(io)?;
    result
}
