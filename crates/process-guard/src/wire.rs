//! ABI 1. Fixed 16-byte header; bounded, incrementally decoded frames.
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Instant;

pub const ABI: u16 = 1;
pub const FRAME_LIMIT: usize = 64 * 1024;
pub const LAUNCH_LIMIT: usize = 16 * 1024 * 1024;
pub const QUEUE_LIMIT: usize = 256 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    Hello = 1,
    LaunchChunk,
    LaunchEnd,
    Ready,
    Commit,
    ExecOk,
    SpawnError,
    RootExited,
    Wait,
    TryReap,
    Running,
    Signal,
    Ack,
    FinalStatus,
}
impl TryFrom<u16> for Kind {
    type Error = io::Error;
    fn try_from(n: u16) -> io::Result<Self> {
        Ok(match n {
            1 => Self::Hello,
            2 => Self::LaunchChunk,
            3 => Self::LaunchEnd,
            4 => Self::Ready,
            5 => Self::Commit,
            6 => Self::ExecOk,
            7 => Self::SpawnError,
            8 => Self::RootExited,
            9 => Self::Wait,
            10 => Self::TryReap,
            11 => Self::Running,
            12 => Self::Signal,
            13 => Self::Ack,
            14 => Self::FinalStatus,
            _ => return Err(invalid("unknown guard message")),
        })
    }
}
pub fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
pub fn frame(kind: Kind, payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.len() > FRAME_LIMIT {
        return Err(invalid("guard frame too large"));
    }
    let mut b = Vec::with_capacity(16 + payload.len());
    b.extend_from_slice(b"EVOG");
    b.extend_from_slice(&ABI.to_le_bytes());
    b.extend_from_slice(&(kind as u16).to_le_bytes());
    b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(payload);
    Ok(b)
}
#[derive(Default, Debug)]
pub struct Decoder {
    bytes: Vec<u8>,
}
impl Decoder {
    pub fn push(&mut self, b: &[u8]) -> io::Result<()> {
        if self.bytes.len() + b.len() > FRAME_LIMIT * 2 + 32 {
            return Err(invalid("guard input overflow"));
        }
        self.bytes.extend_from_slice(b);
        Ok(())
    }
    pub fn next_frame(&mut self) -> io::Result<Option<(Kind, Vec<u8>)>> {
        if self.bytes.len() < 16 {
            return Ok(None);
        }
        let h = &self.bytes[..16];
        if &h[..4] != b"EVOG"
            || u16::from_le_bytes(h[4..6].try_into().unwrap()) != ABI
            || h[12..16] != [0; 4]
        {
            return Err(invalid("guard header/ABI mismatch"));
        }
        let kind = Kind::try_from(u16::from_le_bytes(h[6..8].try_into().unwrap()))?;
        let len = u32::from_le_bytes(h[8..12].try_into().unwrap()) as usize;
        if len > FRAME_LIMIT {
            return Err(invalid("guard frame length"));
        }
        if self.bytes.len() < len + 16 {
            return Ok(None);
        }
        let p = self.bytes[16..len + 16].to_vec();
        self.bytes.drain(..len + 16);
        Ok(Some((kind, p)))
    }
    pub fn read(&mut self, s: &mut UnixStream) -> io::Result<usize> {
        let mut b = [0; 8192];
        let n = s.read(&mut b)?;
        self.push(&b[..n])?;
        Ok(n)
    }
}
pub fn poll(fds: &mut [libc::pollfd], deadline: Option<Instant>) -> io::Result<()> {
    loop {
        let ms = match deadline {
            None => -1,
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "guard handshake exceeded absolute 5s deadline",
                    ));
                }
                left.as_millis().clamp(1, i32::MAX as u128) as i32
            }
        };
        let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, ms) };
        if r > 0 {
            return Ok(());
        }
        if r == 0 {
            continue;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}
pub fn fd(n: i32, events: i16) -> libc::pollfd {
    libc::pollfd {
        fd: n,
        events,
        revents: 0,
    }
}
pub fn send(s: &mut UnixStream, kind: Kind, p: &[u8], deadline: Option<Instant>) -> io::Result<()> {
    let bytes = frame(kind, p)?;
    let mut at = 0;
    while at < bytes.len() {
        if let Some(d) = deadline
            && Instant::now() >= d
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "guard handshake deadline",
            ));
        }
        match s.write(&bytes[at..]) {
            Ok(0) => return Err(invalid("guard write zero")),
            Ok(n) => at += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                poll(&mut [fd(s.as_raw_fd(), libc::POLLOUT)], deadline)?
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
/// No poll/flush on the control-request path. An incomplete frame invalidates
/// the stream; the owner must half-close rather than retrying another request.
pub fn try_send(s: &mut UnixStream, kind: Kind, p: &[u8]) -> io::Result<()> {
    let bytes = frame(kind, p)?;
    let mut at = 0;
    while at < bytes.len() {
        match s.write(&bytes[at..]) {
            Ok(0) => return Err(invalid("guard write zero")),
            Ok(n) => at += n,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
/// Consume only immediately available bytes. A readable death fd triggers one
/// last stream read: poll's per-fd readiness snapshot is not atomic, and a
/// FinalStatus may arrive between the stream check and the death check.
pub fn try_receive(
    s: &mut UnixStream,
    d: &mut Decoder,
    death: i32,
) -> io::Result<Option<(Kind, Vec<u8>)>> {
    loop {
        if let Some(frame) = d.next_frame()? {
            return Ok(Some(frame));
        }
        match d.read(s) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "GuardLost: control EOF",
                ));
            }
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        let mut dead = fd(death, libc::POLLIN);
        if unsafe { libc::poll(&mut dead, 1, 0) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(error);
        }
        if dead.revents == 0 {
            return Ok(None);
        }
        match d.read(s) {
            Ok(n) if n > 0 => continue,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() != io::ErrorKind::WouldBlock => return Err(e),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "GuardLost: helper exited without FinalStatus",
                ));
            }
        }
    }
}
pub fn receive(
    s: &mut UnixStream,
    d: &mut Decoder,
    death: i32,
    deadline: Option<Instant>,
) -> io::Result<(Kind, Vec<u8>)> {
    loop {
        if let Some(limit) = deadline
            && Instant::now() >= limit
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "guard handshake deadline",
            ));
        }
        if let Some(f) = d.next_frame()? {
            return Ok(f);
        }
        let mut fds = [fd(s.as_raw_fd(), libc::POLLIN), fd(death, libc::POLLIN)];
        poll(&mut fds, deadline)?;
        // Drain an already-sent FinalStatus before diagnosing helper death.
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 || fds[1].revents != 0 {
            match d.read(s) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "GuardLost: control EOF",
                    ));
                }
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if fds[1].revents != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "GuardLost: pidfd readable",
            ));
        }
    }
}
pub fn integer(p: &[u8]) -> io::Result<i32> {
    Ok(i32::from_le_bytes(
        p.try_into().map_err(|_| invalid("guard integer length"))?,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nonblocking_receive_retains_partial_frames_and_rejects_missing_final() {
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let mut decoder = Decoder::default();
        assert!(
            try_receive(&mut receiver, &mut decoder, -1)
                .unwrap()
                .is_none()
        );
        let bytes = frame(Kind::FinalStatus, &37i32.to_le_bytes()).unwrap();
        sender.write_all(&bytes[..9]).unwrap();
        assert!(
            try_receive(&mut receiver, &mut decoder, -1)
                .unwrap()
                .is_none()
        );
        sender.write_all(&bytes[9..]).unwrap();
        drop(sender);
        assert_eq!(
            try_receive(&mut receiver, &mut decoder, -1).unwrap(),
            Some((Kind::FinalStatus, 37i32.to_le_bytes().to_vec()))
        );
        assert_eq!(
            try_receive(&mut receiver, &mut decoder, -1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }
    #[test]
    fn split_frames_and_bad_lengths_are_bounded() {
        let encoded = frame(Kind::LaunchChunk, &[1, 2, 3]).unwrap();
        let mut d = Decoder::default();
        for b in &encoded[..encoded.len() - 1] {
            d.push(&[*b]).unwrap();
            assert!(d.next_frame().unwrap().is_none());
        }
        d.push(&encoded[encoded.len() - 1..]).unwrap();
        assert_eq!(
            d.next_frame().unwrap(),
            Some((Kind::LaunchChunk, vec![1, 2, 3]))
        );
        for (at, value) in [(4, 2u32), (8, FRAME_LIMIT as u32 + 1), (12, 1)] {
            let mut bad = encoded[..16].to_vec();
            bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
            let mut d = Decoder::default();
            d.push(&bad).unwrap();
            assert!(d.next_frame().is_err());
        }
        assert!(frame(Kind::LaunchChunk, &vec![0; FRAME_LIMIT + 1]).is_err());
        assert!(
            Decoder::default()
                .push(&vec![0; FRAME_LIMIT * 2 + 33])
                .is_err()
        );
    }
    #[test]
    fn expired_absolute_deadline_cannot_be_reset_by_buffered_progress() {
        let (mut s, _) = UnixStream::pair().unwrap();
        let mut d = Decoder::default();
        d.push(&frame(Kind::Ready, &[]).unwrap()).unwrap();
        let expired = Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            receive(&mut s, &mut d, -1, Some(expired))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            send(&mut s, Kind::Commit, &[], Some(expired))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
}
