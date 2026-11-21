use super::*;
use std::os::{
    fd::{FromRawFd, OwnedFd},
    unix::{
        fs::MetadataExt,
        net::{UnixListener, UnixStream},
    },
};

pub(super) fn controller(test: &str, scenario: &str) -> bool {
    if std::env::var("PTY_CONTROLLER").as_deref() == Ok(scenario) {
        return true;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("PTY_CONTROLLER", scenario)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "controller {scenario}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}
pub(super) struct Reaper(Vec<(i32, OwnedFd)>);
impl Reaper {
    pub(super) fn new() -> Self {
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        Self(Vec::new())
    }
    pub(super) fn add(&mut self, pid: i32) {
        self.0.push((pid, pidfd_open(pid).unwrap()));
    }
}
fn kill(fd: &impl AsRawFd) {
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        );
    }
}
fn reap(pid: i32) {
    loop {
        let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        if result >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            break;
        }
    }
}
impl Drop for Reaper {
    fn drop(&mut self) {
        for (_, fd) in &self.0 {
            kill(fd);
        }
        for (pid, _) in &self.0 {
            reap(*pid);
        }
        // Only this isolated controller's children; also recover partial setup.
        loop {
            let mut pids = std::collections::BTreeSet::new();
            for task in std::fs::read_dir("/proc/self/task").unwrap() {
                let path = task.unwrap().path().join("children");
                if let Ok(text) = std::fs::read_to_string(path) {
                    pids.extend(text.split_whitespace().map(|s| s.parse::<i32>().unwrap()));
                }
            }
            if pids.is_empty() {
                break;
            }
            for pid in pids {
                if let Ok(fd) = pidfd_open(pid) {
                    kill(&fd);
                }
                reap(pid);
            }
        }
    }
}
#[test]
fn wait_force_drop_retire_root_and_same_group_descendant() {
    let Ok(action) = std::env::var("PTY_CONTROLLER") else {
        for action in ["wait", "force", "drop"] {
            controller(
                "ownership::wait_force_drop_retire_root_and_same_group_descendant",
                action,
            );
        }
        return;
    };
    let mut reaper = Reaper::new();
    let target = Target::new();
    let (mut child, master) = target.spawn("group");
    reaper.add(child.id() as i32);
    reaper.add(child.target_id() as i32);
    let mut output = BufReader::new(master.try_clone_reader().unwrap());
    let mut input = master.take_writer().unwrap();
    let descendant = state(&until(&mut output, "DESC "), "DESC", child.id());
    reaper.add(descendant);
    let death = pidfd_open(descendant).unwrap();
    state(&until(&mut output, "READY "), "READY", child.id());
    match action.as_str() {
        "wait" => {
            input.write_all(b"EXIT\n").unwrap();
            assert_eq!(child.exited().unwrap().code(), Some(37));
            let mut fds = [wire::fd(death.as_raw_fd(), libc::POLLIN)];
            assert_eq!(
                unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) },
                0,
                "exited() must not kill the surviving group"
            );
            child.request_wait().unwrap();
            assert_eq!(child.wait().unwrap().code(), Some(37));
        }
        "force" => {
            child.force_kill().unwrap();
            assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        }
        "drop" => drop(child),
        _ => unreachable!(),
    }
    dead(&death);
}

fn transfer(socket: &UnixStream, fd: i32) {
    unsafe {
        let mut byte = 0u8;
        let mut iov = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(4) as usize;
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(4) as usize;
        std::ptr::write(libc::CMSG_DATA(header).cast::<i32>(), fd);
        assert_eq!(
            libc::sendmsg(socket.as_raw_fd(), &msg, libc::MSG_NOSIGNAL),
            1
        );
    }
}
fn receive(socket: &UnixStream) -> OwnedFd {
    unsafe {
        let mut byte = 0u8;
        let mut iov = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control);
        assert_eq!(
            libc::recvmsg(socket.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC),
            1
        );
        let header = libc::CMSG_FIRSTHDR(&msg);
        assert!(!header.is_null());
        assert_eq!((*header).cmsg_type, libc::SCM_RIGHTS);
        OwnedFd::from_raw_fd(std::ptr::read(libc::CMSG_DATA(header).cast::<i32>()))
    }
}

fn master_for(slave: &std::fs::Metadata) -> OwnedFd {
    for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
        let fd = entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .parse::<i32>()
            .unwrap();
        // Own the probe before inspecting it: an enumerated descriptor may
        // disappear/reuse its number, but the duplicated file stays stable.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            continue;
        }
        let master = unsafe { OwnedFd::from_raw_fd(duplicate) };
        let peer = unsafe {
            libc::ioctl(
                master.as_raw_fd(),
                libc::TIOCGPTPEER,
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            )
        };
        if peer < 0 {
            continue;
        }
        let peer = unsafe { std::fs::File::from_raw_fd(peer) };
        let actual = peer.metadata().unwrap();
        if (actual.dev(), actual.ino(), actual.rdev()) == (slave.dev(), slave.ino(), slave.rdev()) {
            return master;
        }
    }
    panic!("PTY master matching the target slave was not found");
}

#[test]
fn master_discovery_matches_slave_identity_not_another_pty() {
    let system = portable_pty::native_pty_system();
    let first = system.openpty(size()).unwrap();
    let second = system.openpty(size()).unwrap();
    let mut first_number = 0u32;
    let mut expected = 0u32;
    let second_fd = second.master.as_raw_fd().unwrap();
    unsafe {
        assert_eq!(
            libc::ioctl(
                first.master.as_raw_fd().unwrap(),
                libc::TIOCGPTN,
                &mut first_number
            ),
            0
        );
        assert_eq!(libc::ioctl(second_fd, libc::TIOCGPTN, &mut expected), 0);
    }
    assert_ne!(first_number, expected);
    let peer = unsafe {
        libc::ioctl(
            second_fd,
            libc::TIOCGPTPEER,
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    assert!(peer >= 0);
    let peer = unsafe { std::fs::File::from_raw_fd(peer) };
    let master = master_for(&peer.metadata().unwrap());
    let mut actual = 0u32;
    assert_eq!(
        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGPTN, &mut actual) },
        0
    );
    assert_eq!(actual, expected, "must retain this PTY, not another master");
}

#[test]
fn core_fixture() {
    let Some(socket) = std::env::var_os("PTY_CORE_SOCKET") else {
        return;
    };
    let mut controller = UnixStream::connect(socket).unwrap();
    let target = Target::new();
    let (child, master) = target.spawn("group");
    let mut output = BufReader::new(master.try_clone_reader().unwrap());
    let descendant = state(&until(&mut output, "DESC "), "DESC", child.id());
    until(&mut output, "READY ");
    transfer(&controller, child.control().as_raw_fd());
    // Keep the real master open in the outer controller. Otherwise core death
    // would pass from terminal HUP, independently of the owner pidfd monitor.
    // Match the kernel slave identity, not a /dev/ptmx vs /dev/pts/ptmx alias.
    // This test-only inspection is not a new raw-fd owner API.
    let slave = std::fs::metadata(format!("/proc/{}/fd/0", child.target_id())).unwrap();
    let retained_master = master_for(&slave);
    transfer(&controller, retained_master.as_raw_fd());
    writeln!(
        controller,
        "{} {} {descendant}",
        child.id(),
        child.target_id()
    )
    .unwrap();
    let mut byte = [0u8];
    // The outer controller kills us, retaining *both* possible EOF triggers.
    let _ = controller.read(&mut byte);
}
#[test]
fn core_death_is_not_control_eof_or_terminal_hup() {
    if !controller(
        "ownership::core_death_is_not_control_eof_or_terminal_hup",
        "core-death",
    ) {
        return;
    }
    let mut reaper = Reaper::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("controller.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let mut core = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ownership::core_fixture",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PTY_CORE_SOCKET", path)
        .spawn()
        .unwrap();
    reaper.add(core.id() as i32);
    let (mut socket, _) = listener.accept().unwrap();
    let control = receive(&socket);
    let _master = receive(&socket);
    let mut text = Vec::new();
    loop {
        let mut byte = [0u8];
        socket.read_exact(&mut byte).unwrap();
        if byte[0] == b'\n' {
            break;
        }
        text.push(byte[0]);
    }
    let ids: Vec<i32> = String::from_utf8(text)
        .unwrap()
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    for &pid in &ids {
        reaper.add(pid);
    }
    let deaths: Vec<_> = ids.iter().map(|&pid| pidfd_open(pid).unwrap()).collect();
    eprintln!("PTY_READY helper/root/descendant={ids:?}; control and master retained");
    core.kill().unwrap();
    assert_eq!(core.wait().unwrap().signal(), Some(libc::SIGKILL));
    let mut control = UnixStream::from(control);
    control.set_nonblocking(true).unwrap();
    // With owner-death monitoring removed, a live guard ACKs this harmless
    // signal. Production must notice the already-dead owner before that.
    let _ = wire::send(
        &mut control,
        wire::Kind::Signal,
        &libc::SIGCONT.to_le_bytes(),
        None,
    );
    let reply = wire::receive(
        &mut control,
        &mut wire::Decoder::default(),
        deaths[0].as_raw_fd(),
        None,
    );
    assert!(reply.is_err(), "guard survived owner death: {reply:?}");
    for fd in &deaths {
        dead(fd);
    }
}
