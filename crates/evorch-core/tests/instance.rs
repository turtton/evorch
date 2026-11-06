use evorch_core::instance::{Instance, InstanceError};
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::{Arc, Barrier},
};

fn dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("p7-")
        .tempdir_in("/tmp")
        .unwrap()
}

#[test]
fn simultaneous_starts_have_one_lifetime_owner_even_without_a_socket() {
    let dir = dir();
    let barrier = Arc::new(Barrier::new(3));
    let acquired = Arc::new(Barrier::new(3));
    std::thread::scope(|s| {
        let attempt = || {
            barrier.wait();
            let instance = Instance::acquire(&dir.path().join("config"), dir.path());
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
    assert!(Instance::acquire(&dir.path().join("config"), dir.path()).is_ok());
}

#[tokio::test]
async fn stale_socket_is_reaped_but_symlinks_are_refused() {
    let dir = dir();
    let config = dir.path().join("config");
    let first = Instance::acquire(&config, dir.path()).unwrap();
    let path = first.socket.clone();
    drop(first);
    let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(stale);
    let next = Instance::acquire(&config, dir.path()).unwrap();
    let listener = next.bind().unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(listener);
    drop(next);
    symlink(dir.path().join("victim"), &path).unwrap();
    assert!(Instance::acquire(&config, dir.path()).is_err());
    assert!(path.is_symlink());
}

#[test]
fn canonical_aliases_share_the_lock() {
    let dir = dir();
    let config = dir.path().join("config");
    let first = Instance::acquire(&config, dir.path()).unwrap();
    symlink(&config, dir.path().join("alias")).unwrap();
    assert!(matches!(
        Instance::acquire(&dir.path().join("alias"), dir.path()),
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
    let first = Instance::acquire_with_runtime_dir(&config, Some(&runtime), dir.path()).unwrap();
    assert!(first.socket.starts_with(&runtime));
    assert!(matches!(
        Instance::acquire_with_runtime_dir(&config, None, dir.path()),
        Err(InstanceError::AlreadyRunning)
    ));
    drop(first);
    let fallback = Instance::acquire_with_runtime_dir(&config, None, dir.path()).unwrap();
    assert!(!fallback.socket.starts_with(&runtime));
    drop(fallback);
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Instance::acquire_with_runtime_dir(&config, Some(&runtime), dir.path()).is_err());
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let alias = dir.path().join("runtime-link");
    symlink(&runtime, &alias).unwrap();
    let missing = dir.path().join("missing");
    for path in [
        alias.as_path(),
        std::path::Path::new("relative"),
        missing.as_path(),
    ] {
        assert!(Instance::acquire_with_runtime_dir(&config, Some(path), dir.path()).is_err());
    }
}
