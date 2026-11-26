#![cfg(all(target_os = "linux", feature = "bootstrap-test-support"))]
//! Real exec through the successor pipeline, with IPC checkpoints instead of
//! elapsed-time assertions. The runner owns hang detection.
use core_instance::{ConfigIdentity, Instance, LaunchGuard, handoff::receipt::BootStatus};
use runtime::ownership::{OwnerState, Registry};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};
use storage::recovery::restart::RestartPhase;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, planned::PlannedStorage};

struct Fixture {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    data: tempfile::TempDir,
    sockets: test_socket_dir::SocketDir,
    owners: test_socket_dir::SocketDir,
}
impl Fixture {
    fn new(scenario: &str) -> Self {
        let data = tempfile::tempdir().unwrap();
        let sockets = test_socket_dir::SocketDir::new();
        let owners = test_socket_dir::SocketDir::new();
        std::fs::create_dir(data.path().join("project")).unwrap();
        let bin = data.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        let mut child =
            Command::new(std::env::var("CARGO_BIN_EXE_core-bootstrap-harness").unwrap())
                .env("PATH", path)
                .env("EVORCH_FIXTURE_LOCAL_VALUE", "unused")
                .env("EVORCH_PROBE_MARKER", data.path().join("probe-called"))
                .env(
                    "EVORCH_PROBE_EXIT",
                    if scenario == "probe_success" {
                        "0"
                    } else {
                        "9"
                    },
                )
                .arg("--config-dir")
                .arg(data.path().join("config"))
                .arg("--project")
                .arg(data.path().join("project"))
                .arg("--owners")
                .arg(owners.path())
                .arg("--ipc-parent")
                .arg(sockets.path())
                .arg("--scenario")
                .arg(scenario)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            data,
            sockets,
            owners,
        }
    }
    fn event(&mut self, status: &str) -> Value {
        let mut line = String::new();
        assert_ne!(self.output.read_line(&mut line).unwrap(), 0, "{status}");
        let event: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["status"], status);
        event
    }
    fn advance(&mut self) {
        self.input.write_all(b"continue\n").unwrap();
        self.input.flush().unwrap();
    }
    fn identity(&self) -> ConfigIdentity {
        ConfigIdentity::new(&self.data.path().join("config")).unwrap()
    }
    fn database(&self) -> storage::Database {
        storage::Database::open(&storage::StorageConfig {
            db_path: self.data.path().join("config/evorch-events.db"),
            ..Default::default()
        })
        .unwrap()
    }
    fn owner(&self) -> runtime::ownership::ThreadOwner {
        Registry::open_readonly(&self.owners.path().join("owners.db"))
            .unwrap()
            .attach("idle")
            .unwrap()
    }
    fn excludes_second_core(&self) {
        let launch = LaunchGuard::acquire(&self.identity()).unwrap();
        assert!(!launch.lifetime_available().unwrap());
        assert!(matches!(
            Instance::acquire(&launch, self.sockets.path()),
            Err(core_instance::InstanceError::AlreadyRunning)
        ));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

#[test]
fn physical_adopt_block_is_forced_closed_at_original_deadline_without_threads_or_drops() {
    let mut f = Fixture::new("physical_deadline");
    f.event("exec");
    let blocked = f.event("physical_blocked");
    assert_eq!(blocked["threads"], 1);
    let start = blocked["restarting_ns"].as_u64().unwrap();
    let end = blocked["deadline_ns"].as_u64().unwrap();
    assert_eq!(end - start, 120_000_000_000);
    f.excludes_second_core();
    f.advance(); // Inject deadline - 1ns while physical Adopt stays blocked.
    f.event("before_deadline");
    assert!(f.child.try_wait().unwrap().is_none());
    f.excludes_second_core();
    f.advance(); // Inject the sealed deadline, never a reset deadline.
    assert_eq!(f.child.wait().unwrap().code(), Some(70));
    assert!(!f.data.path().join("config/physical-dropped").exists());
    let launch = LaunchGuard::acquire(&f.identity()).unwrap();
    assert!(launch.lifetime_available().unwrap());
    assert_eq!(
        f.database()
            .restart_transaction("restart")
            .unwrap()
            .unwrap()
            .phase,
        RestartPhase::Armed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_sandbox_probe_is_owned_and_only_runs_after_ready() {
    use core_instance::handoff::boot::BootFailure;
    use std::os::unix::fs::PermissionsExt;
    for scenario in ["probe_success", "probe_failure"] {
        let mock = wiremock::MockServer::start().await;
        let mut f = Fixture::new(scenario);
        f.event("exec");
        f.event("physical");
        let held = f.owner();
        std::fs::write(
            f.data.path().join("config/config.toml"),
            format!(
                r#"
[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "EVORCH_FIXTURE_LOCAL_VALUE"
models = ["local-model"]
default_model = "local-model"
[tools.browser]
enabled = false
[routing.routes]
fixture = [{{ profile = "local", model = "local-model" }}]
[agents.worker.categories.quick]
models = [{{ route = "fixture" }}]
"#,
                mock.uri()
            ),
        )
        .unwrap();
        let script = f.data.path().join("bin/bwrap");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf probe > \"$EVORCH_PROBE_MARKER\"\nexit \"$EVORCH_PROBE_EXIT\"\n",
        )
        .unwrap();
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let git_marker = f.data.path().join("git-called");
        let git = f.data.path().join("bin/git");
        std::fs::write(
            &git,
            format!(
                "#!/bin/sh\nprintf git > '{}'\nexit 1\n",
                git_marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(git, std::fs::Permissions::from_mode(0o700)).unwrap();
        f.advance();
        f.event("prepared");
        assert!(!git_marker.exists(), "no learning git child before Ready");
        assert!(!f.data.path().join("probe-called").exists());
        assert!(mock.received_requests().await.unwrap().is_empty());
        f.excludes_second_core();
        f.advance();
        if scenario == "probe_success" {
            let ready = f.event("ready");
            assert_eq!(
                ready["probe"],
                serde_json::json!({"registered":1,"reaped":1,"unconfirmed":0})
            );
            assert!(mock.received_requests().await.unwrap().is_empty());
            assert!(!git_marker.exists(), "learning awaits activation");
            f.advance();
            assert!(f.child.wait().unwrap().success());
            assert!(git_marker.exists());
        } else {
            assert_eq!(f.child.wait().unwrap().code(), Some(73));
            let receipt = f.identity().read_restart_receipt().unwrap().unwrap();
            assert!(matches!(
                receipt.status,
                BootStatus::Failed {
                    failure: BootFailure::SandboxProbe,
                    exit_code: 73
                }
            ));
            assert_eq!(f.owner().lease.generation, held.lease.generation + 1);
            assert_ne!(f.owner().state, OwnerState::Released);
            assert!(f.owner().planned_claim.is_some());
            assert!(f.owner().handoff_hold.is_none());
            assert!(mock.received_requests().await.unwrap().is_empty());
            assert!(!git_marker.exists());
        }
        assert_eq!(
            std::fs::read_to_string(f.data.path().join("probe-called")).unwrap(),
            "probe"
        );
        assert_eq!(
            f.database()
                .restart_transaction("restart")
                .unwrap()
                .unwrap()
                .ready_at_ms,
            Some(123)
        );
    }
}

fn change_owner(f: &Fixture, thread: &str, change: impl FnOnce(&mut Value)) -> Value {
    // Model another owner's already-committed generation, or a damaged hold;
    // the successor must re-read canonical owners.db, not trust its inventory.
    let db = rusqlite::Connection::open(f.owners.path().join("owners.db")).unwrap();
    let state: String = db
        .query_row(
            "SELECT state FROM thread_owners WHERE thread_id = ?1",
            [thread],
            |r| r.get(0),
        )
        .unwrap();
    let mut state: Value = serde_json::from_str(&state).unwrap();
    change(&mut state);
    db.execute(
        "UPDATE thread_owners SET state = ?2 WHERE thread_id = ?1",
        [thread, &state.to_string()],
    )
    .unwrap();
    state
}

#[test]
fn another_owner_is_reported_blocked_and_never_taken_back() {
    let mut f = Fixture::new("blocked");
    f.event("exec");
    f.event("physical");
    let foreign = change_owner(&f, "idle", |s| {
        s["lease"]["generation"] = (s["lease"]["generation"].as_u64().unwrap() + 1).into();
        s["lease"]["owner_id"] = "another-owner".into();
        s["handoff_hold"] = Value::Null;
        s["state"] = "Running".into();
    });
    f.advance();
    assert_eq!(f.event("prepared")["blocked"], serde_json::json!(["idle"]));
    assert_eq!(serde_json::to_value(f.owner()).unwrap(), foreign);
    f.advance();
    f.event("ready");
    f.advance();
    assert!(f.child.wait().unwrap().success());
    assert_eq!(serde_json::to_value(f.owner()).unwrap(), foreign);
}

#[test]
fn partial_claim_error_keeps_both_claimed_generation_and_unclaimed_hold() {
    let mut f = Fixture::new("partial_claim");
    f.event("exec");
    f.event("physical");
    let first = f.owner();
    let damaged = change_owner(&f, "z-idle", |s| {
        s["handoff_hold"]["capture_id"] = "different-capture".into();
    });
    f.advance();
    assert_eq!(f.child.wait().unwrap().code(), Some(73));
    assert_eq!(f.owner().lease.generation, first.lease.generation + 1);
    assert_ne!(f.owner().state, OwnerState::Released);
    let second = Registry::open_readonly(&f.owners.path().join("owners.db"))
        .unwrap()
        .attach("z-idle")
        .unwrap();
    assert_eq!(serde_json::to_value(second).unwrap(), damaged);
    assert_eq!(
        f.database()
            .restart_transaction("restart")
            .unwrap()
            .unwrap()
            .phase,
        RestartPhase::Adopted
    );
}

#[test]
fn control_bind_failure_uses_planned_failure_not_normal_shutdown() {
    let mut f = Fixture::new("bind");
    f.event("exec");
    f.event("physical");
    f.advance();
    f.event("prepared");
    let _occupied =
        std::os::unix::net::UnixListener::bind(f.identity().socket(f.sockets.path()).unwrap())
            .unwrap();
    let owner = f.owner();
    f.advance();
    assert_eq!(f.child.wait().unwrap().code(), Some(73));
    assert_eq!(f.owner().lease, owner.lease);
    assert_ne!(f.owner().state, OwnerState::Released);
    assert!(
        f.database()
            .restart_transaction("restart")
            .unwrap()
            .unwrap()
            .ready_at_ms
            .is_none()
    );
}

async fn check_success(scenario: &str) {
    let mut f = Fixture::new(scenario);
    let old = f.event("exec");
    let physical = f.event("physical");
    assert_eq!(old["pid"], physical["pid"]);
    let held = f.owner();
    assert!(held.handoff_hold.is_some());
    f.excludes_second_core();
    f.advance();
    let prepared = f.event("prepared");
    assert_eq!(prepared["blocked"], serde_json::json!([]));
    let stages = prepared["timings_ms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry[0].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        stages,
        [
            "PhysicalAdopted",
            "StorageOpened",
            "Adopted",
            "FirstOwnerClaimed",
            "OwnersClaimed",
            "HostComposed"
        ]
    );
    let tx = f
        .database()
        .restart_transaction("restart")
        .unwrap()
        .unwrap();
    assert_eq!(tx.phase, RestartPhase::Adopted);
    assert!(tx.ready_at_ms.is_none());
    let claimed = f.owner();
    assert_eq!(claimed.lease.generation, held.lease.generation + 1);
    assert_ne!(claimed.lease.owner_id, held.lease.owner_id);
    assert!(claimed.planned_claim.is_some());
    assert!(claimed.handoff_hold.is_none());
    f.excludes_second_core();
    f.advance();
    let ready = f.event("ready");
    assert_eq!(ready["pid"], old["pid"]);
    assert_eq!(ready["services"], serde_json::json!([]));
    assert_eq!(ready["agents"], 0);
    assert_eq!(ready["retention_inactive"], true);
    assert_eq!(
        f.database()
            .restart_transaction("restart")
            .unwrap()
            .unwrap()
            .ready_at_ms,
        Some(123)
    );
    assert_eq!(f.identity().read_record().unwrap().unwrap().epoch, 2);
    f.excludes_second_core();
    f.advance();
    let activated = f.event("activated");
    let services = activated["services"].as_array().unwrap();
    for name in ["admin-read-io", "admin-audit"] {
        assert_eq!(
            services.iter().filter(|service| **service == name).count(),
            1,
            "management worker starts exactly once after activation: {name}"
        );
    }
    assert_eq!(activated["admin_read"], 123);
    assert!(
        !activated["retention_holds"]
            .as_object()
            .unwrap()
            .contains_key("AuthorityUnavailable"),
        "the activated successor must retain both writer leases"
    );
    assert!(f.child.wait().unwrap().success());
}

#[tokio::test]
async fn planned_bootstrap_exec_adopts_claims_before_composition_and_defers_activation() {
    check_success("success").await;
}
#[tokio::test]
async fn adopt_lost_ack_is_confirmed_by_readback() {
    check_success("adopt_ack").await;
}
#[tokio::test]
async fn adopt_rollback_retries_the_same_proof() {
    check_success("adopt_retry").await;
}
#[tokio::test]
async fn ready_lost_ack_is_confirmed_before_activation() {
    check_success("ready_ack").await;
}

async fn check_failure(scenario: &str, code: i32, adopted: bool) {
    let mut f = Fixture::new(scenario);
    f.event("exec");
    f.event("physical");
    let held = f.owner();
    f.excludes_second_core();
    let db_path = f.data.path().join("config/evorch-events.db");
    let saved_db = db_path.with_extension("saved");
    if scenario == "database" {
        std::fs::rename(&db_path, &saved_db).unwrap();
        std::fs::create_dir(&db_path).unwrap();
    }
    f.advance();
    if adopted {
        f.event("prepared");
        f.excludes_second_core();
        if scenario == "interrupt" || scenario == "terminate" {
            rustix::process::kill_process(
                rustix::process::Pid::from_raw(f.child.id() as _).unwrap(),
                if scenario == "interrupt" {
                    rustix::process::Signal::INT
                } else {
                    rustix::process::Signal::TERM
                },
            )
            .unwrap();
        }
        f.advance();
    }
    assert_eq!(f.child.wait().unwrap().code(), Some(code), "{scenario}");
    if scenario == "database" {
        std::fs::remove_dir(&db_path).unwrap();
        std::fs::rename(&saved_db, &db_path).unwrap();
    }
    let tx = f
        .database()
        .restart_transaction("restart")
        .unwrap()
        .unwrap();
    assert_eq!(
        tx.phase,
        if adopted {
            RestartPhase::Adopted
        } else {
            RestartPhase::Armed
        }
    );
    assert!(tx.ready_at_ms.is_none());
    let after = f.owner();
    assert_ne!(after.state, OwnerState::Released);
    assert_eq!(
        after.lease.generation,
        held.lease.generation + u64::from(adopted)
    );
    assert_eq!(after.handoff_hold.is_some(), !adopted);
    assert_eq!(after.planned_claim.is_some(), adopted);
    let receipt = f.identity().read_restart_receipt().unwrap().unwrap();
    assert!(matches!(receipt.status, BootStatus::Failed { exit_code, .. } if exit_code == code));
    // Repair only the fixture's deliberately broken local config. Failure
    // never silently falls back: quarantine happens in this explicit open.
    if scenario == "storage" {
        std::fs::remove_file(f.data.path().join("config/config.toml")).unwrap();
    }
    let mut cold = BootstrappedHost::open_for_core(
        BootstrapOptions {
            config_dir: f.identity().config_dir().into(),
            project_root: f.data.path().join("project"),
            ownership_root: f.owners.path().into(),
        },
        &evorch_core::recovery_service::CoreGraphRecovery,
    )
    .await
    .unwrap();
    assert!(
        f.database()
            .restart_transaction("restart")
            .unwrap()
            .unwrap()
            .phase
            .terminal()
    );
    cold.shutdown().await.unwrap();
}

macro_rules! failure_case {
    ($name:ident, $scenario:literal, $code:literal, $adopted:literal) => {
        #[tokio::test]
        async fn $name() {
            check_failure($scenario, $code, $adopted).await;
        }
    };
}
failure_case!(
    database_open_failure_preserves_armed_until_explicit_cold,
    "database",
    70,
    false
);
failure_case!(
    configuration_failure_preserves_armed_until_explicit_cold,
    "storage",
    70,
    false
);
failure_case!(
    rejected_adoption_preserves_armed_until_explicit_cold,
    "rejected",
    71,
    false
);
failure_case!(
    predecessor_epoch_mismatch_is_a_definitive_rejection,
    "epoch",
    71,
    false
);
failure_case!(
    unknown_ack_never_authorizes_claim_or_cold_fallback,
    "ack_unknown",
    72,
    false
);
failure_case!(
    post_claim_failure_never_releases_successor_generation,
    "before_ready",
    73,
    true
);
failure_case!(
    sigint_before_ready_never_activates_or_releases,
    "interrupt",
    130,
    true
);
failure_case!(
    sigterm_before_ready_never_activates_or_releases,
    "terminate",
    143,
    true
);

#[tokio::test]
async fn planned_storage_without_proof_cannot_open_an_owner_or_run_maintenance() {
    let data = tempfile::tempdir().unwrap();
    let owners = test_socket_dir::SocketDir::new();
    let project = data.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let config = data.path().join("config");
    std::fs::create_dir(&config).unwrap();
    let stage = PlannedStorage::open(BootstrapOptions {
        config_dir: config.clone(),
        ownership_root: owners.path().into(),
        project_root: project,
    })
    .await
    .unwrap();
    assert!(stage.runtime().list_agents().is_empty());
    assert!(!config.join("credentials").exists());
    assert!(!owners.path().join("owners.db").exists());
    assert!(stage.claim("unproven").is_err());
    assert!(!owners.path().join("owners.db").exists());
}

#[tokio::test]
async fn planned_storage_lease_contention_does_not_open_an_owner_or_fall_back() {
    let data = tempfile::tempdir().unwrap();
    let config = data.path().join("config");
    let project = data.path().join("project");
    let owners = data.path().join("owners");
    std::fs::create_dir(&config).unwrap();
    std::fs::create_dir(&project).unwrap();
    let exclusive = storage::try_exclusive(&config.join("evorch-events.db"), &owners)
        .unwrap()
        .unwrap();
    let options = BootstrapOptions {
        config_dir: config,
        project_root: project,
        ownership_root: owners.clone(),
    };
    assert!(
        PlannedStorage::open_with_lease_wait(options.clone(), std::time::Duration::ZERO)
            .await
            .is_err()
    );
    assert!(!owners.join("owners.db").exists());
    drop(exclusive);
    let stage = PlannedStorage::open_with_lease_wait(options, std::time::Duration::ZERO)
        .await
        .unwrap();
    assert!(stage.runtime().list_agents().is_empty());
    assert!(!owners.join("owners.db").exists());
}
