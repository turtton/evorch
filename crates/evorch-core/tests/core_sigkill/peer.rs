//! External dependency fixture, not a substitute for the caller inside core.
use std::{
    io::{self, BufRead, Write},
    process::{Command, Stdio},
};
pub fn run(args: &[String]) -> io::Result<()> {
    if args[0] == "leaf" {
        if args.get(2).is_some_and(|arg| arg == "escape") {
            rustix::process::setsid()?;
        }
        println!("READY");
        io::stdout().flush()?;
    } else {
        let mut command = Command::new(std::env::current_exe()?);
        command.args(["--peer", "leaf", &args[1]]);
        if args.get(2).is_some_and(|arg| arg == "escape") {
            command.arg("escape");
        }
        let mut child = command.stdout(Stdio::piped()).spawn()?;
        let mut ready = String::new();
        io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready)?;
        if ready != "READY\n" {
            return Err(io::Error::other("leaf failed before READY"));
        }
        // A line on a private socket, never the caller's protocol/stdio.
        let mut control = std::os::unix::net::UnixStream::connect(&args[1])?;
        control.write_all(b"READY\n")?;
        io::Write::flush(&mut control)?;
        // Losing stdin or the control socket must NOT naturally end this peer.
        loop {
            std::thread::park();
        }
    }
    loop {
        std::thread::park();
    }
}
