//! Diagnostic/lifetime leaf units, not production-composition evidence.
use super::test_clock::{self, TestClock};
use std::{
    ffi::{c_char, c_int},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::net::UnixStream,
    },
    process::{Child, Command, Stdio},
};
unsafe extern "C" {
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn posix_openpt(flags: c_int) -> c_int;
    fn grantpt(fd: c_int) -> c_int;
    fn unlockpt(fd: c_int) -> c_int;
    fn ptsname_r(fd: c_int, name: *mut c_char, len: usize) -> c_int;
}
fn flags(fd: &impl AsRawFd) -> i32 {
    let flags = unsafe { fcntl(fd.as_raw_fd(), 3) };
    assert!(flags >= 0);
    flags
}
fn nonblock(fd: &impl AsRawFd, enabled: bool) {
    let old = flags(fd);
    let bit = if cfg!(any(
        target_arch = "mips",
        target_arch = "mips32r6",
        target_arch = "mips64",
        target_arch = "mips64r6"
    )) {
        128
    } else if cfg!(any(target_arch = "sparc", target_arch = "sparc64")) {
        0x4000
    } else {
        0x800
    };
    assert_eq!(
        unsafe {
            fcntl(
                fd.as_raw_fd(),
                4,
                if enabled { old | bit } else { old & !bit },
            )
        },
        0
    );
}
fn fill(writer: &mut (impl Write + AsRawFd)) {
    nonblock(writer, true);
    for _ in 0..1024 {
        match writer.write(&[b'x'; 16384]) {
            Ok(n) => assert!(n > 0),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                nonblock(writer, false);
                return;
            }
            Err(error) => panic!("fill: {error}"),
        }
    }
    panic!("sink unexpectedly accepted over 16 MiB");
}
struct Reap(Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn child(case: &str, stderr: Stdio) -> Reap {
    Reap(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "restart::shutdown_watchdog_tests::sink_tests::sink_child",
                "--ignored",
                "--nocapture",
            ])
            .env("EVORCH_MASKED_TEST_CHILD", "1")
            .env("EVORCH_WATCHDOG_SINK", case)
            .env("TOKIO_WORKER_THREADS", "2")
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap(),
    )
}
fn expired(child: &mut Reap) {
    assert_eq!(child.0.wait().unwrap().code(), Some(74));
}
#[test]
fn diagnostic_pipe_full_and_disconnected_never_block_or_change_stderr_flags() {
    for case in ["pipe", "full", "disconnected"] {
        let (reader, mut writer) = io::pipe().unwrap();
        if case == "full" {
            fill(&mut writer);
        }
        let before = flags(&writer);
        let mut reader = if case == "disconnected" {
            drop(reader);
            None
        } else {
            Some(reader)
        };
        let mut process = child(case, writer.try_clone().unwrap().into());
        expired(&mut process);
        assert_eq!(flags(&writer), before, "{case} changed original OFD flags");
        drop(writer);
        if let Some(reader) = &mut reader {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            let diagnostic =
                String::from_utf8_lossy(&bytes).contains("planned shutdown exceeded 10s");
            assert_eq!(diagnostic, case == "pipe");
        }
    }
}
#[test]
fn diagnostic_socket_full_and_disconnected_never_block_or_sigpipe() {
    for case in ["socket", "full", "disconnected"] {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        if case == "full" {
            fill(&mut writer);
        }
        let before = flags(&writer);
        let mut reader = if case == "disconnected" {
            drop(reader);
            None
        } else {
            Some(reader)
        };
        let stderr: std::os::fd::OwnedFd = writer.try_clone().unwrap().into();
        let mut process = child(case, stderr.into());
        expired(&mut process);
        assert_eq!(flags(&writer), before);
        drop(writer);
        if let Some(reader) = &mut reader {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(
                String::from_utf8_lossy(&bytes).contains("planned shutdown exceeded 10s"),
                case == "socket"
            );
        }
    }
}
#[test]
fn diagnostic_omits_regular_tty_and_closed_stderr() {
    let file = tempfile::tempfile().unwrap();
    let before = flags(&file);
    let mut process = child("file", file.try_clone().unwrap().into());
    expired(&mut process);
    assert_eq!(file.metadata().unwrap().len(), 0);
    assert_eq!(flags(&file), before);
    let fd = unsafe { posix_openpt(2 | 0x100 | 0x80000) };
    assert!(fd >= 0, "posix_openpt: {}", io::Error::last_os_error());
    let mut master = unsafe { File::from_raw_fd(fd) };
    assert_eq!(unsafe { grantpt(fd) }, 0);
    assert_eq!(unsafe { unlockpt(fd) }, 0);
    let mut name = [0 as c_char; 128];
    assert_eq!(unsafe { ptsname_r(fd, name.as_mut_ptr(), name.len()) }, 0);
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_str()
        .unwrap();
    let slave = OpenOptions::new().write(true).open(name).unwrap();
    let before = flags(&slave);
    let mut process = child("tty", slave.try_clone().unwrap().into());
    expired(&mut process);
    assert_eq!(flags(&slave), before);
    nonblock(&master, true);
    assert_eq!(
        master.read(&mut [0u8; 64]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(slave);
    let mut process = child("closed", Stdio::null());
    expired(&mut process);
}
#[test]
#[ignore = "subprocess-only diagnostic unit: deliberately _exit(74)"]
fn sink_child() {
    let mask = crate::signal_test_entry::mask();
    let inbox = mask.inbox().unwrap();
    // Close only after obtaining the real inbox, so no fake fd can reuse 2.
    if std::env::var("EVORCH_WATCHDOG_SINK").unwrap() == "closed" {
        assert_eq!(unsafe { close(2) }, 0);
    }
    let clock = TestClock::default();
    let controlled = clock.clone();
    let _: io::Result<()> = clock
        .run_with_inbox((), inbox, |(), _, _inbox, observer| {
            observer.capture_started();
            test_clock::signal(&observer);
            assert_eq!(test_clock::deadline(&observer), 10_000_000_000);
            controlled.advance_to(10_000_000_000, &observer);
            loop {
                std::thread::park();
            }
        })
        .unwrap_or_else(|failure| panic!("setup: {}", failure.error));
    panic!("expiry returned");
}
