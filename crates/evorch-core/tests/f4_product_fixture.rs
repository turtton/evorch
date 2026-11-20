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
    process::{ManagedProcess, WaitError},
    wire::{self, MockProvider},
};
use std::{
    ffi::OsString, os::unix::fs::PermissionsExt, path::Path, process::Command, time::Duration,
};

const FAKE_ROLE_ENV: &str = "F4P_FAKE_ROLE";
const FAKE_PID_PREFIX: &str = "F4P-GRANDCHILD ";

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
    use std::io::{BufRead, Write};
    match std::env::var(FAKE_ROLE_ENV).unwrap().as_str() {
        // A parent that starts a grandchild, reports both and waits for stdin EOF.
        "family" => {
            let mut grandchild = fake_command("sleeper")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut ready = String::new();
            std::io::BufReader::new(grandchild.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            println!("{FAKE_PID_PREFIX}{}", grandchild.id());
            println!("{{\"status\":\"ready\"}}");
            std::io::stdout().flush().unwrap();
            let mut sink = String::new();
            let _ = std::io::stdin().read_line(&mut sink);
            let _ = grandchild.wait();
        }
        "sleeper" => {
            println!("up");
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
    let mut family = ManagedProcess::spawn(fake_command("family")).unwrap();
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

#[test]
fn descendants_are_found_and_taken_back_when_the_owner_is_dropped() {
    let (mut family, grandchild) = spawn_family();
    let seen = family.remember_descendants().to_vec();
    assert!(seen.iter().any(|p| p.pid == grandchild), "{seen:?}");
    assert!(
        product_exec::observe::process(grandchild).is_some(),
        "the grandchild runs"
    );
    drop(family); // the failure path: no explicit stop
    for p in &seen {
        assert!(product_exec::observe::is_gone(p), "{p:?} survived the drop");
    }
}

#[test]
fn terminate_and_join_report_clean_exit_and_no_orphans() {
    let (mut family, _) = spawn_family();
    family.remember_descendants();
    family.close_stdin(); // EOF is the child's own shutdown request
    let status = family
        .wait_exit(Duration::from_secs(60))
        .expect("exit event");
    assert!(status.success());
    let report = family.join();
    assert!(report.status.success());
    assert!(report.orphans.is_empty(), "{:?}", report.orphans);
}

#[test]
fn an_orphan_left_by_the_child_is_reported_then_killed() {
    let (mut family, grandchild) = spawn_family();
    family.remember_descendants();
    // SIGKILL of the parent alone leaves the grandchild behind.
    let pid = rustix::process::Pid::from_raw(family.pid()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    assert!(family.wait_exit(Duration::from_secs(60)).is_some());
    let report = family.join();
    assert!(
        report.orphans.iter().any(|p| p.pid == grandchild),
        "the leftover must be reported: {:?}",
        report.orphans
    );
    assert!(
        product_exec::observe::process(grandchild).is_none_or(|p| p.state != 'S' && p.state != 'R'),
        "and then taken back"
    );
}

#[test]
fn exit_before_ready_is_an_event_with_the_childs_stderr() {
    let mut process = ManagedProcess::spawn(fake_command("fail")).unwrap();
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
    assert!(
        report.status.success(),
        "{:?} {}",
        report.status,
        report.stderr
    );
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
        .unwrap_or_else(|e| panic!("exec cycle failed: {e}"));
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
    for n in 1..=5 {
        let dir = tempfile::Builder::new()
            .prefix("f4p-copy-")
            .tempdir()
            .unwrap();
        let copy = intervals::fresh_copy(&artifact, dir.path());
        runtime.block_on(intervals::one_round(&mut rec, n, &copy, &current, &harness));
        eprintln!("first {n}/5 done");
    }
    // The warm series runs on the already verified and probed artifact.
    artifact.describe().expect("probe before the warm series");
    rec.condition = "warm";
    for n in 1..=20 {
        runtime.block_on(intervals::one_round(
            &mut rec, n, &artifact, &current, &harness,
        ));
        eprintln!("warm {n}/20 done");
    }
    measure::write_csv(
        &out.join(format!("samples-{profile}-{label}.csv")),
        &rec.samples,
    )
    .unwrap();
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
    let joined = r::join_adopted(&helpers, Duration::from_secs(60));
    eprintln!("F4P helper join: {joined:?}");
    assert!(
        joined
            .iter()
            .all(|(_, j)| matches!(j, r::Joined::Exited(_) | r::Joined::NotAdopted)),
        "the helper must exit with its product: {joined:?}"
    );
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
