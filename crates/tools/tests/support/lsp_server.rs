use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn read(input: &mut impl BufRead) -> Result<Value, Box<dyn std::error::Error>> {
    let mut size = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Err("EOF".into());
        }
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            size = Some(value.trim().parse::<usize>()?);
        }
    }
    let mut bytes = vec![0; size.ok_or("length")?];
    input.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn send(value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::to_vec(value)?;
    let mut out = std::io::stdout().lock();
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = args.get(1).ok_or("mode")?;
    let pid = args.get(2).ok_or("pid")?;
    std::fs::write(pid, std::process::id().to_string())?;
    let mut input = std::io::stdin().lock();
    let request = read(&mut input)?;
    assert_eq!(request["method"], "initialize");
    std::io::stderr().write_all(&vec![b'x'; 131072])?;
    send(&json!({"jsonrpc":"2.0","id":request["id"],"result":{"capabilities":{}}}))?;
    assert_eq!(read(&mut input)?["method"], "initialized");
    let opened = read(&mut input)?;
    assert_eq!(opened["method"], "textDocument/didOpen");
    match mode.as_str() {
        "crash" => std::process::exit(3),
        "hang" => {
            read(&mut input)?;
            return Err("unexpected input".into());
        }
        "malformed" => {
            std::io::stdout().write_all(b"Content-Length: 999999999\r\n\r\n")?;
            read(&mut input)?;
            return Ok(());
        }
        _ => {}
    }
    let diagnostics = (1..=4).map(|n| json!({"range":{"start":{"line":1,"character":2},"end":{"line":1,"character":4}},"severity":n,"code":n.to_string(),"message":format!("message {n}")})).collect::<Vec<_>>();
    let diagnostics = if mode == "empty" { vec![] } else { diagnostics };
    send(
        &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":opened["params"]["textDocument"]["uri"],"diagnostics":diagnostics}}),
    )?;
    let request = read(&mut input)?;
    assert_eq!(request["method"], "shutdown");
    send(&json!({"jsonrpc":"2.0","id":request["id"],"result":null}))?;
    assert_eq!(read(&mut input)?["method"], "exit");
    std::fs::write(format!("{pid}.exit"), "graceful")?;
    Ok(())
}
