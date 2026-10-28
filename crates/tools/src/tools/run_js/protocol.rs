//! JSON Lines protocol between evorch and its `js-worker` process (ADR 0039).
//!
//! One JSON object per line over the worker's stdin / stdout. The worker is
//! untrusted: the host bounds every line it reads and treats any malformed
//! message as a worker failure.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Upper bound for one protocol line. A tool result is already bounded by the
/// executor, so this only stops a runaway worker from exhausting host memory.
pub const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;

/// Host → worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// Starts the program. Sent exactly once, first.
    Run {
        code: String,
        timeout_ms: u64,
        memory_limit_bytes: u64,
        /// Names exposed as `tools.<name>`.
        tools: Vec<String>,
    },
    /// Answers the `tool_call` with the same `id`. Exactly one of `result` and
    /// `error` is set; `error` makes `tools.<name>()` reject with that message.
    ToolResult {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Worker → host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    /// `tools.<name>(args)` was called. `id` is unique within the run.
    ToolCall { id: u64, name: String, args: Value },
    /// Text printed by `console.log` / `print`.
    Output { text: String },
    /// The program ended. Always the last message.
    Done {
        /// The returned value as JSON (`null` for `undefined` or on error).
        #[serde(default)]
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<JsError>,
        /// The worker's own deadline expired.
        #[serde(default)]
        timed_out: bool,
    },
}

/// A JavaScript exception with the stack trace in user-code line numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsError {
    pub message: String,
    #[serde(default)]
    pub stack: String,
}

/// Reads one message. `Ok(None)` is a clean end of stream.
///
/// # Errors
/// I/O failure, a line longer than [`MAX_LINE_BYTES`], or invalid JSON.
pub async fn read_message<T, R>(reader: &mut R) -> std::io::Result<Option<T>>
where
    T: serde::de::DeserializeOwned,
    R: AsyncBufRead + Unpin,
{
    Ok(read_message_sized(reader)
        .await?
        .map(|(message, _)| message))
}

/// Like [`read_message`], and also returns the size of the line the message came in.
///
/// # Errors
/// As [`read_message`].
pub async fn read_message_sized<T, R>(reader: &mut R) -> std::io::Result<Option<(T, usize)>>
where
    T: serde::de::DeserializeOwned,
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        let (take, found) = match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => (index + 1, true),
            None => (available.len(), false),
        };
        if line.len() + take > MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("protocol line exceeds {MAX_LINE_BYTES} bytes"),
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if found {
            break;
        }
    }
    serde_json::from_slice(&line)
        .map(|message| Some((message, line.len())))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// One message as the line [`write_message`] would send.
///
/// # Errors
/// A message that cannot be serialised.
pub fn encode_message<T: Serialize>(message: &T) -> std::io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(message)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    line.push(b'\n');
    Ok(line)
}

/// Writes one message as a single line and flushes.
///
/// # Errors
/// I/O failure or a message that cannot be serialised.
pub async fn write_message<T, W>(writer: &mut W, message: &T) -> std::io::Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(message)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn messages_round_trip_through_json_lines() {
        let sent = [
            HostMessage::Run {
                code: "return 1".into(),
                timeout_ms: 30_000,
                memory_limit_bytes: 64 << 20,
                tools: vec!["read".into(), "grep".into()],
            },
            HostMessage::ToolResult {
                id: 7,
                result: Some(json!({"ok": true, "text": "日本語"})),
                error: None,
            },
            HostMessage::ToolResult {
                id: 8,
                result: None,
                error: Some("denied".into()),
            },
        ];
        let mut wire = Vec::new();
        for message in &sent {
            write_message(&mut wire, message).await.unwrap();
        }
        assert_eq!(
            wire.iter().filter(|byte| **byte == b'\n').count(),
            sent.len()
        );

        let mut reader = BufReader::new(wire.as_slice());
        for expected in sent {
            let received: HostMessage = read_message(&mut reader).await.unwrap().unwrap();
            assert_eq!(received, expected);
        }
        assert!(
            read_message::<HostMessage, _>(&mut reader)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn worker_messages_use_the_documented_wire_shape() {
        let mut wire = Vec::new();
        write_message(
            &mut wire,
            &WorkerMessage::ToolCall {
                id: 1,
                name: "read".into(),
                args: json!({"path": "a"}),
            },
        )
        .await
        .unwrap();
        write_message(
            &mut wire,
            &WorkerMessage::Output {
                text: "hi\n".into(),
            },
        )
        .await
        .unwrap();
        write_message(
            &mut wire,
            &WorkerMessage::Done {
                value: json!([1]),
                error: None,
                timed_out: false,
            },
        )
        .await
        .unwrap();
        let lines: Vec<Value> = String::from_utf8(wire)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            json!({"type": "tool_call", "id": 1, "name": "read", "args": {"path": "a"}})
        );
        assert_eq!(lines[1], json!({"type": "output", "text": "hi\n"}));
        assert_eq!(
            lines[2],
            json!({"type": "done", "value": [1], "timed_out": false})
        );
    }

    #[tokio::test]
    async fn overlong_lines_and_invalid_json_are_errors() {
        let mut oversized = vec![b'x'; MAX_LINE_BYTES + 1];
        oversized.push(b'\n');
        let mut reader = BufReader::new(oversized.as_slice());
        assert!(read_message::<WorkerMessage, _>(&mut reader).await.is_err());

        let mut reader = BufReader::new(&b"not json\n"[..]);
        assert!(read_message::<WorkerMessage, _>(&mut reader).await.is_err());
    }
}
