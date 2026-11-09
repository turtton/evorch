//! Process-only test harness: no storage/runtime and no GUI. Every failpoint
//! uses a control socket acknowledgement rather than a wall-clock delay.
use core_instance::{ConfigIdentity, Instance, InstanceError, LaunchGuard};
use core_launcher::{CoreBinary, LaunchStage, Launcher};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::Mutex,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use workbench_protocol::{handshake::*, ids::*, wire::InstanceEpoch};

struct Control(Mutex<BufReader<UnixStream>>);
impl Control {
    fn new(config: &ConfigIdentity) -> Self {
        let path = fs::read_to_string(config.config_dir().join("control")).unwrap();
        Self(Mutex::new(BufReader::new(
            UnixStream::connect(path).unwrap(),
        )))
    }
    fn point(&self, stage: &str, value: u32) {
        let mut io = self.0.lock().unwrap();
        writeln!(
            io.get_mut(),
            "{}",
            serde_json::json!({"stage":stage,"pid":std::process::id(),"value":value})
        )
        .unwrap();
        let mut ack = String::new();
        assert_ne!(
            io.read_line(&mut ack).unwrap(),
            0,
            "controller closed at {stage}"
        );
    }
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(
            if matches!(
                e.downcast_ref::<InstanceError>(),
                Some(InstanceError::AlreadyRunning)
            ) {
                2
            } else {
                1
            },
        );
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut config = None;
    let mut fd = None;
    let mut client = false;
    let mut scan = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config-dir" => config = args.next().map(PathBuf::from),
            "--launch-fd" => fd = Some(args.next().unwrap().parse::<i32>()?),
            "--client" => client = true,
            "--scan" => scan = true,
            _ => panic!("bad arg {a}"),
        }
    }
    let config = ConfigIdentity::new(&config.unwrap())?;
    let control = Control::new(&config);
    if scan {
        // This is an actually exec'd process, not merely an fcntl assertion.
        for entry in fs::read_dir("/proc/self/fd")? {
            let Ok(path) = fs::read_link(entry?.path()) else {
                continue;
            };
            assert!(
                !path
                    .file_name()
                    .is_some_and(|n| n == ".evorch-core.launch" || n == ".evorch-core.lock"),
                "leaked {path:?}"
            );
        }
        control.point("scan-clean", 0);
        return Ok(());
    }
    let runtime = PathBuf::from(fs::read_to_string(config.config_dir().join("runtime"))?);
    if client {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let socket = config.socket(&runtime)?;
        let binary = CoreBinary::explicit(&std::env::current_exe()?)?;
        let cc = workbench_client::ClientConfig::new(
            ClientId::new(format!("c-{}", std::process::id()))?,
            BuildInfo {
                name: "harness".into(),
                version: "1".into(),
                revision: None,
            },
        );
        let launcher = Launcher::with_socket(config, binary, cc, socket)?
            .diagnostic_after(std::time::Duration::MAX);
        control.point("client-start", 0);
        let connected = rt.block_on(launcher.connect_or_start_observed(|s| match s {
            LaunchStage::BeforeSpawn => control.point("before-spawn", 0),
            LaunchStage::Spawned(pid) => control.point("spawned", pid),
            LaunchStage::WaitingForLaunch => control.point("waiting-launch", 0),
            LaunchStage::LifetimeBusy => control.point("lifetime-busy", 0),
            LaunchStage::AlreadyRunning => control.point("already-running", 0),
        }))?;
        control.point(
            "connected",
            connected.server_hello().instance_epoch.0 as u32,
        );
        return Ok(());
    }
    control.point("inherited", 0);
    let launch = match fd {
        // SAFETY: owned inherited launch fd, at entry before runtime creation.
        Some(fd) => unsafe { LaunchGuard::adopt(&config, fd)? },
        None => LaunchGuard::acquire(&config)?,
    };
    control.point("adopted", 0);
    let instance = Instance::acquire(&launch, &runtime)?;
    let mut scan = std::process::Command::new(std::env::current_exe()?);
    scan.arg("--scan")
        .arg("--config-dir")
        .arg(config.config_dir());
    assert!(scan.status()?.success());
    control.point("before-ready", 0);
    let listener = instance.bind()?;
    listener.set_nonblocking(true)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let listener = tokio::net::UnixListener::from_std(listener)?;
        let epoch = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
        let mut hello = ServerHello {
            protocol_version: ProtocolVersion::CURRENT,
            capabilities: Capability::ALL.to_vec(),
            server_build: BuildInfo {
                name: "harness".into(),
                version: "1".into(),
                revision: None,
            },
            core: CoreIdentity::new(config.identity())?,
            instance_epoch: InstanceEpoch(epoch),
            limits: WireLimits::default(),
            status: CoreStatus {
                state: CoreState::Ready,
                retention_reasons: vec![],
                running: ActivityCounts::default(),
            },
        };
        if let Ok(data) = fs::read(config.config_dir().join("hello.json")) {
            let mut value = serde_json::to_value(&hello)?;
            let overrides: serde_json::Map<String, serde_json::Value> =
                serde_json::from_slice(&data)?;
            value.as_object_mut().unwrap().extend(overrides);
            hello = serde_json::from_value(value)?;
        }
        instance.publish(epoch)?;
        control.point("ready", 0);
        drop(launch);
        loop {
            let (mut socket, _) = listener.accept().await?;
            let hello = hello.clone();
            tokio::spawn(async move {
                let len = socket.read_u32().await.unwrap();
                assert!(len <= 1024 * 1024);
                let mut bytes = vec![0; len as usize];
                socket.read_exact(&mut bytes).await.unwrap();
                let _: ClientHello = serde_json::from_slice(&bytes).unwrap();
                let reply = workbench_protocol::framing::encode_frame(
                    &HandshakeReply::Accepted(hello),
                    1024 * 1024,
                )
                .unwrap();
                socket.write_all(&reply).await.unwrap();
                let mut buf = [0; 4096];
                while socket.read(&mut buf).await.unwrap_or(0) != 0 {}
            });
        }
        #[allow(unreachable_code)]
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}
