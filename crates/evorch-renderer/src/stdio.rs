//! Taking the IPC channels from the standard streams.
//!
//! evorch starts children so that they inherit only stdin, stdout and stderr,
//! so the control channel arrives as stdin and the frame channel as stdout
//! (both Unix sockets). They are duplicated onto private close-on-exec
//! descriptors and the standard streams are then pointed elsewhere: CEF's own
//! processes, and anything that prints, can never read or write a channel.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

use rustix::fs::{FileType, fstat};

pub struct Channels {
    pub control: UnixStream,
    pub frames: UnixStream,
}

fn adopt(fd: BorrowedFd<'_>, name: &str) -> io::Result<UnixStream> {
    let owned: OwnedFd = fd.try_clone_to_owned()?;
    let kind = FileType::from_raw_mode(fstat(&owned)?.st_mode);
    if kind != FileType::Socket {
        return Err(io::Error::other(format!("{name} is not a socket")));
    }
    Ok(UnixStream::from(owned))
}

pub fn take_channels() -> io::Result<Channels> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let control = adopt(stdin.as_fd(), "stdin (control channel)")?;
    let frames = adopt(stdout.as_fd(), "stdout (frame channel)")?;
    let null = File::options().read(true).write(true).open("/dev/null")?;
    rustix::stdio::dup2_stdin(&null)?;
    rustix::stdio::dup2_stdout(std::io::stderr().as_fd())?;
    Ok(Channels { control, frames })
}
