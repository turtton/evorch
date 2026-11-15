use evorch_core::instance::{ConfigIdentity, Instance, InstanceError, LaunchGuard};
fn acquire(config: &std::path::Path, ipc: &std::path::Path) -> Result<Instance, InstanceError> {
    let identity = ConfigIdentity::new(config)?;
    let launch = LaunchGuard::acquire(&identity)?;
    Instance::acquire(&launch, ipc)
}
fn acquire_runtime(
    config: &std::path::Path,
    runtime: Option<&std::path::Path>,
    fallback: &std::path::Path,
) -> Result<Instance, InstanceError> {
    let identity = ConfigIdentity::new(config)?;
    let launch = LaunchGuard::acquire(&identity)?;
    Instance::acquire_with_runtime_dir(&launch, runtime, fallback)
}
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::{Arc, Barrier},
};

fn dir() -> test_socket_dir::SocketDir {
    test_socket_dir::SocketDir::new()
}

#[test]
fn simultaneous_starts_have_one_lifetime_owner_even_without_a_socket() {
    let dir = dir();
    let barrier = Arc::new(Barrier::new(3));
    let acquired = Arc::new(Barrier::new(3));
    std::thread::scope(|s| {
        let attempt = || {
            barrier.wait();
            let instance = acquire(&dir.path().join("config"), dir.path());
            acquired.wait();
            instance
        };
        let a = s.spawn(attempt);
        let b = s.spawn(attempt);
        barrier.wait();
        acquired.wait();
        let a = a.join().unwrap();
        let b = b.join().unwrap();
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert!(matches!(
            a.as_ref().err().or(b.as_ref().err()),
            Some(InstanceError::AlreadyRunning)
        ));
    });
    assert!(acquire(&dir.path().join("config"), dir.path()).is_ok());
}

#[tokio::test]
async fn stale_socket_is_reaped_but_symlinks_are_refused() {
    let dir = dir();
    let config = dir.path().join("config");
    let first = acquire(&config, dir.path()).unwrap();
    let path = first.socket.clone();
    drop(first);
    let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(stale);
    let next = acquire(&config, dir.path()).unwrap();
    let listener = next.bind().unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(listener);
    drop(next);
    symlink(dir.path().join("victim"), &path).unwrap();
    assert!(acquire(&config, dir.path()).is_err());
    assert!(path.is_symlink());
}

#[test]
fn canonical_aliases_share_the_lock() {
    let dir = dir();
    let config = dir.path().join("config");
    let first = acquire(&config, dir.path()).unwrap();
    symlink(&config, dir.path().join("alias")).unwrap();
    assert!(matches!(
        acquire(&dir.path().join("alias"), dir.path()),
        Err(InstanceError::AlreadyRunning)
    ));
    drop(first);
}

#[test]
fn runtime_directory_is_validated_and_does_not_change_database_lock_identity() {
    let dir = dir();
    let config = dir.path().join("config");
    let runtime = dir.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let first = acquire_runtime(&config, Some(&runtime), dir.path()).unwrap();
    assert!(first.socket.starts_with(&runtime));
    assert!(matches!(
        acquire_runtime(&config, None, dir.path()),
        Err(InstanceError::AlreadyRunning)
    ));
    drop(first);
    let fallback = acquire_runtime(&config, None, dir.path()).unwrap();
    assert!(!fallback.socket.starts_with(&runtime));
    drop(fallback);
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(acquire_runtime(&config, Some(&runtime), dir.path()).is_err());
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let alias = dir.path().join("runtime-link");
    symlink(&runtime, &alias).unwrap();
    let missing = dir.path().join("missing");
    for path in [
        alias.as_path(),
        std::path::Path::new("relative"),
        missing.as_path(),
    ] {
        assert!(acquire_runtime(&config, Some(path), dir.path()).is_err());
    }
}
