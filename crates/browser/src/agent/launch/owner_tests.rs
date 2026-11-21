use super::*;
use crate::agent::RequestPolicy;
use futures_util::io::{AsyncBufReadExt, BufReader};
use std::{
    collections::BTreeMap,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::Path,
    sync::Arc,
};
#[path = "../../../fixtures/launch/filesystem.rs"]
mod filesystem;
#[path = "../../../fixtures/launch/old_config.rs"]
mod old_config;
#[path = "../../../fixtures/launch/processes.rs"]
mod processes;
use filesystem::Watch;

fn fifo(path: &Path) {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

struct Allow;
#[async_trait::async_trait]
impl RequestPolicy for Allow {
    async fn check(&self, _: &url::Url) -> Result<(), String> {
        Ok(())
    }
    async fn resolve(
        &self,
        host: &str,
        port: u16,
        _: bool,
    ) -> Result<Vec<std::net::SocketAddr>, String> {
        tokio::net::lookup_host((host, port))
            .await
            .map(Iterator::collect)
            .map_err(|e| e.to_string())
    }
}
fn fixture(root: &Path) -> PathBuf {
    for name in ["start", "hold"] {
        fifo(&root.join(name));
    }
    let source = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/launch/capture.sh");
    let exe = root.join("chromium-fixture");
    std::fs::copy(source, &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("endpoint"),
        b"DevTools listening on ws://127.0.0.1:9222/devtools/browser/test\n",
    )
    .unwrap();
    exe
}
async fn start(root: &Path) {
    let path = root.join("start");
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .write_all(b"go\n")
            .unwrap();
    })
    .await
    .unwrap();
}
fn env(bytes: &[u8]) -> BTreeMap<Vec<u8>, Vec<u8>> {
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let i = s.iter().position(|&b| b == b'=').unwrap();
            (s[..i].to_vec(), s[i + 1..].to_vec())
        })
        .collect()
}
fn argv(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut args: Vec<_> = bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    // Stable key-only sort: repeated keys keep their order AND their full values.
    args[1..].sort_by(|a, b| {
        a.split(|&c| c == b'=')
            .next()
            .cmp(&b.split(|&c| c == b'=').next())
    });
    args
}
fn capture(root: &Path) -> BTreeMap<&'static str, Vec<u8>> {
    ["argv", "env", "cwd", "stdin", "stdout", "stderr-fd"]
        .into_iter()
        .map(|name| {
            let mut bytes = std::fs::read(root.join(name)).unwrap();
            if name == "argv" {
                bytes = argv(&bytes)
                    .into_iter()
                    .flat_map(|s| s.into_iter().chain([0]))
                    .collect();
            }
            if name == "env" {
                bytes = env(&bytes)
                    .into_iter()
                    .flat_map(|(k, v)| k.into_iter().chain(*b"=").chain(v).chain([0]))
                    .collect();
            }
            if name == "stderr-fd" {
                assert!(bytes.starts_with(b"pipe:["));
                bytes = b"pipe".to_vec();
            }
            (name, bytes)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_contract_and_cleanup_in_controlled_environment() {
    if std::env::var_os("B3_CONTROLLED").is_none() {
        let root = tempfile::tempdir().unwrap();
        let stdin = std::fs::File::create(root.path().join("inherited-stdin")).unwrap();
        let status=std::process::Command::new(std::env::current_exe().unwrap())
   .args(["--exact","agent::chromium::launch::owner::tests::launch_contract_and_cleanup_in_controlled_environment","--nocapture"])
   .env_clear().env("B3_CONTROLLED","1").env("B3_CAPTURE",root.path())
   .env("PATH",std::env::var_os("PATH").unwrap()).env("HOME",root.path()).env("LANG","C.UTF-8")
   .env("CARGO_MANIFEST_DIR",std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
   .env("EVORCH_PROCESS_GUARD_BUNDLE",std::env::var_os("EVORCH_PROCESS_GUARD_BUNDLE").expect("guard test bundle"))
   .stdin(stdin).status().unwrap();
        assert!(status.success());
        return;
    }
    processes::subreaper();
    let mut tree = processes::Tree::default();
    let root = PathBuf::from(std::env::var_os("B3_CAPTURE").unwrap());
    let exe = fixture(&root);
    let profile = tempfile::Builder::new()
        .prefix("b3 profile space ")
        .tempdir()
        .unwrap();
    let profile_path = profile.path().to_owned();
    let proxy = Proxy::start(Arc::new(Allow)).await.unwrap();
    let proxy_address = proxy.address();
    let proxy_done = proxy.completion();
    let config = old_config::config(&exe, &profile_path, proxy_address);
    let mut old = config.launch().unwrap();
    let ready = Watch::new(&root, libc::IN_CLOSE_WRITE);
    // It may have written the record before the watch; the stderr endpoint below
    // is still a complete-record barrier, and start is a FIFO rendezvous.
    start(&root).await;
    let mut reader = BufReader::new(old.stderr.take().unwrap());
    let mut endpoint = String::new();
    reader.read_line(&mut endpoint).await.unwrap();
    let old_capture = capture(&root);
    old.kill().await.unwrap();
    old.wait().await.unwrap();
    drop(ready);
    let (mut owned, deadline) = Chromium::spawn(exe.clone(), profile, proxy).await.unwrap();
    start(&root).await;
    let endpoint = endpoint::read(
        owned.child.as_mut().unwrap().take_stderr().unwrap(),
        deadline,
    )
    .await
    .unwrap();
    println!(
        "B3_GOLDEN_READY pid={}",
        owned.child.as_ref().unwrap().id().unwrap()
    );
    let pid = owned.child.as_ref().unwrap().id().unwrap() as i32;
    tree.capture(std::process::id() as i32);
    tree.trace();
    assert!(endpoint.starts_with("ws://127.0.0.1:"));
    assert_eq!(old_capture, capture(&root));
    let expected_env: BTreeMap<_, _> = std::env::vars_os()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();
    assert_eq!(env(&std::fs::read(root.join("env")).unwrap()), expected_env);
    assert_eq!(old_capture["stdout"], b"/dev/null\n");
    assert_eq!(
        old_capture["stdin"],
        format!("{}\n", root.join("inherited-stdin").display()).as_bytes()
    );
    owned.retire().unwrap().await.unwrap().unwrap();
    tree.assert_group_dead(pid);
    assert!(
        !profile_path.exists(),
        "profile removed only after confirmed reap"
    );
    proxy_done.wait().await;
    assert!(
        tokio::net::TcpStream::connect(proxy_address).await.is_err(),
        "proxy listener closed"
    );

    // Same root, different keys are NOT allowed to normalize repeated switch values.
    assert_ne!(
        argv(b"exe\0--key=a\0--key=b\0"),
        argv(b"exe\0--key=b\0--key=a\0")
    );
    assert_ne!(argv(b"exe\0--key=a\0--key=b\0"), argv(b"exe\0--key=a,b\0"));
    assert_eq!(argv(b"exe\0--a=1\0--b=2\0"), argv(b"exe\0--b=2\0--a=1\0"));

    // Cancellation AFTER ExecOk, while waiting for the endpoint. The child and
    // profile must remain together even though launch never returns its owner.
    std::fs::remove_file(root.join("pid")).unwrap();
    let profile = tempfile::tempdir().unwrap();
    let path = profile.path().to_owned();
    let removed = Watch::new(&path, libc::IN_DELETE_SELF);
    let ready = Watch::new(&root, libc::IN_CLOSE_WRITE);
    let proxy = Proxy::start(Arc::new(Allow)).await.unwrap();
    let pending = tokio::spawn(Chromium::launch(exe.clone(), profile, proxy));
    ready.event(Some("pid")).await;
    let pid: i32 = std::fs::read_to_string(root.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    tree.capture(std::process::id() as i32);
    println!("B3_CANCEL_READY pid={pid}");
    pending.abort();
    assert!(matches!(pending.await, Err(error) if error.is_cancelled()));
    tree.assert_group_dead(pid);
    removed.event(None).await;
    assert!(!path.exists());

    // EOF/invalid endpoint/CDP rejection clean up exactly the same owner on error.
    let mut flood = b"log\n".repeat(256 * 1024 / 4);
    flood.extend(b"DevTools listening on ws://127.0.0.1:9222/devtools/browser/id\n");
    let mut long = vec![b'a'; 16 * 1024];
    long.push(b'\n');
    long.extend(b"DevTools listening on ws://127.0.0.1:9222/devtools/browser/id\n");
    for stderr in [
        Vec::new(), // close the real pipe while keeping the target alive
        b"invalid \xff\n".to_vec(),
        b"DevTools listening on ws://192.0.2.1:9222/devtools/browser/id\n".to_vec(),
        b"DevTools listening on invalid\n".to_vec(),
        flood,
        long,
    ] {
        std::fs::write(root.join("endpoint"), &stderr).unwrap();
        if stderr.is_empty() {
            std::fs::write(root.join("close-stderr"), "").unwrap();
        }
        let profile = tempfile::tempdir().unwrap();
        let path = profile.path().to_owned();
        let removed = Watch::new(&path, libc::IN_DELETE_SELF);
        let ready = Watch::new(&root, libc::IN_CLOSE_WRITE);
        let proxy = Proxy::start(Arc::new(Allow)).await.unwrap();
        let task = tokio::spawn(Chromium::launch(exe.clone(), profile, proxy));
        ready.event(Some("pid")).await;
        tree.capture(std::process::id() as i32);
        let pid: i32 = std::fs::read_to_string(root.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        start(&root).await;
        println!("B3_INVALID_STDERR_READY pid={pid} bytes={}", stderr.len());
        assert!(task.await.unwrap().is_err());
        if stderr.is_empty() {
            std::fs::remove_file(root.join("close-stderr")).unwrap();
        }
        tree.assert_group_dead(pid);
        removed.event(None).await;
        assert!(!path.exists());
    }
    // Real CDP handshake failures before and after chromiumoxide's connection.
    // On the second case the first socket stays open until AFTER cleanup, so
    // dropping the partial initializer, not a server EOF, must stop its driver.
    for successful_handshake in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        std::fs::write(
            root.join("endpoint"),
            format!("DevTools listening on ws://{address}/devtools/browser/test\n"),
        )
        .unwrap();
        let (release, hold) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let socket = if successful_handshake {
                let (s, _) = listener.accept().await.unwrap();
                Some(async_tungstenite::tokio::accept_async(s).await.unwrap())
            } else {
                None
            };
            let (mut rejected, _) = listener.accept().await.unwrap();
            tokio::io::AsyncWriteExt::write_all(
                &mut rejected,
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            )
            .await
            .unwrap();
            drop(rejected);
            let _ = hold.await;
            drop(socket);
        });
        let ready = Watch::new(&root, libc::IN_CLOSE_WRITE);
        let browser = Arc::new(crate::agent::AgentBrowser::new(
            crate::agent::LaunchOptions {
                executable: Some(exe.clone()),
                policy: Arc::new(Allow),
                max_pages: 2,
                max_tabs: 2,
            },
        ));
        let task = tokio::spawn({
            let browser = browser.clone();
            async move {
                use crate::agent::PageProvider;
                browser.page("partial-init").await
            }
        });
        ready.event(Some("pid")).await;
        tree.capture(std::process::id() as i32);
        let pid: i32 = std::fs::read_to_string(root.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let args = std::fs::read(root.join("argv")).unwrap();
        let profile = PathBuf::from(
            String::from_utf8_lossy(
                args.split(|&b| b == 0)
                    .find_map(|a| a.strip_prefix(b"--user-data-dir="))
                    .unwrap(),
            )
            .into_owned(),
        );
        let removed = Watch::new(&profile, libc::IN_DELETE_SELF);
        start(&root).await;
        assert!(task.await.unwrap().is_err());
        tree.assert_group_dead(pid);
        removed.event(None).await;
        assert!(!profile.exists());
        let _ = release.send(());
        server.await.unwrap();
        drop(browser);
    }
    // Destroy the authenticated helper AFTER a real ExecOk: no FinalStatus,
    // hence a failed reap must retain the profile, never claim cleanup success.
    let profile = tempfile::tempdir().unwrap();
    let kept = profile.path().to_owned();
    let ready = Watch::new(&root, libc::IN_CLOSE_WRITE);
    let proxy = Proxy::start(Arc::new(Allow)).await.unwrap();
    let proxy_done = proxy.completion();
    let (mut unconfirmed, _) = Chromium::spawn(exe, profile, proxy).await.unwrap();
    ready.event(Some("pid")).await;
    tree.capture(std::process::id() as i32);
    let helper = tree
        .processes
        .iter()
        .find(|p| !p.is_dead() && p.argv.iter().any(|a| a == "--launch"))
        .expect("authenticated launch helper");
    let helper_group = helper.pgid;
    helper.signal(libc::SIGKILL);
    tree.assert_group_dead(helper_group);
    assert!(unconfirmed.retire().unwrap().await.unwrap().is_err());
    assert!(kept.exists(), "unconfirmed reap must retain the profile");
    proxy_done.wait().await;
    drop(unconfirmed);
    drop(tree); // outer pidfds recover target, NOT the failed reaper
    std::fs::remove_dir_all(kept).unwrap();
}

#[tokio::test]
async fn executable_canonicalization_preserves_snap_launcher() {
    let root = tempfile::tempdir().unwrap();
    let snap = root.path().join("snap");
    std::fs::write(&snap, "").unwrap();
    let launcher = root.path().join("chromium");
    std::os::unix::fs::symlink(&snap, &launcher).unwrap();
    assert_eq!(executable_path(launcher.clone()).unwrap(), launcher);
    let native = root.path().join("native");
    std::fs::write(&native, "").unwrap();
    std::fs::remove_file(&launcher).unwrap();
    std::os::unix::fs::symlink(&native, &launcher).unwrap();
    assert_eq!(
        executable_path(launcher).unwrap(),
        std::fs::canonicalize(native).unwrap()
    );
}
