//! Deterministic external CLI double: strict upstream protocol and explicit IPC.
use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

fn ready(socket: &str) -> UnixStream {
    let mut stream = UnixStream::connect(socket).unwrap();
    writeln!(stream, "{}", std::process::id()).unwrap();
    stream
}

fn block(stream: &mut UnixStream) {
    let mut byte = [0];
    stream.read_exact(&mut byte).unwrap();
}

fn comments(content: &str) -> HashSet<String> {
    // Only line comments are needed by these protocol tests. Extraction quality
    // belongs to the external CLI; this models its old/new normalized set rule.
    content
        .lines()
        .filter_map(|line| line.split_once("//"))
        .map(|(_, comment)| comment.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--descendant"] {
        let mut stream = ready("checker.child.sock");
        block(&mut stream);
        panic!("descendant must be stopped by the supervisor");
    }
    assert_eq!(args.len(), 3);
    assert_eq!(&args[..2], ["check", "--prompt"]);
    std::fs::write("checker.args", format!("{}\n", args.join("\n"))).unwrap();
    std::fs::write(
        "checker.cwd",
        format!("{}\n", std::env::current_dir().unwrap().display()),
    )
    .unwrap();
    let mode = args[2].as_str();
    if matches!(mode, "timeout" | "unread-input" | "descendant-pipe") {
        // This fixture deliberately exits without reaping its descendant: the
        // checker supervisor must stop a pipe-holding child after leader exit.
        #[expect(
            clippy::zombie_processes,
            reason = "intentional orphan cleanup fixture"
        )]
        let _child = Command::new(std::env::current_exe().unwrap())
            .arg("--descendant")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut stream = ready("checker.ready.sock");
        block(&mut stream);
        if mode == "descendant-pipe" {
            return;
        }
        panic!("checker must be stopped by the supervisor");
    }
    if mode == "large-output" {
        // Emit more than both pipe capacities before reading a large input.
        let block = [b'x'; 8192];
        for _ in 0..128 {
            std::io::stdout().write_all(&block).unwrap();
            std::io::stderr().write_all(&block).unwrap();
        }
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let value: serde_json::Value = serde_json::from_str(&input).unwrap();
    let root = value.as_object().unwrap();
    assert_eq!(root.len(), 2);
    let tool = root["tool_name"].as_str().unwrap();
    let fields = root["tool_input"].as_object().unwrap();
    assert!(fields["file_path"].is_string());
    let pairs: Vec<(&str, &str)> = match tool {
        "Write" => {
            assert_eq!(fields.len(), 2);
            vec![("", fields["content"].as_str().unwrap())]
        }
        "Edit" => {
            assert_eq!(fields.len(), 3);
            vec![(
                fields["old_string"].as_str().unwrap(),
                fields["new_string"].as_str().unwrap(),
            )]
        }
        // Upstream v0.8.2 checks each edit's old/new pair separately.
        "MultiEdit" => {
            assert_eq!(fields.len(), 2);
            let edits = fields["edits"].as_array().unwrap();
            assert!(!edits.is_empty());
            edits
                .iter()
                .map(|edit| {
                    let edit = edit.as_object().unwrap();
                    assert_eq!(edit.len(), 2);
                    (
                        edit["old_string"].as_str().unwrap(),
                        edit["new_string"].as_str().unwrap(),
                    )
                })
                .collect()
        }
        _ => panic!("upstream tool_name is case-sensitive: {tool}"),
    };
    std::fs::write("checker.input", &input).unwrap();
    writeln!(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("checker.calls")
            .unwrap(),
        "called"
    )
    .unwrap();
    if mode == "abnormal" {
        eprintln!("unexpected failure");
        std::process::exit(1);
    }
    if mode == "large-output" {
        std::process::exit(2);
    }
    let added = pairs
        .iter()
        .any(|(old, new)| comments(new).difference(&comments(old)).next().is_some());
    let fixture_warning = pairs.iter().any(|(_, new)| new.contains("fixture-warning"));
    if mode == "warning" || (mode == "comments" && added) || fixture_warning {
        eprintln!("  explain why, not what  ");
        std::process::exit(2);
    }
    println!("ignored stdout");
}
