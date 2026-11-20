//! Exec guard and immutable delivery. Existing spawn owners are NOT migrated in B1.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const COMPAT_ID: &str = env!("GUARD_COMPAT_ID");
pub const TARGET: &str = env!("GUARD_TARGET");
pub const BUNDLE_ENV: &str = "EVORCH_PROCESS_GUARD_BUNDLE";
#[cfg(target_os = "linux")]
pub mod guard;
#[cfg(target_os = "linux")]
pub mod image;
#[cfg(target_os = "linux")]
pub mod wire;
#[cfg(target_os = "linux")]
mod client {
    use super::{
        BUNDLE_ENV, COMPAT_ID,
        image::GuardImage,
        wire::{self, Decoder, Kind},
    };
    use serde::{Deserialize, Serialize};
    use std::os::unix::process::ExitStatusExt;
    use std::{
        io,
        net::Shutdown,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            linux::net::SocketAddrExt,
            unix::{
                ffi::OsStrExt,
                net::{SocketAddr, UnixListener, UnixStream},
            },
        },
        path::{Path, PathBuf},
        process::{Child, Command, ExitStatus, Stdio},
        sync::{Arc, OnceLock},
        time::{Duration, Instant},
    };

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct LaunchSpec {
        pub argv: Vec<Vec<u8>>,
        pub env: Vec<(Vec<u8>, Vec<u8>)>,
        pub cwd: Vec<u8>,
    }
    impl LaunchSpec {
        pub fn new(program: impl AsRef<std::ffi::OsStr>) -> io::Result<Self> {
            Ok(Self {
                argv: vec![program.as_ref().as_bytes().to_vec()],
                env: std::env::vars_os()
                    .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                    .collect(),
                cwd: std::env::current_dir()?.as_os_str().as_bytes().to_vec(),
            })
        }
    }
    #[derive(Clone, Debug)]
    pub struct GuardRuntime {
        image: Arc<GuardImage>,
    }
    static INSTALLED: OnceLock<GuardRuntime> = OnceLock::new();
    static LIBRARY: OnceLock<Result<GuardRuntime, String>> = OnceLock::new();
    impl GuardRuntime {
        pub fn from_bundle(path: &Path) -> io::Result<Self> {
            Ok(Self {
                image: Arc::new(GuardImage::open(path, None)?),
            })
        }
        pub fn bind_installed(role: &str) -> io::Result<Self> {
            let exe = std::env::current_exe()?.canonicalize()?;
            let path = exe.parent().unwrap().join("guard-bundle.json");
            let rt = Self {
                image: Arc::new(GuardImage::open(&path, Some(role))?),
            };
            INSTALLED
                .set(rt.clone())
                .map_err(|_| wire::invalid("guard runtime already installed"))?;
            Ok(rt)
        }
        pub fn for_library(explicit: Option<&Self>) -> io::Result<Self> {
            if let Some(rt) = explicit.or_else(|| INSTALLED.get()) {
                return Ok(rt.clone());
            }
            LIBRARY.get_or_init(||{let p=std::env::var_os(BUNDLE_ENV).map(PathBuf::from).ok_or_else(||format!("{BUNDLE_ENV} missing: scripts/guard-artifact.sh prepare --profile release --out DIR"))?;Self::from_bundle(&p).map_err(|e|e.to_string())}).clone().map_err(|e|io::Error::new(io::ErrorKind::NotFound,e))
        }
        pub fn image(&self) -> &GuardImage {
            &self.image
        }
        pub fn spawn(&self, spec: &LaunchSpec, stdio: [Stdio; 3]) -> io::Result<GuardChild> {
            // Validate before creating a helper, including kernel argv/env budget.
            super::guard::validate(spec)?;
            let start = Instant::now();
            let deadline = start + Duration::from_secs(5);
            let mut random = [0u8; 16];
            let n = unsafe { libc::getrandom(random.as_mut_ptr().cast(), random.len(), 0) };
            if n != 16 {
                return Err(io::Error::last_os_error());
            }
            let nonce = random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let name = format!("evorch-guard-{}-{nonce}", std::process::id());
            let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
            let listener = non_stdio(UnixListener::bind_addr(&addr)?)?;
            listener.set_nonblocking(true)?;
            let [stdin, stdout, stderr] = stdio;
            let [dev, ino] = self.image.identity_args();
            let mut helper = Command::new(self.image.inherited_exec_path())
                .args([
                    "--launch",
                    &dev,
                    &ino,
                    &std::process::id().to_string(),
                    &unsafe { libc::getuid() }.to_string(),
                    &name,
                    &nonce,
                ])
                .env_clear()
                .current_dir("/")
                .stdin(stdin)
                .stdout(stdout)
                .stderr(stderr)
                .spawn()?;
            let mut committed = false;
            let result = (|| {
                let pidfd = pidfd_open(helper.id() as i32)?;
                let mut wrong = 0;
                let mut stream = loop {
                    let mut fds = [
                        wire::fd(listener.as_raw_fd(), libc::POLLIN),
                        wire::fd(pidfd.as_raw_fd(), libc::POLLIN),
                    ];
                    wire::poll(&mut fds, Some(deadline))?;
                    if fds[1].revents != 0 {
                        return Err(wire::invalid("GuardLost before authentication"));
                    }
                    match listener.accept() {
                        Ok((s, _)) => {
                            let s = non_stdio(s)?;
                            if peer(&s, helper.id() as i32, unsafe { libc::getuid() })? {
                                break s;
                            }
                            wrong += 1;
                            if wrong >= 16 {
                                return Err(wire::invalid("too many wrong guard peers"));
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e),
                    }
                };
                stream.set_nonblocking(true)?;
                let mut decoder = Decoder::default();
                let (kind, hello) =
                    wire::receive(&mut stream, &mut decoder, pidfd.as_raw_fd(), Some(deadline))?;
                if kind != Kind::Hello || hello != format!("{COMPAT_ID}:{nonce}").as_bytes() {
                    return Err(wire::invalid("guard Hello ABI/compat/nonce mismatch"));
                }
                let bytes = serde_json::to_vec(spec)?;
                if bytes.len() > wire::LAUNCH_LIMIT {
                    return Err(io::Error::from_raw_os_error(libc::E2BIG));
                }
                for chunk in bytes.chunks(wire::FRAME_LIMIT) {
                    wire::send(&mut stream, Kind::LaunchChunk, chunk, Some(deadline))?;
                }
                wire::send(&mut stream, Kind::LaunchEnd, &[], Some(deadline))?;
                let (kind, p) =
                    wire::receive(&mut stream, &mut decoder, pidfd.as_raw_fd(), Some(deadline))?;
                if kind == Kind::SpawnError {
                    return Err(io::Error::from_raw_os_error(wire::integer(&p)?));
                }
                if kind != Kind::Ready || !p.is_empty() {
                    return Err(wire::invalid("guard Ready expected"));
                }
                committed = true; // Set before the first byte of Commit can be sent.
                wire::send(&mut stream, Kind::Commit, &[], Some(deadline))?;
                let (kind, p) = wire::receive(&mut stream, &mut decoder, pidfd.as_raw_fd(), None)?;
                if kind == Kind::SpawnError {
                    let _ = stream.shutdown(Shutdown::Write);
                    return Err(io::Error::from_raw_os_error(wire::integer(&p)?));
                }
                if kind != Kind::ExecOk {
                    return Err(wire::invalid("guard ExecOk expected"));
                }
                let target = wire::integer(&p)?;
                let mut fds = [wire::fd(pidfd.as_raw_fd(), libc::POLLIN)];
                if unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                if fds[0].revents != 0 {
                    return Err(wire::invalid("GuardLost at ExecOk"));
                }
                Ok((stream, decoder, pidfd, target))
            })();
            match result {
                Ok((stream, decoder, pidfd, target)) => Ok(GuardChild {
                    helper,
                    stream,
                    decoder,
                    pidfd,
                    target,
                    root: None,
                    final_status: None,
                    wait_requested: false,
                    write_closed: false,
                }),
                Err(e) => {
                    if !committed {
                        let _ = helper.kill();
                    }
                    let _ = helper.wait();
                    Err(e)
                }
            }
        }
    }
    /// Internal descriptors must never fill a caller's closed stdio slot.
    /// Duplication retains CLOEXEC atomically; the original is closed by RAII.
    pub(crate) fn non_stdio<T: AsRawFd + Into<OwnedFd> + From<OwnedFd>>(fd: T) -> io::Result<T> {
        let owned: OwnedFd = fd.into();
        let fd = owned.as_raw_fd();
        if fd < 3 {
            let high = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            if high < 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(T::from(unsafe { OwnedFd::from_raw_fd(high) }));
        }
        Ok(T::from(owned))
    }
    pub fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        non_stdio(unsafe { OwnedFd::from_raw_fd(fd) })
    }
    pub fn peer(s: &UnixStream, pid: i32, uid: u32) -> io::Result<bool> {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut n = std::mem::size_of_val(&cred) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut n,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.pid == pid && cred.uid == uid)
    }
    /// The decoder and status live in the owner, not in any one wait invocation.
    #[derive(Debug)]
    pub struct GuardChild {
        helper: Child,
        stream: UnixStream,
        decoder: Decoder,
        pidfd: OwnedFd,
        target: i32,
        root: Option<i32>,
        final_status: Option<i32>,
        wait_requested: bool,
        write_closed: bool,
    }
    impl GuardChild {
        pub fn id(&self) -> u32 {
            self.helper.id()
        }
        pub fn target_id(&self) -> u32 {
            self.target as u32
        }
        pub fn control(&self) -> &UnixStream {
            &self.stream
        }
        fn receive(&mut self) -> io::Result<Kind> {
            let (k, p) = wire::receive(
                &mut self.stream,
                &mut self.decoder,
                self.pidfd.as_raw_fd(),
                None,
            )
            .map_err(|e| io::Error::new(e.kind(), format!("GuardLost: {e}")))?;
            match k {
                Kind::RootExited => self.root = Some(wire::integer(&p)?),
                Kind::FinalStatus => {
                    let s = wire::integer(&p)?;
                    if !self.helper.wait()?.success() {
                        return Err(wire::invalid("GuardLost: abnormal helper status"));
                    }
                    self.root = Some(s);
                    self.final_status = Some(s);
                }
                Kind::Running | Kind::Ack => {
                    if !p.is_empty() {
                        return Err(wire::invalid("guard reply payload"));
                    }
                }
                _ => return Err(wire::invalid("unexpected guard response")),
            }
            Ok(k)
        }
        pub fn exited(&mut self) -> io::Result<ExitStatus> {
            while self.root.is_none() {
                self.receive()?;
            }
            Ok(ExitStatus::from_raw(self.root.unwrap()))
        }
        /// Sticky reservation: cancelling the caller's future does not undo this.
        pub fn request_wait(&mut self) -> io::Result<()> {
            if !self.wait_requested && !self.write_closed && self.final_status.is_none() {
                self.request(Kind::Wait, &[])?;
                self.wait_requested = true;
            }
            Ok(())
        }
        pub fn wait(&mut self) -> io::Result<ExitStatus> {
            self.request_wait()?;
            while self.final_status.is_none() {
                self.receive()?;
            }
            Ok(ExitStatus::from_raw(self.final_status.unwrap()))
        }
        pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            if let Some(s) = self.final_status {
                return Ok(Some(ExitStatus::from_raw(s)));
            }
            self.request(Kind::TryReap, &[])?;
            loop {
                match self.receive()? {
                    Kind::Running => return Ok(None),
                    Kind::FinalStatus => return Ok(self.final_status.map(ExitStatus::from_raw)),
                    _ => {}
                }
            }
        }
        fn request(&mut self, kind: Kind, payload: &[u8]) -> io::Result<()> {
            if self.write_closed {
                return Err(wire::invalid("GuardLost: control already half-closed"));
            }
            if let Err(e) = wire::try_send(&mut self.stream, kind, payload) {
                let _ = self.force_kill();
                return Err(io::Error::new(e.kind(), format!("GuardLost: {e}")));
            }
            Ok(())
        }
        /// Successful return means a complete request, not a signal receipt.
        /// Backpressure is an error and half-closes the invalidated channel.
        pub fn request_signal(&mut self, signal: i32) -> io::Result<()> {
            self.request(Kind::Signal, &signal.to_le_bytes())
        }
        pub fn signal(&mut self, signal: i32) -> io::Result<()> {
            self.request_signal(signal)?;
            while self.receive()? != Kind::Ack {}
            Ok(())
        }
        pub fn force_kill(&mut self) -> io::Result<()> {
            if self.final_status.is_some() || self.write_closed {
                return Ok(());
            }
            self.stream.shutdown(Shutdown::Write)?;
            self.write_closed = true;
            Ok(())
        }
    }
    impl Drop for GuardChild {
        fn drop(&mut self) {
            if self.final_status.is_none() {
                let _ = self.stream.shutdown(Shutdown::Write);
                let _ = self.helper.wait();
            }
        }
    }
}
#[cfg(target_os = "linux")]
pub use client::*;
