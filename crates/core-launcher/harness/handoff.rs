//! Feature-gated process fixture. Not a product core and has no DB/runtime.
#[cfg(target_os = "linux")]
mod linux {
    use core_instance::{
        ConfigIdentity, Instance, LaunchGuard,
        handoff::{
            BlockedShutdownSignals, HandoffEntry, HandoffRecord, ShutdownSignal,
            running_elf_identity,
        },
    };
    use core_launcher::handoff::{HandoffDescription, HandoffTarget, ProbeLimits, SchemaRange};
    use std::{
        ffi::OsString,
        fs::File,
        io::{self, Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
    };
    static SIGNALS: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn signal(s: i32) {
        SIGNALS.fetch_or(if s == libc::SIGINT { 1 } else { 2 }, Ordering::SeqCst);
    }
    fn say(s: &str) {
        println!("{s}");
        io::stdout().flush().unwrap();
    }
    fn step() {
        let mut b = [0];
        io::stdin().read_exact(&mut b).unwrap();
    }
    fn value(args: &[OsString], key: &str) -> Option<OsString> {
        args.windows(2).find(|a| a[0] == key).map(|a| a[1].clone())
    }
    fn record(config: &ConfigIdentity, epoch: u64) -> HandoffRecord {
        HandoffRecord {
            restart_id: format!("fixture-{epoch}"),
            config_identity: config.identity().into(),
            successor_epoch: epoch,
            nonce_hash: "fixture-nonce-hash".into(),
            successor_build: running_elf_identity().unwrap(),
        }
    }
    fn install() {
        unsafe {
            let mut a: libc::sigaction = std::mem::zeroed();
            a.sa_sigaction = signal as *const () as usize;
            libc::sigemptyset(&mut a.sa_mask);
            assert_eq!(libc::sigaction(libc::SIGINT, &a, std::ptr::null_mut()), 0);
            assert_eq!(libc::sigaction(libc::SIGTERM, &a, std::ptr::null_mut()), 0);
        }
    }
    fn probe_mode(args: &[OsString]) {
        match value(args, "--probe-behavior")
            .and_then(|v| v.into_string().ok())
            .as_deref()
        {
            Some("invalid") => say("not-json"),
            Some("large") => {
                let b = [b'x'; 4096];
                loop {
                    if io::stdout().write_all(&b).is_err() {
                        break;
                    }
                }
            }
            Some("hang") | Some("escape") => {
                let escape = value(args, "--probe-behavior").unwrap() == "escape";
                let notify = value(args, "--notify").unwrap();
                let mut stream = UnixStream::connect(notify).unwrap();
                // Only this single-threaded fixture forks. Parent waits on a pipe
                // until the escaped grandchild and its child are both established.
                let mut ready = [0; 2];
                assert_eq!(unsafe { libc::pipe(ready.as_mut_ptr()) }, 0);
                let supervisor = unsafe { libc::getppid() };
                let child = unsafe { libc::fork() };
                assert!(child >= 0);
                if child == 0 {
                    unsafe {
                        libc::close(stream.as_raw_fd());
                        libc::close(ready[0]);
                        if escape {
                            assert!(libc::setsid() > 0);
                        }
                        let grandchild = libc::fork();
                        assert!(grandchild >= 0);
                        if grandchild == 0 {
                            libc::close(ready[1]);
                            loop {
                                libc::pause();
                            }
                        }
                        let text = format!(
                            "{supervisor} {} {} {}",
                            libc::getppid(),
                            libc::getpid(),
                            grandchild
                        );
                        libc::write(ready[1], text.as_ptr().cast(), text.len());
                        libc::close(ready[1]);
                        loop {
                            libc::pause();
                        }
                    }
                }
                unsafe {
                    libc::close(ready[1]);
                }
                let mut b = [0; 128];
                let n = unsafe { libc::read(ready[0], b.as_mut_ptr().cast(), b.len()) };
                assert!(n > 0);
                unsafe {
                    libc::close(ready[0]);
                }
                stream.write_all(&b[..n as usize]).unwrap();
                drop(stream);
                loop {
                    unsafe {
                        libc::pause();
                    }
                }
            }
            Some("old") => {
                let mut d = description();
                d.protocol += 1;
                say(&serde_json::to_string(&d).unwrap());
            }
            Some("wrong-build") => {
                let mut d = description();
                d.build_identity = "sha256:old".into();
                say(&serde_json::to_string(&d).unwrap());
            }
            _ => say(&serde_json::to_string(&description()).unwrap()),
        }
    }
    fn description() -> HandoffDescription {
        HandoffDescription::for_running(
            1,
            1,
            SchemaRange {
                minimum: 62,
                maximum: 63,
            },
            1,
            "fixture-catalog-v1".into(),
        )
        .unwrap()
    }

    pub fn run() {
        let signals = BlockedShutdownSignals::at_entry().unwrap();
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.iter().any(|a| a == "--probe-parent") {
            // A separate test process owns this subreaper setting; neither the
            // product host nor the test runner's process state is modified.
            assert_eq!(
                unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
                0
            );
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--probe-host")
                .arg("--target")
                .arg(value(&args, "--target").unwrap())
                .spawn()
                .unwrap();
            let mut pid = String::new();
            io::stdin().read_line(&mut pid).unwrap();
            let pid = pid.trim().parse::<i32>().unwrap();
            child.kill().unwrap();
            child.wait().unwrap();
            let mut status = 0;
            loop {
                let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
                if rc == pid {
                    break;
                }
                assert_eq!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::Interrupted
                );
            }
            assert!(libc::WIFEXITED(status));
            say("parent-death-cleaned");
            return;
        }
        if args.iter().any(|a| a == "--probe-host") {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let target = value(&args, "--target").unwrap();
            let _ = runtime.block_on(HandoffTarget::probe(
                Path::new(&target),
                ProbeLimits {
                    timeout: std::time::Duration::from_secs(60),
                    output_bytes: 4096,
                },
            ));
            panic!("probe host must be killed by the fixture parent");
        }
        // Self-description happens before config, lock, listener or executor init.
        if args.iter().any(|a| a == "--describe-handoff") {
            probe_mode(&args);
            return;
        }
        let scenario = value(&args, "--scenario").unwrap().into_string().unwrap();
        let inherited = value(&args, "--handoff-lifetime-fd")
            .map(|v| v.to_str().unwrap().parse::<i32>().unwrap());
        let ticket = value(&args, "--handoff-ticket-fd")
            .map(|v| v.to_str().unwrap().parse::<i32>().unwrap());
        // The leak case deliberately violates entry ordering to prove the audit.
        let _leak = if inherited.is_some() && scenario == "leak" {
            Some(File::open("/dev/null").unwrap())
        } else {
            None
        };
        let entry = inherited.map(|fd| unsafe { HandoffEntry::inspect(fd, ticket.unwrap()) });
        if scenario == "leak" && inherited.is_some() {
            assert!(entry.unwrap().is_err());
            say("rejected-fds");
            return;
        }
        let entry = entry.map(Result::unwrap);
        let config = ConfigIdentity::new(Path::new(&value(&args, "--config").unwrap())).unwrap();
        let ipc = value(&args, "--ipc-parent").unwrap();
        let ipc = Path::new(&ipc);
        let target_path = value(&args, "--target").unwrap();
        let round = value(&args, "--round")
            .and_then(|s| s.to_str().unwrap().parse::<u64>().ok())
            .unwrap_or(1);
        let instance = if let Some(entry) = entry {
            let mut expected = record(&config, round);
            if scenario == "mismatch" {
                expected.successor_epoch += 1;
            }
            let mut calls = 0;
            let result = Instance::adopt_handoff(&config, ipc, entry, &expected, |_| {
                calls += 1;
                if scenario == "callback-fails" {
                    Err(io::Error::other("DB CAS rejected"))
                } else {
                    Ok(())
                }
            });
            if scenario == "mismatch" || scenario == "callback-fails" {
                assert!(result.is_err());
                assert_eq!(calls, usize::from(scenario == "callback-fails"));
                say("rejected-adoption");
                return;
            }
            let (instance, proof) = result.unwrap();
            assert_eq!(calls, 1);
            assert_eq!(proof.restart_id(), expected.restart_id);
            assert_eq!(proof.config_identity(), config.identity());
            assert_eq!(proof.successor_epoch(), round);
            assert_eq!(proof.nonce_hash(), expected.nonce_hash);
            assert_eq!(proof.successor_build(), expected.successor_build);
            // The consumed ticket number cannot be used a second time. inspect
            // rejects before taking ownership of Instance's still-valid lifetime.
            assert!(unsafe { HandoffEntry::inspect(inherited.unwrap(), ticket.unwrap()) }.is_err());
            let inbox = signals.inbox().unwrap();
            let pending = inbox.read_pending().unwrap();
            let observed = usize::from(pending.contains(&ShutdownSignal::Interrupt))
                | (usize::from(pending.contains(&ShutdownSignal::Terminate)) << 1);
            drop(inbox);
            assert_eq!(std::env::current_dir().unwrap(), config.config_dir());
            let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask);
                assert_eq!(libc::sigismember(&mask, libc::SIGUSR1), 1);
            }
            say(&format!(
                "adopted {} {round} signals={} path={}",
                std::process::id(),
                observed,
                std::env::var("PATH").unwrap()
            ));
            if scenario != "twice" || round == 2 {
                step();
                return;
            }
            instance
        } else {
            let launch = LaunchGuard::acquire(&config).unwrap();
            let instance = Instance::acquire(&launch, ipc).unwrap();
            drop(launch);
            unsafe {
                let mut mask: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut mask);
                libc::sigaddset(&mut mask, libc::SIGUSR1);
                libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
            }
            instance
        };
        // Non-allowlisted FD starts inheritable; it must not cross exec.
        let extra = File::open("/dev/null").unwrap();
        unsafe {
            libc::fcntl(extra.as_raw_fd(), libc::F_SETFD, 0);
        }
        if scenario == "exec-failure" {
            install();
            let before = unsafe { libc::fcntl(extra.as_raw_fd(), libc::F_GETFD) };
            let result = unsafe {
                instance.exec_handoff(
                    &signals,
                    Path::new("/definitely-no-evorch-core"),
                    &[],
                    record(&config, round),
                )
            };
            assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOENT));
            assert_eq!(
                unsafe { libc::fcntl(extra.as_raw_fd(), libc::F_GETFD) },
                before
            );
            unsafe {
                libc::raise(libc::SIGINT);
                libc::raise(libc::SIGTERM);
            }
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigaction(libc::SIGINT, std::ptr::null(), &mut action);
            }
            assert_eq!(action.sa_sigaction, signal as *const () as usize);
            let inbox = signals.inbox().unwrap();
            assert_eq!(inbox.read_pending().unwrap().len(), 2);
            assert_eq!(SIGNALS.load(Ordering::SeqCst), 0);
            say("exec-failed-restored");
            step();
            return;
        }
        if scenario == "thread-fence" {
            let (tx, rx) = std::sync::mpsc::channel();
            let (ready, wait) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                unsafe {
                    let mut mask: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    libc::sigaddset(&mut mask, libc::SIGTERM);
                    libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
                }
                ready.send(()).unwrap();
                let f = File::open("/dev/null").unwrap();
                rx.recv().unwrap();
                drop(f);
            });
            wait.recv().unwrap();
            let result = unsafe {
                instance.exec_handoff(
                    &signals,
                    Path::new("/never-exec"),
                    &[],
                    record(&config, round),
                )
            };
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
            tx.send(()).unwrap();
            thread.join().unwrap();
            say("thread-fence-rejected");
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let target = runtime.block_on(async {
            let t = HandoffTarget::probe(Path::new(&target_path), ProbeLimits::default())
                .await
                .unwrap();
            t.reprobe(ProbeLimits::default()).await.unwrap();
            t
        });
        drop(runtime);
        // Live background threads are allowed when they inherit blocked signals
        // and only create CLOEXEC FDs. They are terminated by a successful exec.
        let (_worker_tx, worker_rx) = std::sync::mpsc::channel::<()>();
        let (worker_ready, worker_wait) = std::sync::mpsc::channel();
        let _worker = if scenario == "multithread" {
            Some(std::thread::spawn(move || {
                let _file = File::open("/dev/null").unwrap();
                worker_ready.send(()).unwrap();
                let _ = worker_rx.recv();
            }))
        } else {
            None
        };
        if scenario == "multithread" {
            worker_wait.recv().unwrap();
        }
        say("before-exec");
        step();
        let next_round = if scenario == "twice" && inherited.is_some() {
            2
        } else {
            round
        };
        let next: Vec<OsString> = vec![
            "--scenario".into(),
            scenario.into(),
            "--config".into(),
            config.config_dir().into(),
            "--ipc-parent".into(),
            ipc.into(),
            "--target".into(),
            target_path,
            "--round".into(),
            next_round.to_string().into(),
        ];
        let error = unsafe { target.exec(&instance, &signals, &next, record(&config, next_round)) }
            .unwrap_err();
        panic!("exec returned: {error}");
    }
}
fn main() {
    #[cfg(target_os = "linux")]
    linux::run();
}
