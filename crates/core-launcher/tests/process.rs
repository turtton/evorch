//! Real exec/kill tests. Every transition is acknowledged over an IPC barrier;
//! pidfds provide terminal events even when a launcher dies and orphans its core.
use core_instance::{ConfigIdentity, Instance, InstanceError, LaunchGuard};
use core_launcher::{CoreBinary, LaunchError, Launcher};
use rustix::{
    event::{PollFd, PollFlags, poll},
    process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal},
};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::{
        fd::OwnedFd,
        unix::{
            fs::{MetadataExt, PermissionsExt, symlink},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command},
};

fn binary() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_core-launch-harness")
        .unwrap()
        .into()
}
fn private(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Peer {
    io: BufReader<UnixStream>,
    event: serde_json::Value,
    fd: OwnedFd,
    core: bool,
}
impl Peer {
    fn new(stream: UnixStream) -> Self {
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
    fn expect(&mut self, stage: &str) {
        assert_eq!(self.event["stage"], stage, "{:?}", self.event);
        if stage == "adopted" || stage == "inherited" {
            self.core = true;
        }
    }
    fn ack(&mut self) {
        self.io.get_mut().write_all(b"go\n").unwrap();
    }
    fn receive(&mut self) {
        let mut line = String::new();
        assert_ne!(
            self.io.read_line(&mut line).unwrap(),
            0,
            "control peer exited"
        );
        self.event = serde_json::from_str(&line).unwrap();
    }
    fn next(&mut self, stage: &str) {
        self.receive();
        self.expect(stage);
    }
    fn kill(&mut self) {
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
struct Rig {
    root: tempfile::TempDir,
    config: PathBuf,
    runtime: PathBuf,
    control: UnixListener,
}
impl Rig {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("p10-")
            .tempdir_in("/tmp")
            .unwrap();
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
    fn accept(&self, stage: &str) -> Peer {
        let mut p = Peer::new(self.control.accept().unwrap().0);
        p.expect(stage);
        p
    }
    fn client(&self, config: &Path) -> (ChildGuard, Peer) {
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
    fn spawn(&self, parent: &mut Peer) -> Peer {
        parent.next("before-spawn");
        parent.ack();
        parent.next("spawned");
        let mut core = self.accept("inherited");
        core.ack();
        core.next("adopted");
        core
    }
    fn until_before_ready(&self, core: &mut Peer) {
        core.ack();
        let mut scan = self.accept("scan-clean");
        scan.ack();
        core.next("before-ready");
    }
    fn ready(&self, core: &mut Peer) {
        self.until_before_ready(core);
        core.ack();
        core.next("ready");
        core.ack();
    }
    fn identity(&self) -> ConfigIdentity {
        ConfigIdentity::new(&self.config).unwrap()
    }
}

#[test]
fn two_simultaneous_clients_spawn_one_core_and_inherited_launch_reaches_ready() {
    let r = Rig::new();
    let mut a = ChildGuard(
        Command::new(binary())
            .arg("--client")
            .arg("--config-dir")
            .arg(&r.config)
            .spawn()
            .unwrap(),
    );
    let mut b = ChildGuard(
        Command::new(binary())
            .arg("--client")
            .arg("--config-dir")
            .arg(&r.config)
            .spawn()
            .unwrap(),
    );
    let mut pa = r.accept("client-start");
    let mut pb = r.accept("client-start");
    // Release both clients from the same startup barrier before either elects.
    pa.ack();
    pb.ack();
    pa.receive();
    pb.receive();
    if pa.event["stage"] == "waiting-launch" {
        std::mem::swap(&mut pa, &mut pb);
    }
    pa.expect("before-spawn");
    pb.expect("waiting-launch");
    pa.ack();
    pa.next("spawned");
    let mut core = r.accept("inherited");
    core.ack();
    core.next("adopted");
    assert!(matches!(
        LaunchGuard::acquire(&r.identity()),
        Err(InstanceError::AlreadyRunning)
    ));
    r.ready(&mut core);
    pa.ack();
    pb.ack();
    pa.next("connected");
    pb.next("connected");
    assert_eq!(pa.event["value"], pb.event["value"]);
    pa.ack();
    pb.ack();
    assert!(a.0.wait().unwrap().success());
    assert!(b.0.wait().unwrap().success());
    let launch = LaunchGuard::acquire(&r.identity()).unwrap();
    assert!(!launch.lifetime_available().unwrap());
}
#[test]
fn killing_launcher_before_spawn_allows_election_takeover() {
    let r = Rig::new();
    let (mut a, mut pa) = r.client(&r.config);
    pa.next("before-spawn");
    a.0.kill().unwrap();
    a.0.wait().unwrap();
    let (mut b, mut pb) = r.client(&r.config);
    let mut core = r.spawn(&mut pb);
    r.ready(&mut core);
    pb.ack();
    pb.next("connected");
    pb.ack();
    assert!(b.0.wait().unwrap().success());
}
#[test]
fn killing_launcher_after_spawn_or_before_ready_does_not_spawn_a_second_core() {
    for before_ready in [false, true] {
        let r = Rig::new();
        let (mut a, mut pa) = r.client(&r.config);
        let mut core = r.spawn(&mut pa);
        if before_ready {
            r.until_before_ready(&mut core);
        }
        a.0.kill().unwrap();
        a.0.wait().unwrap();
        let (mut b, mut pb) = r.client(&r.config);
        pb.next("waiting-launch");
        assert!(matches!(
            LaunchGuard::acquire(&r.identity()),
            Err(InstanceError::AlreadyRunning)
        ));
        if before_ready {
            core.ack();
            core.next("ready");
            core.ack();
        } else {
            r.ready(&mut core);
        }
        pb.ack();
        pb.next("connected");
        pb.ack();
        assert!(b.0.wait().unwrap().success());
    }
}
#[test]
fn stale_socket_pid_reuse_lock_files_alias_and_different_xdg_are_hints_only() {
    let r = Rig::new();
    // A live (reused) PID in obsolete files must not affect election, nor be unlinked.
    let start = r.config.join(".evorch-core.start");
    fs::write(&start, std::process::id().to_string()).unwrap();
    let inode = fs::metadata(&start).unwrap().ino();
    let obsolete = fs::File::open(&start).unwrap();
    rustix::fs::flock(&obsolete, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let (mut a, mut pa) = r.client(&r.config);
    let mut core = r.spawn(&mut pa);
    r.ready(&mut core);
    pa.ack();
    pa.next("connected");
    pa.ack();
    assert!(a.0.wait().unwrap().success());
    let first = r.identity().read_record().unwrap().unwrap();
    core.kill();
    assert!(first.socket.exists());
    fs::write(
        r.config.join(".evorch-core.lock"),
        std::process::id().to_string(),
    )
    .unwrap();
    let second_runtime = r.root.path().join("run2");
    private(&second_runtime);
    fs::write(
        r.config.join("runtime"),
        second_runtime.as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    let (mut b, mut pb) = r.client(&r.config);
    let mut core = r.spawn(&mut pb);
    r.ready(&mut core);
    pb.ack();
    pb.next("connected");
    pb.ack();
    assert!(b.0.wait().unwrap().success());
    let second = r.identity().read_record().unwrap().unwrap();
    assert_ne!(first.socket, second.socket);
    assert!(!first.socket.exists());
    let alias = r.root.path().join("alias");
    symlink(&r.config, &alias).unwrap();
    // The client computes a different standard endpoint but finds the saved live endpoint.
    fs::write(
        r.config.join("runtime"),
        r.runtime.as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    let (mut c, mut pc) = r.client(&alias);
    pc.next("connected");
    pc.ack();
    assert!(c.0.wait().unwrap().success());
    assert_eq!(fs::metadata(&start).unwrap().ino(), inode);
}
#[test]
fn separate_configs_have_separate_processes_and_lock_fds_do_not_leak_on_exec() {
    let a = Rig::new();
    let b = Rig::new();
    let (mut ca, mut pa) = a.client(&a.config);
    let mut acore = a.spawn(&mut pa);
    a.ready(&mut acore);
    let (mut cb, mut pb) = b.client(&b.config);
    let mut bcore = b.spawn(&mut pb);
    b.ready(&mut bcore);
    assert_ne!(acore.event["pid"], bcore.event["pid"]);
    pa.ack();
    pb.ack();
    pa.next("connected");
    pb.next("connected");
    pa.ack();
    pb.ack();
    assert!(ca.0.wait().unwrap().success());
    assert!(cb.0.wait().unwrap().success());
    let identity = a.identity();
    let _launch = LaunchGuard::acquire(&identity).unwrap();
    let mut scanner = ChildGuard(
        Command::new(binary())
            .arg("--scan")
            .arg("--config-dir")
            .arg(&a.config)
            .spawn()
            .unwrap(),
    );
    let mut scan = a.accept("scan-clean");
    scan.ack();
    assert!(scanner.0.wait().unwrap().success());
}

#[test]
fn unsafe_paths_are_refused_and_lock_inodes_are_preserved() {
    let r = Rig::new();
    let id = r.identity();
    let launch = LaunchGuard::acquire(&id).unwrap();
    let instance = Instance::acquire(&launch, &r.runtime).unwrap();
    let socket = instance.socket.clone();
    drop(instance);
    drop(launch);
    let lock_path = r.config.join(".evorch-core.lock");
    let inode = fs::metadata(&lock_path).unwrap().ino();
    for mode in [0o600, 0o644, 0o660] {
        fs::write(&socket, "not a socket").unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(mode)).unwrap();
        let launch = LaunchGuard::acquire(&id).unwrap();
        assert!(Instance::acquire(&launch, &r.runtime).is_err());
        fs::remove_file(&socket).unwrap();
    }
    symlink(r.root.path().join("victim"), &socket).unwrap();
    {
        let launch = LaunchGuard::acquire(&id).unwrap();
        assert!(Instance::acquire(&launch, &r.runtime).is_err());
    }
    assert!(socket.is_symlink());
    fs::remove_file(&socket).unwrap();
    assert_eq!(fs::metadata(&lock_path).unwrap().ino(), inode);
    fs::remove_file(&lock_path).unwrap();
    symlink(r.root.path().join("victim"), &lock_path).unwrap();
    let launch = LaunchGuard::acquire(&id).unwrap();
    assert!(launch.lifetime_available().is_err());
    assert!(lock_path.is_symlink());
    assert!(CoreBinary::explicit(Path::new("evorch-core")).is_err());
    let malicious = r.root.path().join("evorch-core");
    fs::write(&malicious, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&malicious, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(CoreBinary::explicit(&malicious).is_err());
    fs::set_permissions(&malicious, fs::Permissions::from_mode(0o700)).unwrap();
    let link = r.root.path().join("bin-link");
    symlink(&malicious, &link).unwrap();
    assert!(CoreBinary::explicit(&link).is_err());
}

#[tokio::test(start_paused = true)]
async fn hung_owner_is_diagnosed_without_stealing_locks() {
    let r = Rig::new();
    let identity = r.identity();
    let _launch = LaunchGuard::acquire(&identity).unwrap();
    let cc = workbench_client::ClientConfig::new(
        workbench_protocol::ids::ClientId::new("diagnostic").unwrap(),
        workbench_protocol::handshake::BuildInfo {
            name: "test".into(),
            version: "1".into(),
            revision: None,
        },
    );
    let launcher = Launcher::with_socket(
        identity.clone(),
        CoreBinary::explicit(&binary()).unwrap(),
        cc,
        identity.socket(&r.runtime).unwrap(),
    )
    .unwrap()
    .diagnostic_after(std::time::Duration::from_secs(1));
    // Tokio advances virtual time to the production diagnostic deadline.
    assert!(matches!(
        launcher.connect_or_start().await,
        Err(LaunchError::NotReady)
    ));
    assert!(matches!(
        LaunchGuard::acquire(&identity),
        Err(InstanceError::AlreadyRunning)
    ));
}

fn local_launcher(r: &Rig) -> Launcher {
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
#[test]
fn parent_can_die_before_child_adopts_the_inherited_open_file_description() {
    let r = Rig::new();
    let (mut a, mut pa) = r.client(&r.config);
    pa.next("before-spawn");
    pa.ack();
    pa.next("spawned");
    let mut core = r.accept("inherited");
    a.0.kill().unwrap();
    a.0.wait().unwrap();
    assert!(matches!(
        LaunchGuard::acquire(&r.identity()),
        Err(InstanceError::AlreadyRunning)
    ));
    core.ack();
    core.next("adopted");
    r.ready(&mut core);
    let (mut b, mut pb) = r.client(&r.config);
    pb.next("connected");
    pb.ack();
    assert!(b.0.wait().unwrap().success());
}
#[test]
fn process_readiness_rejects_wrong_identity_version_capability_epoch_and_state() {
    use serde_json::json;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for overrides in [
        json!({"core":"wrong-config"}),
        json!({"protocol_version":999}),
        json!({"capabilities":[]}),
        json!({"instance_epoch":"0"}),
        json!({"status":{"state":"starting","retention_reasons":[],"running":{"turns":0,"tool_calls":0,"waiting_lanes":0,"shell_jobs":0,"clients":0}}}),
    ] {
        let r = Rig::new();
        fs::write(r.config.join("hello.json"), overrides.to_string()).unwrap();
        let mut process = ChildGuard(
            Command::new(binary())
                .arg("--config-dir")
                .arg(&r.config)
                .spawn()
                .unwrap(),
        );
        let mut core = r.accept("inherited");
        core.ack();
        core.next("adopted");
        r.ready(&mut core);
        assert!(matches!(
            rt.block_on(local_launcher(&r).connect_or_start()),
            Err(LaunchError::Readiness(_))
        ));
        let launch = LaunchGuard::acquire(&r.identity()).unwrap();
        assert!(!launch.lifetime_available().unwrap());
        core.kill();
        let _ = process.0.wait();
    }
}
#[test]
fn absent_or_stale_endpoint_record_does_not_replace_a_live_standard_endpoint() {
    let r = Rig::new();
    let (mut a, mut pa) = r.client(&r.config);
    let mut core = r.spawn(&mut pa);
    r.ready(&mut core);
    pa.ack();
    pa.next("connected");
    pa.ack();
    assert!(a.0.wait().unwrap().success());
    let record = r.identity().read_record().unwrap().unwrap();
    fs::remove_file(r.identity().record_path()).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = rt
        .block_on(
            local_launcher(&r).connect_or_start_observed(|_| panic!("live core must not launch")),
        )
        .unwrap();
    assert_eq!(client.server_hello().instance_epoch.0, record.epoch);
    drop(client);
    let mut record = record;
    record.epoch = record.epoch.wrapping_add(1).max(1);
    fs::write(
        r.identity().record_path(),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    fs::set_permissions(
        r.identity().record_path(),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(matches!(
        rt.block_on(local_launcher(&r).connect_or_start()),
        Err(LaunchError::Readiness(_))
    ));
}

#[test]
fn child_exit_two_reconnects_to_existing_owner_without_respawning() {
    use std::io::Read;
    use workbench_protocol::{handshake::*, ids::CoreIdentity, wire::InstanceEpoch};
    let r = Rig::new();
    let (mut a, mut pa) = r.client(&r.config);
    pa.next("before-spawn");
    // Model a pre-arbitration owner racing the lifetime probe. New owners never
    // do this: raw flock here is deliberately outside the LaunchGuard API.
    let lifetime = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(r.config.join(".evorch-core.lock"))
        .unwrap();
    rustix::fs::flock(&lifetime, rustix::fs::FlockOperation::LockExclusive).unwrap();
    pa.ack();
    pa.next("spawned");
    let mut core = r.accept("inherited");
    core.ack();
    core.next("adopted");
    core.ack();
    poll(&mut [PollFd::new(&core.fd, PollFlags::IN)], None).unwrap();
    core.core = false;
    pa.ack();
    pa.next("already-running");
    let path = r.identity().socket(&r.runtime).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let identity = r.identity();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut len = [0; 4];
        stream.read_exact(&mut len).unwrap();
        let len = u32::from_be_bytes(len);
        assert!(len < 1024 * 1024);
        let mut bytes = vec![0; len as usize];
        stream.read_exact(&mut bytes).unwrap();
        let _: ClientHello = serde_json::from_slice(&bytes).unwrap();
        let hello = ServerHello {
            protocol_version: ProtocolVersion::CURRENT,
            capabilities: Capability::ALL.to_vec(),
            server_build: BuildInfo {
                name: "legacy-test-owner".into(),
                version: "1".into(),
                revision: None,
            },
            core: CoreIdentity::new(identity.identity()).unwrap(),
            instance_epoch: InstanceEpoch(7),
            limits: WireLimits::default(),
            status: CoreStatus {
                state: CoreState::Ready,
                retention_reasons: vec![],
                running: ActivityCounts::default(),
            },
        };
        stream
            .write_all(
                &workbench_protocol::framing::encode_frame(
                    &HandshakeReply::Accepted(hello),
                    1024 * 1024,
                )
                .unwrap(),
            )
            .unwrap();
        let mut bytes = [0; 1024];
        while stream.read(&mut bytes).unwrap_or(0) != 0 {}
    });
    pa.ack();
    pa.next("connected");
    assert_eq!(pa.event["value"], 7);
    pa.ack();
    assert!(a.0.wait().unwrap().success());
    server.join().unwrap();
}
#[test]
fn inherited_unlocked_or_wrong_inode_descriptor_is_not_a_launch_guard() {
    use std::os::fd::IntoRawFd;
    let r = Rig::new();
    let id = r.identity();
    let launch = LaunchGuard::acquire(&id).unwrap();
    drop(launch);
    for path in [r.config.join(".evorch-core.launch"), r.config.join("other")] {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        // SAFETY: into_raw_fd transfers a valid descriptor with no other owner.
        assert!(unsafe { LaunchGuard::adopt(&id, file.into_raw_fd()) }.is_err());
        assert!(LaunchGuard::acquire(&id).is_ok());
    }
}
