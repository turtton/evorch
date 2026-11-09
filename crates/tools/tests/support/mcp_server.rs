//! Deterministic local peer. The optional Unix control stream is test-only;
//! it makes process/read/flush/EOF synchronization independent of wall time.
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Command, Stdio},
};
fn main() {
    if run().is_err() {
        eprintln!("MCP fixture protocol mismatch");
        std::process::exit(2);
    }
}
fn send_control(control: &mut Option<UnixStream>, value: &Value) -> Result<(), ()> {
    if let Some(control) = control {
        let mut bytes = serde_json::to_vec(value).map_err(|_| ())?;
        bytes.push(b'\n');
        control.write_all(&bytes).map_err(|_| ())?;
        control.flush().map_err(|_| ())?;
    }
    Ok(())
}
fn run() -> Result<(), ()> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next().ok_or(())?;
    if first == "--descendant" {
        let mut control = UnixStream::connect(args.next().ok_or(())?).map_err(|_| ())?;
        serde_json::to_writer(
            &mut control,
            &json!({"event":"descendant_ready","pid":std::process::id()}),
        )
        .map_err(|_| ())?;
        control.write_all(b"\n").map_err(|_| ())?;
        control.flush().map_err(|_| ())?;
        // Block on the test-owned connection. Process-group shutdown, not test
        // connection closure, must end this child and close its inherited pipes.
        let mut byte = [0];
        control.read_exact(&mut byte).map_err(|_| ())?;
        return Err(());
    }
    let path = PathBuf::from(first);
    let control_path = args.next();
    let mut control = control_path
        .as_ref()
        .map(UnixStream::connect)
        .transpose()
        .map_err(|_| ())?;
    send_control(
        &mut control,
        &json!({"event":"started","pid":std::process::id()}),
    )?;
    let file = std::fs::read_to_string(path).map_err(|_| ())?;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    for line in file.lines() {
        let step: Value = serde_json::from_str(line).map_err(|_| ())?;
        match step["event"].as_str().ok_or(())? {
            "send" => {
                let mut line = String::new();
                if input.read_line(&mut line).map_err(|_| ())? == 0 {
                    return Err(());
                }
                let actual: Value = serde_json::from_str(&line).map_err(|_| ())?;
                if actual != step["value"] {
                    return Err(());
                }
                send_control(&mut control, &json!({"event":"read","value":actual}))?;
            }
            "receive" => {
                let mut bytes = serde_json::to_vec(&step["value"]).map_err(|_| ())?;
                bytes.push(b'\n');
                // Force many writes without assuming how the OS chunks reads.
                for chunk in bytes.chunks(7) {
                    output.write_all(chunk).map_err(|_| ())?;
                }
                output.flush().map_err(|_| ())?;
                send_control(&mut control, &json!({"event":"written"}))?;
            }
            "raw" => {
                if let Some(raw) = step["bytes"].as_str() {
                    output.write_all(raw.as_bytes()).map_err(|_| ())?;
                }
                if let Some(count) = step["repeat"].as_u64() {
                    let bytes = [b'x'; 8192];
                    let mut remaining = count;
                    while remaining > 0 {
                        let len = remaining.min(bytes.len() as u64) as usize;
                        output.write_all(&bytes[..len]).map_err(|_| ())?;
                        remaining -= len as u64;
                    }
                }
                output.flush().map_err(|_| ())?;
            }
            "stderr" => {
                let mut stderr = io::stderr().lock();
                let bytes = [b'e'; 8192];
                for _ in 0..step["chunks"].as_u64().ok_or(())? {
                    stderr.write_all(&bytes).map_err(|_| ())?;
                }
                stderr.flush().map_err(|_| ())?;
                send_control(&mut control, &json!({"event":"stderr_drained"}))?;
            }
            "gate" => {
                let mut command = String::new();
                io::BufReader::new(control.as_mut().ok_or(())?)
                    .read_line(&mut command)
                    .map_err(|_| ())?;
                if command.trim() != step["name"].as_str().ok_or(())? {
                    return Err(());
                }
            }
            "signal" => send_control(&mut control, &json!({"event":step["name"]}))?,
            "stdin_closed" => {
                let mut line = String::new();
                if input.read_line(&mut line).map_err(|_| ())? != 0 {
                    return Err(());
                }
                send_control(&mut control, &json!({"event":"stdin_closed"}))?;
            }
            "environment" => {
                let environment: serde_json::Map<String, Value> = std::env::vars()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect();
                send_control(
                    &mut control,
                    &json!({"event":"environment","env":environment,
                    "cwd":std::env::current_dir().map_err(|_| ())?}),
                )?;
            }
            "spawn_descendant" => {
                let _child = Command::new(std::env::current_exe().map_err(|_| ())?)
                    .arg("--descendant")
                    .arg(control_path.as_ref().ok_or(())?)
                    .stdin(Stdio::null())
                    .spawn()
                    .map_err(|_| ())?;
            }
            "close" => {
                send_control(&mut control, &json!({"event":"complete"}))?;
                return Ok(());
            }
            _ => return Err(()),
        }
    }
    Err(())
}
