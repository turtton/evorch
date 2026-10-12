use super::*;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};

#[test]
fn restored_reservation_cannot_exhaust_or_jump_the_durable_counter() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let allocator = allocator(&root);
    assert_eq!(allocator.allocate().unwrap(), 0);
    let before = std::fs::read(root.join("counter")).unwrap();
    for floor in [u64::from(u32::MAX) + 1, u64::MAX] {
        assert!(allocator.reserve(floor).is_err());
        assert_eq!(std::fs::read(root.join("counter")).unwrap(), before);
    }
    assert_eq!(allocator.allocate().unwrap(), 1);
    allocator.reserve(u64::from(u32::MAX)).unwrap();
    assert_eq!(allocator.allocate().unwrap(), u64::from(u32::MAX));
    // Numbers reached through normal allocation still use the full u64 range.
    allocator.reserve(u64::from(u32::MAX) + 1).unwrap();
    assert_eq!(allocator.allocate().unwrap(), u64::from(u32::MAX) + 1);
}

#[test]
fn rejected_restored_reservation_does_not_initialize_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("missing-parent/state");
    assert!(allocator(&root).reserve(u64::MAX).is_err());
    assert!(!root.parent().unwrap().exists());
    assert_eq!(allocator(&root).allocate().unwrap(), 0);
}

#[test]
fn naturally_issued_high_handles_allow_noop_restore_and_keep_allocating() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    initialize(&root).unwrap();
    let floor = u64::MAX - 2;
    std::fs::write(root.join("counter"), format!("{FORMAT}{floor}\n")).unwrap();
    let before = std::fs::read(root.join("counter")).unwrap();
    for restored in [floor, floor - 1, u64::from(u32::MAX) + 1] {
        allocator(&root).reserve(restored).unwrap();
        assert_eq!(std::fs::read(root.join("counter")).unwrap(), before);
    }
    assert!(allocator(&root).reserve(u64::MAX).is_err());
    assert_eq!(std::fs::read(root.join("counter")).unwrap(), before);
    assert_eq!(allocator(&root).allocate().unwrap(), floor);
    assert_eq!(allocator(&root).allocate().unwrap(), floor + 1);
    assert!(allocator(&root).allocate().is_err());
    let exhausted = std::fs::read(root.join("counter")).unwrap();
    allocator(&root).reserve(u64::MAX).unwrap();
    assert_eq!(std::fs::read(root.join("counter")).unwrap(), exhausted);
}

fn allocator(root: &Path) -> HandleAllocator {
    HandleAllocator {
        root: Some(root.to_owned()),
    }
}

#[test]
fn reopened_allocators_and_reservations_never_move_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    assert_eq!(allocator(&root).allocate().unwrap(), 0);
    assert_eq!(allocator(&root).allocate().unwrap(), 1);
    allocator(&root).reserve(100).unwrap();
    allocator(&root).reserve(3).unwrap();
    assert_eq!(allocator(&root).allocate().unwrap(), 100);
}

#[test]
fn concurrent_first_publish_and_thread_allocations_are_unique() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (0..10)
                    .map(|_| allocator(&root).allocate().unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut numbers: Vec<_> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    numbers.sort_unstable();
    assert_eq!(numbers, (0..80).collect::<Vec<_>>());
}

#[test]
fn corrupt_missing_or_exhausted_state_is_never_reinitialized() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    allocator(&root).allocate().unwrap();
    for text in [
        "",
        "0",
        "evorch-shell-handles-v1\n-1\n",
        "evorch-shell-handles-v1\n18446744073709551616\n",
        "evorch-shell-handles-v1\n18446744073709551615\n",
    ] {
        std::fs::write(root.join("counter"), text).unwrap();
        assert!(allocator(&root).allocate().is_err());
        assert_eq!(std::fs::read_to_string(root.join("counter")).unwrap(), text);
    }
    std::fs::remove_file(root.join("counter")).unwrap();
    assert!(allocator(&root).allocate().is_err());
    assert!(!root.join("counter").exists());
    std::fs::remove_file(root.join("lock")).unwrap();
    assert!(allocator(&root).reserve(42).is_err());
    assert!(!root.join("lock").exists());
}

#[test]
fn prepublication_crash_debris_does_not_publish_incomplete_state() {
    let dir = tempfile::tempdir().unwrap();
    let debris = dir.path().join("unpublished-temp");
    std::fs::create_dir(&debris).unwrap();
    File::create(debris.join("lock")).unwrap();
    let root = dir.path().join("state");
    assert_eq!(allocator(&root).allocate().unwrap(), 0);
    assert!(debris.join("lock").exists());
    // An incomplete PUBLIC directory is uncertain, not a new installation.
    let incomplete = dir.path().join("incomplete");
    std::fs::create_dir(&incomplete).unwrap();
    File::create(incomplete.join("lock")).unwrap();
    assert!(allocator(&incomplete).allocate().is_err());
}

fn child(root: &Path, mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "shell_handles::tests::allocator_child",
            "--nocapture",
        ])
        .env("EVORCH_ALLOCATOR_CHILD_ROOT", root)
        .env("EVORCH_ALLOCATOR_CHILD_MODE", mode)
        .stdout(Stdio::null());
    command
}

#[test]
fn allocator_child() {
    let Some(root) = std::env::var_os("EVORCH_ALLOCATOR_CHILD_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mode = std::env::var("EVORCH_ALLOCATOR_CHILD_MODE").unwrap();
    if mode == "hold" {
        initialize(&root).unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("lock"))
            .unwrap();
        lock.lock().unwrap();
        println!("allocator-lock-ready");
        std::io::stdout().flush().unwrap();
        // Parent holds stdin open until it kills us, exercising OS lock release
        // on death without timing or filesystem polling.
        std::io::stdin().read_exact(&mut [0]).unwrap();
        panic!("parent must kill lock holder");
    }
    let number = allocator(&root).allocate().unwrap();
    if mode == "crash" {
        // Allocation is durable, but the caller never receives its handle.
        std::process::abort();
    }
    std::fs::write(root.join(format!("issued-{number}")), number.to_string()).unwrap();
}

#[test]
fn processes_compete_on_first_initialization_and_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let mut children: Vec<_> = (0..8)
        .map(|_| child(&root, "allocate").spawn().unwrap())
        .collect();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    for number in 0..8 {
        assert!(root.join(format!("issued-{number}")).exists());
    }
    assert_eq!(allocator(&root).allocate().unwrap(), 8);
    assert!(child(&root, "allocate").status().unwrap().success());
    assert!(root.join("issued-9").exists());
}

#[test]
fn killed_process_releases_os_lock_and_crashed_allocation_leaves_a_gap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let mut holder = child(&root, "hold")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(holder.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "lock holder exited"
        );
        if line.trim() == "allocator-lock-ready" {
            break;
        }
    }
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert_eq!(allocator(&root).allocate().unwrap(), 0);
    assert!(!child(&root, "crash").status().unwrap().success());
    assert_eq!(allocator(&root).allocate().unwrap(), 2);
}

#[cfg(unix)]
#[test]
fn failed_atomic_save_does_not_issue_or_reset_a_number() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    assert_eq!(allocator(&root).allocate().unwrap(), 0);
    let before = std::fs::read(root.join("counter")).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).unwrap();
    let outcome = allocator(&root).allocate();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(outcome.is_err());
    assert_eq!(std::fs::read(root.join("counter")).unwrap(), before);
    assert_eq!(allocator(&root).allocate().unwrap(), 1);
}
