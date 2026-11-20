#![cfg(target_os = "linux")]
//! F4-P: the product fixture itself.
//!
//! Evidence class of every test here: `fixture`. They show that the fixture
//! starts a separately built, feature-free `evorch-core` through its ordinary
//! entry, observes it from outside, fails when the artifact is missing, and
//! takes everything back. They are NOT product-to-product evidence: nothing
//! here asks a product to restart into another product (that is F4-E, after 5a).
//!
//! Tests that need the product artifact fail when it is not provided; they are
//! never skipped and never run against `CARGO_BIN_EXE_*`.
#[path = "../shared-test-support/product_exec.rs"]
mod product_exec;

use product_exec::{
    artifact::{ArtifactError, ProductArtifact, ProductManifest},
    env::IsolatedEnv,
    launch::{LaunchError, ProductLaunch},
    names::{CARGO_HARNESS_BIN_ENV, ENV_MEASURE_MANIFEST, MANIFEST_SCHEMA},
    process::{LaunchSpec, ManagedProcess, WaitError},
    wire::{self, MockProvider},
};
use std::{
    ffi::OsString,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::Path,
    process::Command,
    time::Duration,
};

const FAKE_ROLE_ENV: &str = "F4P_FAKE_ROLE";
const FAKE_PID_PREFIX: &str = "F4P-GRANDCHILD ";
const FAKE_FDS_PREFIX: &str = "F4P-FDS ";

// ---------------------------------------------------------------- artifact

fn sample_manifest(elf: &Path) -> ProductManifest {
    ProductManifest {
        schema: MANIFEST_SCHEMA,
        package: "evorch-core".into(),
        bin: "evorch-core".into(),
        profile: "release".into(),
        cargo_args: [
            "build",
            "--release",
            "-p",
            "evorch-core",
            "--bin",
            "evorch-core",
        ]
        .map(String::from)
        .to_vec(),
        features: vec![],
        rustc: "rustc test".into(),
        cargo: "cargo test".into(),
        target: "x86_64-unknown-linux-gnu".into(),
        source_revision: "test".into(),
        artifact: elf.file_name().unwrap().to_string_lossy().into_owned(),
        elf_size: std::fs::metadata(elf).unwrap().len(),
        elf_sha256: core_instance::handoff::elf_identity(elf).unwrap(),
        stripped: false,
    }
}
fn fake_elf(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("fake-core");
    std::fs::write(&path, b"\x7fELF fixture-only bytes, never executed").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
fn write_manifest(dir: &Path, manifest: &ProductManifest) -> std::path::PathBuf {
    let path = dir.join("manifest.json");
    std::fs::write(&path, serde_json::to_vec_pretty(manifest).unwrap()).unwrap();
    path
}

#[test]
fn artifact_absent_is_a_failure_and_never_falls_back_to_cargo_bin_exe() {
    // Even when Cargo's own build of the binary is advertised, no manifest
    // means no product.
    let lookup = |name: &str| (name == CARGO_HARNESS_BIN_ENV).then(|| OsString::from("/bin/sh"));
    assert!(matches!(
        ProductArtifact::open_measurement_from(&lookup),
        Err(ArtifactError::NotProvided(ENV_MEASURE_MANIFEST))
    ));
    let empty = |_: &str| None;
    assert!(matches!(
        ProductArtifact::open_measurement_from(&empty),
        Err(ArtifactError::NotProvided(_))
    ));
    let blank = |name: &str| (name == ENV_MEASURE_MANIFEST).then(OsString::new);
    assert!(matches!(
        ProductArtifact::open_measurement_from(&blank),
        Err(ArtifactError::NotProvided(_))
    ));
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("manifest.json");
    assert!(matches!(
        ProductArtifact::open(&missing, &empty),
        Err(ArtifactError::Io(_))
    ));
}

#[test]
fn require_panics_in_a_process_that_has_cargo_bin_exe_but_no_sealed_artifact() {
    // The fixture's own entry point, in a child with the harness variable set
    // and the sealed-artifact variables absent: it must fail, not choose the
    // harness build.
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--ignored", "--exact", "fake_role_require", "--nocapture"])
        .env_clear()
        .env(FAKE_ROLE_ENV, "require")
        .env(
            CARGO_HARNESS_BIN_ENV,
            std::env::var_os(CARGO_HARNESS_BIN_ENV).unwrap_or_else(|| "/bin/sh".into()),
        );
    let output = child.output().unwrap();
    assert!(!output.status.success(), "the fixture must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("EVORCH_PRODUCT_CORE_EXE is not set"),
        "{stderr}"
    );
}
#[test]
#[ignore = "child role of require_panics_in_a_process_that_has_cargo_bin_exe_but_no_sealed_artifact"]
fn fake_role_require() {
    assert_eq!(std::env::var(FAKE_ROLE_ENV).unwrap(), "require");
    let _ = ProductArtifact::require();
}

#[test]
fn manifest_rules_reject_what_is_not_a_product() {
    let dir = tempfile::tempdir().unwrap();
    let elf = fake_elf(dir.path());
    let good = sample_manifest(&elf);
    ProductArtifact::check_manifest(&good).unwrap();
    let mut debug = good.clone();
    debug.profile = "debug".into();
    debug.cargo_args = ["build", "-p", "evorch-core", "--bin", "evorch-core"]
        .map(String::from)
        .to_vec();
    ProductArtifact::check_manifest(&debug).unwrap();
    let mutate = |f: &dyn Fn(&mut ProductManifest)| {
        let mut m = good.clone();
        f(&mut m);
        assert!(ProductArtifact::check_manifest(&m).is_err());
    };
    mutate(&|m| m.features = vec!["bootstrap-test-support".into()]);
    mutate(&|m| m.cargo_args.push("--features=renderer-test-helper".into()));
    mutate(&|m| m.cargo_args.push("--all-features".into()));
    mutate(&|m| m.stripped = true);
    mutate(&|m| m.profile = "debug".into());
    mutate(&|m| m.bin = "core-bootstrap-harness".into());
    mutate(&|m| m.cargo_args = vec!["build".into(), "--release".into()]);
    mutate(&|m| m.schema += 1);
    mutate(&|m| m.elf_sha256 = "md5:0".into());
}

#[test]
fn artifact_must_match_its_manifest_and_must_not_be_the_harness_build() {
    let dir = tempfile::tempdir().unwrap();
    let elf = fake_elf(dir.path());
    let manifest = sample_manifest(&elf);
    let path = write_manifest(dir.path(), &manifest);
    let none = |_: &str| None;
    let artifact = ProductArtifact::open(&path, &none).expect("a consistent artifact opens");
    assert_eq!(artifact.build_identity(), manifest.elf_sha256);
    artifact.verify_hash().unwrap();
    // Cargo's own build of the harness, handed over as the product by mistake.
    let harness =
        |name: &str| (name == CARGO_HARNESS_BIN_ENV).then(|| elf.clone().into_os_string());
    assert!(matches!(
        ProductArtifact::open(&path, &harness),
        Err(ArtifactError::Mismatch(m)) if m.contains(CARGO_HARNESS_BIN_ENV)
    ));
    // Wrong size, then wrong content of the same size.
    let mut wrong = manifest.clone();
    wrong.elf_size += 1;
    let bad = write_manifest(dir.path(), &wrong);
    assert!(matches!(
        ProductArtifact::open(&bad, &none),
        Err(ArtifactError::Mismatch(_))
    ));
    write_manifest(dir.path(), &manifest); // the consistent manifest again
    std::fs::write(&elf, b"\x7fELF fixture-only bytes, never executeD").unwrap();
    assert!(matches!(
        ProductArtifact::open(&path, &none),
        Err(ArtifactError::Mismatch(m)) if m.contains("hash")
    ));
    // A modification after opening is caught at launch.
    std::fs::write(&elf, b"\x7fELF fixture-only bytes, never executed").unwrap();
    let again = ProductArtifact::open(&path, &none).unwrap();
    std::fs::write(&elf, b"\x7fELF fixture-only bytes, never executeD").unwrap();
    let caught = std::panic::catch_unwind(|| again.assert_unchanged());
    assert!(caught.is_err(), "a changed artifact must not be launched");
}

#[test]
fn compile_time_environment_macros_are_not_used_by_the_fixture() {
    // F-10: read the sources at run time. The patterns are assembled so that
    // this test does not match itself.
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let banned = [
        format!("{}!(\"CARGO_MANIFEST_DIR", "env"),
        format!("{}!(\"CARGO_BIN_EXE", "env"),
        format!("{}!(\"CARGO_BIN_EXE", "option_env"),
        format!("{}!(\"CARGO_MANIFEST_DIR", "option_env"),
    ];
    let mut files = vec![
        root.join("tests/f4_product_fixture.rs"),
        root.join("shared-test-support/product_exec.rs"),
    ];
    for dir in [
        "shared-test-support/product_exec",
        "tests/renderer_support/product_exec",
    ] {
        if let Ok(entries) = std::fs::read_dir(root.join(dir)) {
            files.extend(entries.flatten().map(|e| e.path()));
        }
    }
    files.push(root.join("tests/renderer_support/product_exec.rs"));
    let mut scanned = 0;
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        scanned += 1;
        for pattern in &banned {
            assert!(
                !text.contains(pattern.as_str()),
                "{} uses {pattern}",
                file.display()
            );
        }
    }
    assert!(scanned >= 8, "the fixture sources were not found");
}

// ---------------------------------------------- supervision and cleanup

/// Reads the child's stdout up to its `up` line. libtest prints its own lines
/// first, so the first line is not the signal.
fn await_up(out: std::process::ChildStdout) {
    use std::io::BufRead;
    for line in std::io::BufReader::new(out).lines() {
        if line.unwrap().trim() == "up" {
            return;
        }
    }
    panic!("the child ended before saying up");
}

fn fake_spec(role: &str) -> LaunchSpec {
    let mut spec = LaunchSpec::new(std::env::current_exe().unwrap());
    spec.args(["--ignored", "--exact", "fake_role_process", "--nocapture"])
        .env(FAKE_ROLE_ENV, role);
    spec
}

fn fake_command(role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--ignored", "--exact", "fake_role_process", "--nocapture"])
        .env(FAKE_ROLE_ENV, role);
    command
}

#[test]
#[ignore = "child process of the supervision tests; selected by F4P_FAKE_ROLE"]
fn fake_role_process() {
    use std::io::Write;
    match std::env::var(FAKE_ROLE_ENV).unwrap().as_str() {
        // A parent that starts a grandchild, reports both and waits for stdin EOF.
        "family" => {
            let child = std::env::var("F4P_FAMILY_CHILD").unwrap_or_else(|_| "sleeper".into());
            // A parker never ends by itself: it is left behind on purpose.
            #[allow(clippy::zombie_processes)]
            let mut grandchild = fake_command(&child)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            await_up(grandchild.stdout.take().unwrap());
            println!("{FAKE_PID_PREFIX}{}", grandchild.id());
            println!("{{\"status\":\"ready\"}}");
            std::io::stdout().flush().unwrap();
            let mut sink = String::new();
            let _ = std::io::stdin().read_line(&mut sink);
            // A parker never ends by itself: it is left behind on purpose.
            if child == "sleeper" {
                let _ = grandchild.wait();
            }
        }
        // Runs until it is killed. No stdin, no timer: only a signal ends it.
        "parker" => {
            println!("up");
            std::io::stdout().flush().unwrap();
            loop {
                std::thread::park();
            }
        }
        // Starts a parker, reports its pid and exits at once, so that the
        // parker is orphaned before anything has observed it.
        // F4P_ORPHAN_MODE=env: the parker inherits the family's environment.
        // F4P_ORPHAN_MODE=setsid: it clears the environment and leaves the
        // session and process group, as the renderer helper does.
        // F4P_ORPHAN_MODE=leak: like setsid, and it keeps this role's stdout
        // pipe open.
        "orphan_maker" => {
            let mode = std::env::var("F4P_ORPHAN_MODE").unwrap();
            let mut command = fake_command("parker");
            command.stdin(std::process::Stdio::null());
            if mode != "env" {
                command.env_clear().env(FAKE_ROLE_ENV, "parker");
                // SAFETY: setsid is async-signal-safe.
                unsafe {
                    command.pre_exec(|| {
                        rustix::process::setsid()
                            .map(|_| ())
                            .map_err(std::io::Error::from)
                    });
                }
            }
            if mode == "leak" {
                command.stdout(std::process::Stdio::inherit());
            } else {
                command.stdout(std::process::Stdio::piped());
            }
            // Deliberately never waited for: the orphan is the point.
            #[allow(clippy::zombie_processes)]
            let mut grandchild = command.spawn().unwrap();
            if let Some(out) = grandchild.stdout.take() {
                await_up(out);
            }
            println!("{FAKE_PID_PREFIX}{}", grandchild.id());
            std::io::stdout().flush().unwrap();
            // The Child handle is dropped without a wait: an orphan.
        }
        "sleeper" => {
            println!("up");
            std::io::stdout().flush().unwrap();
            let mut sink = String::new();
            let _ = std::io::stdin().read_line(&mut sink);
        }
        // Lists its own descriptors above stderr, then waits for stdin EOF.
        "fds" => {
            let mut listed = Vec::new();
            for entry in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
                let fd: i32 = entry.file_name().to_str().unwrap().parse().unwrap();
                if fd > 2
                    && let Ok(target) = std::fs::read_link(entry.path())
                {
                    listed.push(target.to_string_lossy().into_owned());
                }
            }
            println!(
                "{FAKE_FDS_PREFIX}{}",
                serde_json::to_string(&listed).unwrap()
            );
            std::io::stdout().flush().unwrap();
            let mut sink = String::new();
            let _ = std::io::stdin().read_line(&mut sink);
        }
        // Exits at once with a message.
        "fail" => {
            eprintln!("fixture-fake: failing before ready");
            std::process::exit(7);
        }
        other => panic!("unknown role {other}"),
    }
}

fn spawn_family() -> (ManagedProcess, i32) {
    spawn_family_of("sleeper")
}

fn spawn_family_of(child: &str) -> (ManagedProcess, i32) {
    let mut command = fake_spec("family");
    command.env("F4P_FAMILY_CHILD", child);
    let mut family = ManagedProcess::spawn(command).unwrap();
    let (grandchild, _) = family
        .wait_for(Duration::from_secs(60), |line| {
            line.strip_prefix(FAKE_PID_PREFIX)?
                .trim()
                .parse::<i32>()
                .ok()
        })
        .unwrap();
    family
        .wait_for(Duration::from_secs(60), |line| {
            line.contains("ready").then_some(())
        })
        .unwrap();
    (family, grandchild)
}

/// A family whose leader starts a parker and exits at once: the parker is an
/// orphan before anything has looked at it. Returns once the leader's exit
/// notification arrived, which the reaper sends after the kernel reparented
/// the parker to it.
fn orphan_maker(mode: &str) -> (ManagedProcess, i32) {
    let mut command = fake_spec("orphan_maker");
    command.env("F4P_ORPHAN_MODE", mode);
    let mut maker = ManagedProcess::spawn(command).unwrap();
    let (grandchild, _) = maker
        .wait_for(Duration::from_secs(60), |line| {
            line.strip_prefix(FAKE_PID_PREFIX)?
                .trim()
                .parse::<i32>()
                .ok()
        })
        .unwrap();
    let status = maker
        .wait_exit(Duration::from_secs(60))
        .expect("exit event");
    assert!(status.success());
    (maker, grandchild)
}

#[test]
fn a_descendant_orphaned_before_any_observation_is_reclaimed_through_the_reaper() {
    let (maker, grandchild) = orphan_maker("env");
    let adopted = product_exec::observe::process(grandchild).expect("the orphan runs");
    assert_eq!(
        adopted.ppid,
        maker.reaper_pid(),
        "the family's reaper adopted it, not the harness"
    );
    assert_ne!(adopted.ppid, std::process::id() as i32);
    let report = maker.join();
    assert!(!report.reaper_lost, "{}", report.reaper_stderr);
    assert!(
        report.orphans.iter().any(|p| p.pid == grandchild),
        "{:?}",
        report.orphans
    );
    assert!(
        product_exec::observe::process(grandchild).is_none(),
        "reaped"
    );
}

#[test]
fn a_session_leaving_orphan_with_a_cleared_environment_is_reclaimed_through_the_reaper() {
    let (maker, grandchild) = orphan_maker("setsid");
    let orphan = product_exec::observe::process(grandchild).expect("the orphan runs");
    assert_eq!(
        orphan.pgid, grandchild,
        "it leads a group of its own, like a helper"
    );
    assert_eq!(orphan.ppid, maker.reaper_pid());
    let report = maker.join();
    assert!(!report.reaper_lost, "{}", report.reaper_stderr);
    assert!(report.orphans.iter().any(|p| p.pid == grandchild));
    assert!(
        product_exec::observe::process(grandchild).is_none(),
        "reaped"
    );
}

#[test]
fn a_family_member_that_holds_the_output_pipe_is_killed_so_the_readers_end() {
    let (maker, grandchild) = orphan_maker("leak");
    let report = maker.join();
    assert_eq!(
        report.unjoined_readers, 0,
        "the family's own holder is gone"
    );
    assert!(report.orphans.iter().any(|p| p.pid == grandchild));
    assert!(product_exec::observe::process(grandchild).is_none());
}

#[test]
fn a_reader_held_open_by_a_stranger_is_bounded_and_the_stranger_is_left_alone() {
    let (mut family, _) = spawn_family();
    family.set_reader_bound(Duration::from_millis(200));
    // A process of the harness that is not in the family opens the write end of
    // the leader's stdout pipe through /proc, as a log reader or a debugger
    // attached to the descriptor could.
    let held = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/proc/{}/fd/1", family.pid()))
        .unwrap();
    let mut stranger = fake_command("parker");
    stranger
        .stdin(std::process::Stdio::null())
        .stdout(held)
        .stderr(std::process::Stdio::null());
    let stranger = Parker(stranger.spawn().unwrap());
    family.close_stdin();
    assert!(family.wait_exit(Duration::from_secs(60)).unwrap().success());
    let report = family.join();
    assert!(report.unjoined_readers >= 1, "{report:?}");
    assert!(!report.reaper_lost);
    assert!(
        running(stranger.id()),
        "a process outside the family is never touched"
    );
}

#[test]
fn the_exited_leader_keeps_its_pid_reserved_until_the_cleanup() {
    let (mut family, _) = spawn_family();
    family.close_stdin();
    assert!(family.wait_exit(Duration::from_secs(60)).unwrap().success());
    // Exit was observed without reaping: the pid (and the group id) cannot be
    // reused, so a signal or a group kill can only reach this process.
    let zombie = product_exec::observe::process(family.pid()).expect("not reaped yet");
    assert_eq!(zombie.state, 'Z');
    let leader = family.pid();
    let report = family.join();
    assert!(report.success());
    assert!(
        product_exec::observe::process(leader).is_none(),
        "reaped by the cleanup"
    );
}

#[test]
fn descendants_are_taken_back_when_the_owner_is_dropped() {
    let (family, grandchild) = spawn_family();
    let seen = family.descendants();
    assert!(seen.iter().any(|p| p.pid == grandchild), "{seen:?}");
    assert!(product_exec::observe::process(grandchild).is_some());
    drop(family); // the failure path: no explicit stop
    for p in &seen {
        assert!(product_exec::observe::is_gone(p), "{p:?} survived the drop");
    }
}

#[test]
fn terminate_and_join_report_clean_exit_and_no_orphans() {
    let (mut family, _) = spawn_family();
    family.close_stdin(); // EOF is the child's own shutdown request
    let status = family
        .wait_exit(Duration::from_secs(60))
        .expect("exit event");
    assert!(status.success());
    let report = family.join();
    assert!(report.success());
    assert!(report.orphans.is_empty(), "{:?}", report.orphans);
    assert!(!report.reaper_lost, "{}", report.reaper_stderr);
}

#[test]
fn an_orphan_left_by_the_child_is_reported_then_killed() {
    let (mut family, grandchild) = spawn_family_of("parker");
    // SIGKILL of the leader alone leaves the grandchild behind.
    assert!(family.signal(rustix::process::Signal::KILL));
    assert!(family.wait_exit(Duration::from_secs(60)).is_some());
    let report = family.join();
    assert!(
        report.orphans.iter().any(|p| p.pid == grandchild),
        "the leftover must be reported: {:?}",
        report.orphans
    );
    assert!(
        product_exec::observe::process(grandchild).is_none(),
        "and then taken back"
    );
}

#[test]
fn exit_before_ready_is_an_event_with_the_childs_stderr() {
    let mut process = ManagedProcess::spawn(fake_spec("fail")).unwrap();
    match process.wait_for(Duration::from_secs(60), |l| {
        l.contains("ready").then_some(())
    }) {
        Err(WaitError::Exited(status, stderr)) => {
            assert_eq!(status.code(), Some(7));
            assert!(stderr.contains("failing before ready"), "{stderr}");
        }
        other => panic!("expected the exit event, got {other:?}"),
    }
    let report = process.join();
    assert!(report.orphans.is_empty());
}

#[test]
fn isolated_environment_removes_every_directory() {
    let env = IsolatedEnv::new();
    let footprint = env.footprint();
    std::fs::write(env.config_dir().join("marker"), b"x").unwrap();
    assert!(footprint.remains().len() == 2);
    drop(env);
    assert!(footprint.remains().is_empty(), "{:?}", footprint.remains());
}

// ----------------------------------------------------------- the product

#[test]
fn a_fresh_copy_of_the_sealed_product_keeps_its_closure_and_starts() {
    let artifact = ProductArtifact::require();
    let dir = tempfile::tempdir().unwrap();
    let copy = product_exec::intervals::fresh_copy(&artifact, dir.path());
    let sealed = copy
        .sealed
        .as_ref()
        .expect("a sealed product is copied sealed");
    let guard = sealed
        .guard
        .as_ref()
        .expect("the helper and its bundle are copied with the core on Linux");
    assert_ne!(copy.path, artifact.path, "a copy has a path of its own");
    assert_eq!(
        copy.path.parent(),
        Some(dir.path().canonicalize().unwrap().as_path())
    );
    assert_eq!(guard.helper.parent(), copy.path.parent());
    assert_eq!(guard.bundle.parent(), copy.path.parent());
    assert_eq!(sealed.manifest_path.parent(), copy.path.parent());
    assert_eq!(copy.manifest.elf_sha256, artifact.manifest.elf_sha256);

    let env = IsolatedEnv::new();
    let provider = MockProvider::scripted(vec![mock_openai::ScriptedResponse::text_stream(
        "f4p-copy",
        "mock-model",
        ["observed"],
    )]);
    env.write_product_config(&provider.base_url(), product_exec::launch::KEY_ENV);
    let mut product = ProductLaunch::new(&copy, &env).start().unwrap();
    product.wait_ready().expect("the copy reports Ready");
    let observed = product.observe();
    assert!(observed.exe_is_artifact, "{:?}", observed.exe);
    let described = copy.describe().expect("--describe-handoff of the copy");
    assert_eq!(described.build_identity, observed.artifact_identity);
}

#[test]
fn product_starts_isolated_is_observed_serves_the_wire_and_is_taken_back() {
    let artifact = ProductArtifact::require();
    let env = IsolatedEnv::new();
    let provider = MockProvider::scripted(vec![mock_openai::ScriptedResponse::text_stream(
        "f4p-1",
        "mock-model",
        ["observed"],
    )]);
    env.write_product_config(&provider.base_url(), product_exec::launch::KEY_ENV);
    let mut product = ProductLaunch::new(&artifact, &env).start().unwrap();
    let ready = product.wait_ready().expect("the product reports Ready");
    let observed = product.observe();

    // PID and image: the very binary of the manifest, by device and inode.
    assert_eq!(observed.pid, product.pid());
    assert!(observed.exe_is_artifact, "{:?}", observed.exe);
    // Build identity: the manifest's hash is the hash the product describes.
    let described = artifact.describe().expect("--describe-handoff");
    assert_eq!(described.build_identity, observed.artifact_identity);
    // The product runs in the isolated directories only.
    assert!(
        ready.socket.starts_with(env.runtime_dir()),
        "{:?}",
        ready.socket
    );
    assert!(env.ownership_db().exists() || env.state_dir().join("evorch").exists());
    // Descriptors: a listener socket exists, and nothing of the test's own
    // (other than stdio) was inherited.
    let inherited: Vec<_> = observed
        .fds
        .iter()
        .filter(|f| !f.cloexec && f.fd > 2)
        .collect();
    assert!(
        inherited.is_empty(),
        "descriptors leaked into the product: {inherited:?}"
    );
    assert!(observed.fds.iter().any(|f| f.kind() == "socket"));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let hello_epoch = runtime.block_on(async {
        let client = wire::connect(&ready.socket, "f4p-observer").await;
        let hello = wire::HelloSummary::from(client.server_hello());
        let mut events = wire::subscribe_all(&client).await;
        let reply = client
            .command(wire::send_chat("f4p-thread", "f4p-submission", "hello"))
            .await
            .expect("the product accepts a chat on its public wire");
        let run = reply.run_id.expect("a run").as_str().to_owned();
        wire::until_waiting(&mut events, &run).await.unwrap();
        hello.epoch
    });
    assert!(hello_epoch >= 1, "epoch {hello_epoch}");
    // Provider wire: exactly the request the product sent for that chat.
    let sent = provider.chat_requests();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].body["model"], "mock-model");
    assert_eq!(provider.remaining_scripts(), 0);

    let (report, footprint) = product.stop();
    assert!(report.success(), "{:?} {}", report.status, report.stderr);
    assert!(report.orphans.is_empty());
    drop(env);
    assert!(footprint.remains().is_empty());
    // The core's lifetime lock is free again: nothing of it survived.
    let _ = LaunchError::BadReadyLine(String::new());
}

#[test]
fn product_that_cannot_start_fails_with_its_exit_and_leaves_nothing() {
    let artifact = ProductArtifact::require();
    let env = IsolatedEnv::new();
    let footprint = env.footprint();
    let mut product = ProductLaunch::new(&artifact, &env)
        .args(["--no-such-option"])
        .start()
        .unwrap();
    match product.wait_ready() {
        Err(LaunchError::Wait(WaitError::Exited(status, stderr))) => {
            assert_eq!(status.code(), Some(1));
            assert!(stderr.contains("usage: evorch-core"), "{stderr}");
        }
        other => panic!("expected an exit before Ready, got {other:?}"),
    }
    let (report, _) = product.finish();
    assert!(report.orphans.is_empty());
    drop(env);
    assert!(footprint.remains().is_empty());
}

// ------------------------------------------------------ artifact tooling

/// MEASUREMENT ONLY. The sealed contract (scripts/product-artifact.sh) covers
/// the release build; this writes a copy and a measurement-only manifest for
/// the DEBUG build built with `cargo build -p evorch-core --bin evorch-core`.
/// The copy is not stripped or modified.
#[test]
#[ignore = "tooling: writes an artifact copy and manifest from an already built binary"]
fn write_measurement_manifest_for_built_artifact() {
    use product_exec::names::*;
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    let elf = std::path::PathBuf::from(var(ENV_BUILD_ELF));
    let out = std::path::PathBuf::from(var(ENV_ARTIFACT_DIR));
    std::fs::create_dir_all(&out).unwrap();
    let copy = out.join("evorch-core");
    std::fs::copy(&elf, &copy).unwrap();
    let args = var(ENV_BUILD_ARGS);
    let args: Vec<&str> = args.split_whitespace().collect();
    let manifest = product_exec::artifact::describe_build(
        &copy,
        &var(ENV_BUILD_PROFILE),
        &args,
        "evorch-core",
    )
    .unwrap();
    assert_eq!(
        manifest.elf_sha256,
        core_instance::handoff::elf_identity(&elf).unwrap(),
        "the copy is byte-identical to the build"
    );
    ProductArtifact::check_manifest(&manifest).unwrap();
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    eprintln!("wrote {}", out.join("manifest.json").display());
}

// ------------------------------------------- harness -> product (exec)

/// Entered by `exec_cycle` in a child of this binary: the OLD core stand-in.
#[test]
#[ignore = "worker of the harness->product exec tests; needs EVORCH_MASKED_TEST_CHILD"]
fn f4p_old_core_worker() {
    product_exec::old_core::run_worker();
}

/// EVIDENCE CLASS: harness -> product. The old side is this test binary, not a
/// product. It shows that the fixture can observe a real `execve` (same PID,
/// image replaced by the artifact) and the successor's Ready and owner claim,
/// synchronized by the exec pipe EOF and the readiness line.
#[test]
fn harness_exec_into_the_product_is_observed_without_a_clock_guess() {
    let artifact = ProductArtifact::require();
    let cycle = product_exec::old_core::exec_cycle(&artifact, &std::env::current_exe().unwrap())
        .unwrap_or_else(|(e, after)| panic!("exec cycle failed after {after:?}: {e}"));
    assert!(
        cycle.exe_replaced,
        "the same PID now runs the artifact image"
    );
    assert!(cycle.exec_to_ready > Duration::ZERO);
    assert!(cycle.old_epoch >= 1);
    eprintln!("F4P cycle: {cycle:?}");
}

// ------------------------------------------------------------ measurement

/// The measurement of `measurements/f4-product/conditions.md`: 5 first samples
/// on fresh copies, then 20 warm samples. Writes raw samples and a summary to
/// `$EVORCH_F4P_MEASURE_OUT`. It asserts nothing about latency.
#[test]
#[ignore = "measurement: run through scripts/reproduce-under-limits.sh, see measurements/f4-product/conditions.md"]
fn measure_f4_product_intervals() {
    use product_exec::{intervals, measure, names::*};
    let out = std::path::PathBuf::from(
        std::env::var_os(ENV_MEASURE_OUT)
            .unwrap_or_else(|| panic!("{ENV_MEASURE_OUT} is required")),
    );
    let label = std::env::var(ENV_MEASURE_LABEL)
        .unwrap_or_else(|_| panic!("{ENV_MEASURE_LABEL} is required"));
    let label: &'static str = Box::leak(label.into_boxed_str());
    let artifact = ProductArtifact::require_for_measurement();
    let profile = artifact.manifest.profile.clone();
    std::fs::create_dir_all(&out).unwrap();
    let harness = std::env::current_exe().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let current = intervals::current_elf();
    let mut rec = intervals::Recorder::new(&profile, "first");
    // Saved after every round: a failure or hang keeps what was measured so far.
    let csv = out.join(format!("samples-{profile}-{label}.csv"));
    for n in 1..=5 {
        let dir = tempfile::Builder::new()
            .prefix("f4p-copy-")
            .tempdir()
            .unwrap();
        let copy = intervals::fresh_copy(&artifact, dir.path());
        runtime.block_on(intervals::one_round(&mut rec, n, &copy, &current, &harness));
        eprintln!("first {n}/5 done");
        measure::write_csv(&csv, &rec.samples).unwrap();
    }
    // The warm series runs on the already verified and probed artifact.
    artifact.describe().expect("probe before the warm series");
    rec.condition = "warm";
    for n in 1..=20 {
        runtime.block_on(intervals::one_round(
            &mut rec, n, &artifact, &current, &harness,
        ));
        eprintln!("warm {n}/20 done");
        measure::write_csv(&csv, &rec.samples).unwrap();
    }
    measure::write_csv(&csv, &rec.samples).unwrap();
    let mut summary =
        String::from("profile,condition,interval,n,failures,min_ms,p50_ms,p95_ms,max_ms\n");
    for condition in ["first", "warm"] {
        let mut intervals: Vec<&str> = rec.samples.iter().map(|s| s.interval).collect();
        intervals.dedup();
        let mut seen = Vec::new();
        for interval in intervals {
            if seen.contains(&interval) {
                continue;
            }
            seen.push(interval);
            let group: Vec<_> = rec
                .samples
                .iter()
                .filter(|s| s.condition == condition && s.interval == interval)
                .collect();
            match measure::summarize(&group) {
                Some(s) => summary.push_str(&format!(
                    "{profile},{condition},{interval},{},{},{:.3},{:.3},{:.3},{:.3}\n",
                    s.n, s.failures, s.min, s.p50, s.p95, s.max
                )),
                None => summary.push_str(&format!(
                    "{profile},{condition},{interval},{},{},,,,\n",
                    group.len(),
                    group.len()
                )),
            }
        }
    }
    std::fs::write(out.join(format!("summary-{profile}-{label}.csv")), &summary).unwrap();
    println!("{summary}");
}

// -------------------------------------------- renderer helper (product)

#[path = "renderer_support/product_exec.rs"]
mod renderer_product;

/// The product starts a renderer helper on a public command path and takes it
/// back when it stops. Evidence class: `fixture` for the product side; the
/// helper is the fake of the `renderer-test-helper` feature (a harness binary)
/// unless `EVORCH_F4P_RENDERER_HELPER` names another executable.
fn renderer_scenario() {
    use renderer_product as r;
    let artifact = ProductArtifact::require();
    let helper = r::helper_binary();
    let env = IsolatedEnv::new();
    let provider = MockProvider::scripted(vec![]);
    env.write_product_config(&provider.base_url(), product_exec::launch::KEY_ENV);
    let mut product = ProductLaunch::new(&artifact, &env)
        .renderer_helper(helper.clone())
        .start()
        .unwrap();
    product.wait_ready().expect("ready");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (helpers, client) =
        runtime.block_on(r::open_page(&mut product, &provider, &env.project_dir()));
    // One helper is the product's own child; any further process of the same
    // image is that helper's descendant (it is a process tree, not a thread).
    let own: Vec<_> = helpers.iter().filter(|h| h.ppid == product.pid()).collect();
    assert_eq!(own.len(), 1, "one helper child of the product: {helpers:?}");
    let helper_proc = own[0].clone();
    let observed = product.observe();
    assert!(observed.children.iter().any(|c| c.pid == helper_proc.pid));
    // The client connection is observable too: a second wire client sees the
    // renderer as Ready.
    runtime.block_on(async {
        use workbench_protocol::{
            namespaces::browser::{BrowserQuery, BrowserReply, RendererPhase},
            queries::{Query, QueryReply},
        };
        match client
            .query(Query::Browser(BrowserQuery::GetRendererStatus {}))
            .await
            .unwrap()
        {
            QueryReply::Browser(BrowserReply::Status(s)) => {
                assert_eq!(s.phase, RendererPhase::Ready)
            }
            other => panic!("{other:?}"),
        }
    });
    drop(client);
    // The product stops: SIGTERM, its exit notification, then the helper's.
    let status = product
        .process
        .terminate(product_exec::launch::EXIT_WATCHDOG)
        .expect("the product exits on SIGTERM");
    assert!(status.success(), "{status:?}");
    // The helper tree ends with its product: the family's reaper, which adopted it
    // when the product exited, waits for each process to end and reports it.
    let joined = product
        .process
        .join_family(Duration::from_secs(60))
        .expect("the helper must exit with its product");
    eprintln!("F4P helper join: {joined:?}");
    let (report, footprint) = product.finish();
    assert!(report.orphans.is_empty(), "{:?}", report.orphans);
    for helper in &helpers {
        assert!(product_exec::observe::is_gone(helper), "{helper:?}");
    }
    drop(env);
    assert!(footprint.remains().is_empty());
    assert_eq!(provider.remaining_scripts(), 0);
}

// The fake helper exists only with the `renderer-test-helper` feature, like the
// other renderer process tests; the runtime CI group enables it.
#[cfg(feature = "renderer-test-helper")]
#[test]
fn product_starts_and_takes_back_a_renderer_helper() {
    renderer_scenario();
}

/// The same scenario with the real `evorch-renderer` (CEF). Needs the helper
/// named by `EVORCH_F4P_RENDERER_HELPER` and a host where CEF can run
/// (docs/browser.md); it fails, not skips, when the helper is not provided.
#[test]
#[ignore = "needs the real evorch-renderer (CEF); run with the cef-e2e job's environment"]
fn product_starts_and_takes_back_the_real_renderer_helper() {
    use product_exec::names::ENV_RENDERER_HELPER;
    let helper = std::env::var_os(ENV_RENDERER_HELPER)
        .unwrap_or_else(|| panic!("{ENV_RENDERER_HELPER} must name the real evorch-renderer"));
    assert!(
        Path::new(&helper).is_file() && !helper.to_string_lossy().contains("test-helper"),
        "{helper:?} is not a real renderer"
    );
    renderer_scenario();
}

#[test]
fn descriptors_without_close_on_exec_stay_this_processes_own_business() {
    use std::os::fd::AsRawFd;
    // A runner can leave such a descriptor in the test process. Make one, and
    // name it by its pipe inode.
    let (reader, writer) = std::io::pipe().unwrap();
    rustix::io::fcntl_setfd(&writer, rustix::io::FdFlags::empty()).unwrap();
    let leaked = std::fs::read_link(format!("/proc/self/fd/{}", writer.as_raw_fd())).unwrap();
    let listed_by = |child: &mut ManagedProcess| {
        child
            .wait_for(Duration::from_secs(60), |line| {
                serde_json::from_str::<Vec<String>>(line.strip_prefix(FAKE_FDS_PREFIX)?).ok()
            })
            .unwrap()
            .0
    };
    // The supervised child does not get it.
    let mut child = ManagedProcess::spawn(fake_spec("fds")).unwrap();
    let listed = listed_by(&mut child);
    assert!(
        !listed.iter().any(|t| Path::new(t) == leaked),
        "{leaked:?} reached the supervised child: {listed:?}"
    );
    child.close_stdin();
    assert!(child.wait_exit(Duration::from_secs(60)).unwrap().success());
    assert!(child.join().orphans.is_empty());
    // This process's descriptor was not changed, so an unrelated child that is
    // meant to inherit it still does.
    let flags = rustix::io::fcntl_getfd(&writer).unwrap();
    assert!(
        !flags.contains(rustix::io::FdFlags::CLOEXEC),
        "the supervisor must not change this process's descriptor flags"
    );
    let mut unrelated = fake_command("fds");
    unrelated.stdin(std::process::Stdio::null());
    let output = unrelated.output().unwrap();
    let line = String::from_utf8_lossy(&output.stdout);
    let listed: Vec<String> = line
        .lines()
        .find_map(|l| serde_json::from_str(l.strip_prefix(FAKE_FDS_PREFIX)?).ok())
        .expect("the unrelated child listed its descriptors");
    assert!(
        listed.iter().any(|t| Path::new(t) == leaked),
        "an unrelated Command lost its intended inheritance: {listed:?}"
    );
    drop((reader, writer));
}

// ------------------------------------------- ownership and spawn failure

/// The reaper role of every supervised family: this test binary run again.
#[test]
#[ignore = "role: the reaper of one supervised family (product_exec::reaper)"]
fn f4p_family_reaper() {
    product_exec::reaper::run();
}

/// A process of the harness that is killed and reaped when it goes out of
/// scope, so that a failing assertion cannot leave it behind.
struct Parker(std::process::Child);
impl Parker {
    fn id(&self) -> u32 {
        self.0.id()
    }
}
impl Drop for Parker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn plain_parker(args: &[&str], cwd: Option<&Path>) -> Parker {
    let mut command = fake_command("parker");
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    Parker(command.spawn().unwrap())
}

fn running(pid: u32) -> bool {
    product_exec::observe::process(pid as i32).is_some_and(|p| p.state != 'Z')
}

fn finish_family(mut family: ManagedProcess) {
    family.close_stdin();
    assert!(family.wait_exit(Duration::from_secs(60)).unwrap().success());
    drop(family.join());
}

#[test]
fn processes_outside_the_family_are_never_touched_whatever_they_reference() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("family");
    let other = parent.path().join("family-other");
    let below = dir.join("sub");
    for d in [&dir, &other, &below] {
        std::fs::create_dir(d).unwrap();
    }
    let log = format!("{}/output.log", dir.display());
    // One started before the family, as a user's `tail -f <root>/output.log`.
    let before = plain_parker(&[&log], None);
    let (family, _) = spawn_family();
    // Three more start after the family is up: one names the log, one works in
    // a directory below the family's root, one in a sibling with the same
    // prefix. None is in the family's tree.
    let named = plain_parker(&[&log], None);
    let inside = plain_parker(&[], Some(&below));
    let sibling = plain_parker(&[], Some(&other));
    finish_family(family);
    for stray in [&before, &named, &inside, &sibling] {
        assert!(running(stray.id()), "pid {} was touched", stray.id());
    }
}

#[test]
fn the_orphan_of_another_family_is_not_touched_when_its_leader_has_exited() {
    // B's leader has exited and left an orphan that nobody else may claim: it
    // belongs to B's reaper alone, and is gone only when B is cleaned up.
    let (other, orphan) = orphan_maker("setsid");
    let (first, _) = spawn_family();
    finish_family(first);
    assert!(
        running(orphan as u32),
        "another family's orphan was touched"
    );
    let report = other.join();
    assert!(report.orphans.iter().any(|p| p.pid == orphan));
    assert!(product_exec::observe::process(orphan).is_none());
}

#[test]
fn another_live_family_that_names_this_familys_path_is_not_touched() {
    let root = tempfile::tempdir().unwrap();
    let log = format!("{}/output.log", root.path().display());
    let (first, _) = spawn_family();
    let mut command = fake_spec("parker");
    command.arg(&log);
    let second = ManagedProcess::spawn(command).unwrap();
    let second_pid = second.pid();
    finish_family(first);
    assert!(
        running(second_pid as u32),
        "the other family's tree is not ours"
    );
    drop(second);
    assert!(product_exec::observe::process(second_pid).is_none());
}

#[test]
fn a_failed_spawn_step_takes_the_started_family_back() {
    use product_exec::process::{LAST_ABORTED_SPAWN, SpawnFault};
    for fault in [
        SpawnFault::ReaperPidfd,
        SpawnFault::Pidfd,
        SpawnFault::PidfdClone,
    ] {
        let result = ManagedProcess::spawn_faulty(fake_spec("parker"), fault);
        assert!(result.is_err(), "{fault:?} must fail the spawn");
        let (reaper, leader) = LAST_ABORTED_SPAWN.with(std::cell::Cell::get);
        // With a failing reaper pidfd the leader's pid is whatever the reaper had
        // reported by then; it is 0 when it had not.
        assert!(reaper > 0 && (leader > 0 || fault == SpawnFault::ReaperPidfd));
        assert!(
            product_exec::observe::process(reaper).is_none(),
            "{fault:?}: the reaper {reaper} was not reaped"
        );
        assert!(
            leader == 0 || product_exec::observe::process(leader).is_none(),
            "{fault:?}: the leader {leader} was not killed and reaped"
        );
    }
}

#[test]
fn a_join_in_flight_while_the_family_is_killed_leaves_every_child_accounted_for() {
    // The reaper's JOIN thread and a KILL both reap children. Pinning a child by
    // pidfd and reaping it happen under one lock, so a pid that was listed is
    // still the same child when it is signalled; and whichever of the two reaps
    // a child, the other still learns how it ended (the reaper records the wait
    // status of every child it reaps). A reused pid cannot be forced from a test;
    // what is checked is the order the lock makes possible: the JOIN is pending
    // (an adopted orphan that never exits) when the KILL arrives, and the KILL
    // wakes it. Both kinds of orphan are covered: one that kept the family's
    // session, and one that left it with a cleared environment.
    for mode in ["env", "setsid"] {
        let (mut family, orphan) = orphan_maker(mode);
        let bystander = plain_parker(&[], None);
        assert!(family.request_join(), "{mode}: the JOIN is requested first");
        family.kill_all();
        let joined = family
            .await_join(Duration::from_secs(60))
            .unwrap_or_else(|| {
                panic!("{mode}: the pending JOIN must end once the KILL took the orphan")
            });
        assert!(
            joined
                .iter()
                .any(|(pid, status)| *pid == orphan && !status.success()),
            "{mode}: the orphan is accounted for as killed: {joined:?}"
        );
        assert!(
            joined.iter().all(|(pid, _)| *pid != bystander.id() as i32),
            "{mode}: {joined:?}"
        );
        assert!(
            product_exec::observe::process(orphan).is_none(),
            "{mode}: reaped"
        );
        let report = family.join();
        assert!(!report.reaper_lost, "{mode}: {}", report.reaper_stderr);
        assert!(
            running(bystander.id()),
            "{mode}: a process outside the family is untouched"
        );
    }
}

#[test]
fn a_reaper_that_dies_between_the_snapshot_and_the_kill_is_reported_lost() {
    let (mut family, grandchild) = spawn_family_of("parker");
    family.set_cleanup_bounds(Duration::from_millis(300), Duration::from_millis(300));
    family.set_reader_bound(Duration::from_millis(300));
    family.set_after_snapshot_hook(|reaper| {
        // Kill the reaper and wait until it is really dead, so that the next
        // control write meets a closed pipe instead of a slow reply.
        rustix::process::pidfd_send_signal(reaper, rustix::process::Signal::KILL).unwrap();
        let _ = rustix::process::waitid(
            rustix::process::WaitId::PidFd(reaper),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
        );
    });
    assert!(family.signal(rustix::process::Signal::KILL));
    let report = family.join();
    assert!(
        report.reaper_lost,
        "a failed control write is a lost reaper: {report:?}"
    );
    assert!(!report.success());
    // What the lost reaper could not take back is the test's to clean up.
    let orphan = rustix::process::Pid::from_raw(grandchild).unwrap();
    let _ = rustix::process::kill_process(orphan, rustix::process::Signal::KILL);
}

#[test]
fn a_reaper_that_stops_answering_is_bounded_and_reported_lost() {
    let (mut family, _) = spawn_family();
    family.set_cleanup_bounds(Duration::from_millis(300), Duration::from_millis(300));
    let reaper = rustix::process::Pid::from_raw(family.reaper_pid()).unwrap();
    // The reaper is this process's own child, still unreaped: the pid is its own.
    rustix::process::kill_process(reaper, rustix::process::Signal::STOP).unwrap();
    family.close_stdin();
    let report = family.join();
    assert!(report.reaper_lost, "{report:?}");
    assert!(!report.success());
}

#[test]
fn a_reaper_that_dies_before_the_exit_is_reported_lost_and_not_a_panic() {
    let (mut family, grandchild) = spawn_family_of("parker");
    // The orphaned parker keeps the leader's stderr open until the test kills it.
    family.set_reader_bound(Duration::from_millis(300));
    family.set_cleanup_bounds(Duration::from_millis(300), Duration::from_millis(300));
    let reaper = rustix::process::Pid::from_raw(family.reaper_pid()).unwrap();
    rustix::process::kill_process(reaper, rustix::process::Signal::KILL).unwrap();
    // The leader is killed through its own pidfd; nobody is left to report it.
    assert!(family.signal(rustix::process::Signal::KILL));
    let report = family.join();
    assert!(report.reaper_lost, "{report:?}");
    assert!(report.status.is_none());
    // What the lost reaper could not take back is the test's to clean up.
    let orphan = rustix::process::Pid::from_raw(grandchild).unwrap();
    let _ = rustix::process::kill_process(orphan, rustix::process::Signal::KILL);
}
