//! Local deterministic stdio peer. Worker D may extend process lifecycle modes.
//! No external interpreter, package registry or real service is required.
use serde_json::Value;
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
};
fn main() {
    if run().is_err() {
        eprintln!("MCP fixture protocol mismatch");
        std::process::exit(2);
    }
}
fn run() -> Result<(), ()> {
    let path = PathBuf::from(std::env::args_os().nth(1).ok_or(())?);
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
            }
            "receive" => {
                serde_json::to_writer(&mut output, &step["value"]).map_err(|_| ())?;
                output.write_all(b"\n").map_err(|_| ())?;
                output.flush().map_err(|_| ())?;
            }
            "close" => return Ok(()),
            _ => return Err(()),
        }
    }
    Err(())
}
