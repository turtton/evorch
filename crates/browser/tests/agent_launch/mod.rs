//! Real AgentBrowser owners; exit(74) here is a TEST owner, not product expiry.
use browser::agent::{AgentBrowser, LaunchOptions, PageProvider};
use std::{
    io::{BufRead, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
};
#[path = "../../fixtures/launch/filesystem.rs"]
mod filesystem;
#[path = "../../fixtures/launch/processes.rs"]
mod processes;
const TEST: &str = "agent_launch::real_owned_chromium_terminates_after_exit74_sigkill_and_drop";

#[test]
#[ignore = "requires installed Chromium; selected by browser-e2e"]
fn real_owned_chromium_terminates_after_exit74_sigkill_and_drop() {
    if std::env::var_os("B3_OWNER_ROLE").is_none() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST, "--ignored", "--nocapture"])
            .env_clear()
            .env("B3_OWNER_ROLE", "controller")
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", root.path())
            .env("TMPDIR", root.path())
            .env("LANG", "C.UTF-8")
            .env(
                "EVORCH_PROCESS_GUARD_BUNDLE",
                std::env::var_os("EVORCH_PROCESS_GUARD_BUNDLE").expect("guard bundle"),
            );
        if let Some(chrome) = std::env::var_os("CHROME") {
            command.env("CHROME", chrome);
        }
        assert!(command.status().unwrap().success());
        return;
    }
    if std::env::var("B3_OWNER_ROLE").unwrap() == "owner" {
        // Single-threaded fixture: the EOF callback's watch notification is
        // observed only after the driver finishes, deterministically.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(owner());
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(controller());
    }
}

async fn owner() {
    let url =
        super::serve("<!doctype html><title>B3 owned navigation</title><h1>Ready</h1>".into())
            .await;
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(super::Refuse {
            host: "blocked.invalid",
            port: 0,
        }),
        max_pages: 2,
        max_tabs: 2,
    });
    let mut page = browser.page("b3-owner").await.expect("real owned Chromium");
    page.navigate(&url)
        .await
        .expect("navigation before termination");
    let mut pages = browser.pages();
    loop {
        println!("B3_READY navigation-complete owner={}", std::process::id());
        std::io::stdout().flush().unwrap();
        let mut command = String::new();
        std::io::stdin().read_line(&mut command).unwrap();
        match command.trim() {
            "exit74" => std::process::exit(74), // deliberately no destructors
            "reset" => {
                pages.wait_for(|keys| keys.is_empty()).await.unwrap();
                page = browser
                    .page("b3-after-reset")
                    .await
                    .expect("relaunch after handler end");
                page.navigate(&url).await.expect("navigation after reset");
            }
            "drop" => {
                drop(page);
                drop(browser);
                println!("B3_DROPPED");
                std::io::stdout().flush().unwrap();
                command.clear();
                std::io::stdin().read_line(&mut command).unwrap();
                assert_eq!(command.trim(), "finish");
                break;
            }
            command => panic!("unexpected command {command}"),
        }
    }
}
async fn controller() {
    processes::subreaper();
    for mode in ["exit74", "sigkill", "drop", "reset"] {
        let mut tree = processes::Tree::default();
        let mut owner = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--ignored", "--nocapture"])
            .env("B3_OWNER_ROLE", "owner")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(owner.stdout.take().unwrap());
        let mut line = String::new();
        ready(&mut output, &mut line);
        for generation in 0..=2 {
            tree.capture(std::process::id() as i32);
            tree.trace();
            let root = tree
                .processes
                .iter()
                .find(|p| {
                    !p.is_dead()
                        && p.argv.iter().any(|a| a == "--remote-debugging-port=0")
                        && !p.argv.iter().any(|a| {
                            a.split_ascii_whitespace()
                                .any(|arg| arg.starts_with("--type="))
                        })
                })
                .expect("real browser root");
            let pgid = root.pgid;
            let pid = root.pid;
            let sid = root.sid;
            let group: Vec<_> = tree
                .processes
                .iter()
                .filter(|p| !p.is_dead() && p.pgid == pgid)
                .collect();
            assert!(group.len() > 2);
            assert!(
                group.iter().any(|p| p
                    .argv
                    .iter()
                    .any(|a| a.split_ascii_whitespace().any(|a| a == "--type=renderer"))),
                "must include a navigated renderer"
            );
            let profile = PathBuf::from(
                root.argv
                    .iter()
                    .find_map(|a| a.strip_prefix("--user-data-dir="))
                    .unwrap(),
            );
            let deleted = filesystem::Watch::new(&profile, libc::IN_DELETE_SELF);
            for p in tree.processes.iter().filter(|p| {
                !p.is_dead()
                    && p.argv
                        .first()
                        .is_some_and(|a| a.contains("crashpad_handler"))
            }) {
                println!(
                    "B3_CRASHPAD_RECORD pid={} sid={} pgid={} root_pgid={pgid} outside_guarantee={}",
                    p.pid,
                    p.sid,
                    p.pgid,
                    p.pgid != pgid
                );
            }
            println!(
                "B3_TERMINATE mode={mode} generation={generation} owner={} root={pid} group_members={}",
                owner.id(),
                group.len()
            );
            if mode == "reset" && generation < 2 {
                root.signal(libc::SIGKILL);
                writeln!(owner.stdin.as_mut().unwrap(), "reset").unwrap();
                ready(&mut output, &mut line);
                tree.assert_group_dead(pgid);
                deleted.event(None).await;
                assert!(!profile.exists());
                assert_eq!(pgid, pid);
                assert_eq!(sid, pid);
                println!("B3_GROUP_DEAD mode=reset generation={generation} pgid={pgid}");
                continue;
            }
            let dropping = mode == "drop" || mode == "reset";
            if mode == "sigkill" {
                tree.processes
                    .iter()
                    .find(|p| p.pid == owner.id() as i32)
                    .unwrap()
                    .signal(libc::SIGKILL);
            } else {
                writeln!(
                    owner.stdin.as_mut().unwrap(),
                    "{}",
                    if dropping { "drop" } else { mode }
                )
                .unwrap();
            }
            if dropping {
                loop {
                    line.clear();
                    assert!(output.read_line(&mut line).unwrap() > 0);
                    if line.starts_with("B3_DROPPED") {
                        break;
                    }
                }
                tree.assert_group_dead(pgid);
                deleted.event(None).await;
                assert!(!profile.exists());
                writeln!(owner.stdin.as_mut().unwrap(), "finish").unwrap();
                assert!(owner.wait().unwrap().success());
            } else {
                let status = owner.wait().unwrap();
                if mode == "exit74" {
                    assert_eq!(status.code(), Some(74));
                } else {
                    use std::os::unix::process::ExitStatusExt;
                    assert_eq!(status.signal(), Some(libc::SIGKILL));
                }
                tree.assert_group_dead(pgid);
            }
            // These invariants are checked AFTER termination, so reverting to
            // the old launcher fails after READY/owner death, never at startup.
            assert_eq!(pgid, pid);
            assert_eq!(sid, pid);
            println!("B3_GROUP_DEAD mode={mode} generation={generation} pgid={pgid}");
            break;
        }
        // Outer Drop separately reclaims escaped Crashpad/adopted helpers.
    }
}
fn ready(output: &mut impl BufRead, line: &mut String) {
    loop {
        line.clear();
        assert!(
            output.read_line(line).unwrap() > 0,
            "owner must navigate before termination"
        );
        print!("{line}");
        if line.starts_with("B3_READY ") {
            break;
        }
    }
}
