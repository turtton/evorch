//! Native MCP peer with a test-owned control socket: no polling or timed waits.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut control = UnixStream::connect(std::env::args_os().nth(1).unwrap())?;
    let mut commands = BufReader::new(control.try_clone()?);
    let secret = std::env::var("TOKEN")?;
    // The host must discard raw MCP stderr, including credential-bearing output.
    eprintln!("{secret}");
    writeln!(
        control,
        "{}",
        json!({"event":"started","pid":std::process::id(),"credential_present":!secret.is_empty()})
    )?;
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        let method = request["method"].as_str().unwrap_or_default();
        let Some(id) = request.get("id") else {
            continue;
        };
        let result = match method {
            "server/discover" => {
                json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"})
            }
            "tools/list" => {
                json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"echo","description":"Local evidence","inputSchema":{"type":"object"}}]})
            }
            "tools/call" => {
                writeln!(control, "{}", json!({"event":"call","id":id}))?;
                let mut command = String::new();
                commands.read_line(&mut command)?;
                if command.trim() == "crash" {
                    return Ok(());
                }
                if command.trim() == "secret-error" {
                    println!(
                        "{}",
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":secret}})
                    );
                    std::io::stdout().flush()?;
                    continue;
                }
                assert_eq!(command.trim(), "continue");
                json!({"resultType":"complete","content":[{"type":"text","text":"mcp-ok"}]})
            }
            _ => panic!("unexpected MCP method"),
        };
        println!("{}", json!({"jsonrpc":"2.0","id":id,"result":result}));
        std::io::stdout().flush()?;
    }
    Ok(())
}
