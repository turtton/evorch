//! Real-process rig shared by the launcher process tests: every transition is
//! acknowledged over a control socket and pidfds give terminal events.
#![allow(dead_code)]
use core_instance::ConfigIdentity;
use core_launcher::{CoreBinary, Launcher};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal},
};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::{
        fd::OwnedFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command},
};

pub fn binary() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_core-launch-harness")
        .unwrap()
        .into()
}
pub fn private(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
pub struct ChildGuard(pub Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub struct Peer {
    pub io: BufReader<UnixStream>,
    pub event: serde_json::Value,
    pub fd: OwnedFd,
    pub core: bool,
}
impl Peer {
    pub fn new(stream: UnixStream) -> Self {
        // A lost acknowledgement fails the test within a minute instead of hanging.
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(60)))
            .unwrap();
        let mut io = BufReader::new(stream);
        let mut line = String::new();
        assert_ne!(io.read_line(&mut line).unwrap(), 0);
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        let fd = pidfd_open(
            Pid::from_raw(event["pid"].as_u64().unwrap() as i32).unwrap(),
            PidfdFlags::empty(),
        )
        .unwrap();
        Self {
            io,
            event,
            fd,
            core: false,
        }
    }
    pub fn expect(&mut self, stage: &str) {
        assert_eq!(self.event["stage"], stage, "{:?}", self.event);
        if stage == "adopted" || stage == "inherited" {
            self.core = true;
        }
    }
    pub fn ack(&mut self) {
        self.io.get_mut().write_all(b"go\n").unwrap();
    }
    pub fn receive(&mut self) {
        let mut line = String::new();
        assert_ne!(
            self.io.read_line(&mut line).unwrap(),
            0,
            "control peer exited"
        );
        self.event = serde_json::from_str(&line).unwrap();
    }
    pub fn next(&mut self, stage: &str) {
        self.receive();
        self.expect(stage);
    }
    pub fn kill(&mut self) {
        let _ = pidfd_send_signal(&self.fd, Signal::KILL);
        poll(&mut [PollFd::new(&self.fd, PollFlags::IN)], None).unwrap();
        self.core = false;
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if self.core {
            self.kill();
        }
    }
}
pub struct Rig {
    pub root: test_socket_dir::SocketDir,
    pub config: PathBuf,
    pub runtime: PathBuf,
    pub control: UnixListener,
}
impl Rig {
    pub fn new() -> Self {
        let root = test_socket_dir::SocketDir::new();
        let config = root.path().join("cfg");
        private(&config);
        let runtime = root.path().join("run");
        private(&runtime);
        let path = root.path().join("ctl");
        let control = UnixListener::bind(&path).unwrap();
        fs::write(config.join("control"), path.as_os_str().as_encoded_bytes()).unwrap();
        fs::write(
            config.join("runtime"),
            runtime.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        Self {
            root,
            config,
            runtime,
            control,
        }
    }
    pub fn accept(&self, stage: &str) -> Peer {
        let limit = Timespec {
            tv_sec: 60,
            tv_nsec: 0,
        };
        let ready = poll(
            &mut [PollFd::new(&self.control, PollFlags::IN)],
            Some(&limit),
        )
        .unwrap();
        assert_eq!(ready, 1, "no process reported {stage}");
        let mut p = Peer::new(self.control.accept().unwrap().0);
        p.expect(stage);
        p
    }
    pub fn client(&self, config: &Path) -> (ChildGuard, Peer) {
        let child = ChildGuard(
            Command::new(binary())
                .arg("--client")
                .arg("--config-dir")
                .arg(config)
                .spawn()
                .unwrap(),
        );
        let mut peer = self.accept("client-start");
        peer.ack();
        (child, peer)
    }
    pub fn spawn(&self, parent: &mut Peer) -> Peer {
        parent.next("before-spawn");
        parent.ack();
        parent.next("spawned");
        let mut core = self.accept("inherited");
        core.ack();
        core.next("adopted");
        core
    }
    pub fn until_before_ready(&self, core: &mut Peer) {
        core.ack();
        let mut scan = self.accept("scan-clean");
        scan.ack();
        core.next("before-ready");
    }
    pub fn ready(&self, core: &mut Peer) {
        self.until_before_ready(core);
        core.ack();
        core.next("ready");
        core.ack();
    }
    pub fn identity(&self) -> ConfigIdentity {
        ConfigIdentity::new(&self.config).unwrap()
    }
}

pub fn local_launcher(r: &Rig) -> Launcher {
    let id = r.identity();
    let cc = workbench_client::ClientConfig::new(
        workbench_protocol::ids::ClientId::new("local-test").unwrap(),
        workbench_protocol::handshake::BuildInfo {
            name: "test".into(),
            version: "1".into(),
            revision: None,
        },
    );
    Launcher::with_socket(
        id.clone(),
        CoreBinary::explicit(&binary()).unwrap(),
        cc,
        id.socket(&r.runtime).unwrap(),
    )
    .unwrap()
    .diagnostic_after(std::time::Duration::MAX)
}
