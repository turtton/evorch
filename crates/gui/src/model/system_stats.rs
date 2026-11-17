//! CPU, memory and storage readings for the footer.
//!
//! A background thread samples the machine every [`SAMPLE_INTERVAL`] and hands each reading
//! over a channel, so the UI thread never waits on the operating system. The thread wakes the
//! UI once per reading and stops when its [`SystemSampler`] is dropped.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use sysinfo::{Disks, MINIMUM_CPU_UPDATE_INTERVAL, System};

/// How often the machine is read.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(3);

/// A used amount out of a total, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub used: u64,
    pub total: u64,
}

impl Usage {
    /// The used share in `0.0..=1.0`; an empty total reads as unused.
    #[must_use]
    pub fn ratio(self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = self.used as f64 / self.total as f64;
        ratio.clamp(0.0, 1.0)
    }
}

/// The filesystem that holds the evorch data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageUsage {
    /// The directory the reading is about (the one holding the database).
    pub directory: PathBuf,
    /// Where the filesystem that holds `directory` is mounted.
    pub mount: PathBuf,
    pub usage: Usage,
}

/// One reading of the machine.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemSnapshot {
    /// Average load over all cores, in percent.
    pub cpu_percent: f32,
    pub memory: Usage,
    /// `None` when no data directory is known or no mounted filesystem holds it.
    pub storage: Option<StorageUsage>,
}

impl SystemSnapshot {
    /// The CPU load as a share in `0.0..=1.0`.
    #[must_use]
    pub fn cpu_ratio(&self) -> f64 {
        f64::from(self.cpu_percent / 100.0).clamp(0.0, 1.0)
    }
}

/// Binary-unit size, such as `7.8 GiB`.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A mounted filesystem: where it is mounted, its device id when known, and its usage.
#[derive(Debug, Clone, Copy)]
pub struct Mount<'a> {
    pub point: &'a Path,
    pub device: Option<u64>,
    pub usage: Usage,
}

/// The mount whose filesystem holds `path`, whose device id is `device` when known.
///
/// The device id decides first, because a path prefix can name the wrong mount: on macOS
/// `/Users/...` lives on the data volume mounted at `/System/Volumes/Data`, yet only `/`
/// is a prefix of it. Among the mounts on that device the deepest prefix of `path` wins,
/// else the deepest mount.
///
/// A mount whose device is known and differs from `device` is never chosen: `sysinfo` omits
/// some filesystems (tmpfs, NFS), and the prefix of a different filesystem would report the
/// wrong numbers. When no mount matches, the result is `None`. The deepest prefix is used
/// only among mounts whose device is unknown, or when `device` itself is unknown.
/// Mounts reporting no space (pseudo filesystems) are skipped.
pub fn mount_holding<'a>(
    path: &Path,
    device: Option<u64>,
    mounts: impl IntoIterator<Item = Mount<'a>>,
) -> Option<Mount<'a>> {
    let real: Vec<Mount<'a>> = mounts
        .into_iter()
        .filter(|mount| mount.usage.total > 0)
        .collect();
    let deepest = |mounts: Vec<Mount<'a>>| {
        mounts
            .into_iter()
            .max_by_key(|mount| mount.point.components().count())
    };
    if let Some(device) = device {
        let same_device: Vec<Mount<'a>> = real
            .iter()
            .copied()
            .filter(|mount| mount.device == Some(device))
            .collect();
        let (prefixes, others): (Vec<_>, Vec<_>) = same_device
            .into_iter()
            .partition(|mount| path.starts_with(mount.point));
        if let Some(found) = deepest(prefixes).or_else(|| deepest(others)) {
            return Some(found);
        }
    }
    deepest(
        real.into_iter()
            .filter(|mount| match (device, mount.device) {
                (Some(wanted), Some(actual)) => wanted == actual,
                _ => true,
            })
            .filter(|mount| path.starts_with(mount.point))
            .collect(),
    )
}

/// The id of the device holding `path`; unknown where the platform has none.
#[cfg(unix)]
fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).ok().map(|metadata| metadata.dev())
}

#[cfg(not(unix))]
fn device_of(_path: &Path) -> Option<u64> {
    None
}

/// Reads the machine with `sysinfo`.
struct SysinfoSource {
    system: System,
    disks: Disks,
    /// Resolved once, so the mount lookup compares real paths.
    storage_directory: Option<PathBuf>,
    primed: bool,
}

impl SysinfoSource {
    fn new(storage_directory: Option<PathBuf>) -> Self {
        let storage_directory = storage_directory
            .map(|directory| std::fs::canonicalize(&directory).unwrap_or(directory));
        Self {
            system: System::new(),
            disks: Disks::new(),
            storage_directory,
            primed: false,
        }
    }

    fn sample(&mut self) -> SystemSnapshot {
        self.system.refresh_cpu_usage();
        if !self.primed {
            // The first CPU reading is meaningless without an earlier one to compare with.
            self.primed = true;
            std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
            self.system.refresh_cpu_usage();
        }
        self.system.refresh_memory();
        SystemSnapshot {
            cpu_percent: self.system.global_cpu_usage(),
            memory: Usage {
                used: self.system.used_memory(),
                total: self.system.total_memory(),
            },
            storage: self.storage(),
        }
    }

    fn storage(&mut self) -> Option<StorageUsage> {
        let directory = self.storage_directory.clone()?;
        self.disks.refresh(true);
        let found = mount_holding(
            &directory,
            device_of(&directory),
            self.disks.list().iter().map(|disk| {
                let total = disk.total_space();
                Mount {
                    point: disk.mount_point(),
                    // A mount that reports no space is skipped, so it is not worth a lookup.
                    device: (total > 0).then(|| device_of(disk.mount_point())).flatten(),
                    usage: Usage {
                        used: total.saturating_sub(disk.available_space()),
                        total,
                    },
                }
            }),
        )?;
        Some(StorageUsage {
            mount: found.point.to_path_buf(),
            directory,
            usage: found.usage,
        })
    }
}

/// Owns the sampling thread and the latest reading it produced.
pub struct SystemSampler {
    updates: Receiver<SystemSnapshot>,
    latest: Option<SystemSnapshot>,
    /// Dropping it disconnects the thread's wait between readings, so a waiting thread ends at once.
    _stop: Sender<()>,
}

impl SystemSampler {
    /// Starts sampling the machine; `wake` is called after each reading arrives.
    /// `storage_directory` selects the filesystem to report, when known.
    pub fn start(
        storage_directory: Option<PathBuf>,
        wake: impl Fn() + Send + 'static,
    ) -> Option<Self> {
        Self::spawn(SAMPLE_INTERVAL, wake, move || {
            let mut source = SysinfoSource::new(storage_directory);
            move || source.sample()
        })
    }

    /// `make_source` runs on the new thread and returns the function taking one reading.
    fn spawn<F>(
        interval: Duration,
        wake: impl Fn() + Send + 'static,
        make_source: impl FnOnce() -> F + Send + 'static,
    ) -> Option<Self>
    where
        F: FnMut() -> SystemSnapshot,
    {
        let (updates_tx, updates) = mpsc::channel();
        let (stop, stop_rx) = mpsc::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("system-stats".into())
            .spawn(move || {
                let mut sample = make_source();
                loop {
                    if updates_tx.send(sample()).is_err() {
                        break;
                    }
                    wake();
                    match stop_rx.recv_timeout(interval) {
                        Err(RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the system stats sampler");
            return None;
        }
        Some(Self {
            updates,
            latest: None,
            _stop: stop,
        })
    }

    /// The newest reading received so far; never blocks.
    pub fn latest(&mut self) -> Option<&SystemSnapshot> {
        while let Ok(snapshot) = self.updates.try_recv() {
            self.latest = Some(snapshot);
        }
        self.latest.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    fn usage(used: u64, total: u64) -> Usage {
        Usage { used, total }
    }

    fn snapshot(cpu_percent: f32) -> SystemSnapshot {
        SystemSnapshot {
            cpu_percent,
            memory: usage(1, 2),
            storage: None,
        }
    }

    // Given: usages that are empty, partial, full and over-full
    // When: the ratio is taken / Then: it stays inside 0..=1 and an empty total reads as unused
    #[test]
    fn usage_ratio_is_clamped_and_total_free() {
        assert!(usage(5, 0).ratio().abs() < f64::EPSILON);
        assert!((usage(1, 4).ratio() - 0.25).abs() < f64::EPSILON);
        assert!((usage(4, 4).ratio() - 1.0).abs() < f64::EPSILON);
        assert!((usage(9, 4).ratio() - 1.0).abs() < f64::EPSILON);
    }

    // Given: CPU readings below, inside and above the percent range
    // When: converted to a share / Then: it is clamped to 0..=1
    #[test]
    fn cpu_ratio_follows_percent_and_clamps() {
        assert!((snapshot(50.0).cpu_ratio() - 0.5).abs() < 1e-6);
        assert!(snapshot(-3.0).cpu_ratio().abs() < f64::EPSILON);
        assert!((snapshot(180.0).cpu_ratio() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024 / 2), "2.5 GiB");
        assert_eq!(format_bytes(3 * 1024_u64.pow(4)), "3.0 TiB");
        assert_eq!(format_bytes(2048 * 1024_u64.pow(4)), "2048.0 TiB");
    }

    fn mount(point: &str, device: Option<u64>, used: u64, total: u64) -> Mount<'_> {
        Mount {
            point: Path::new(point),
            device,
            usage: usage(used, total),
        }
    }

    // Given: nested mounts, a pseudo mount with no space, and an unrelated mount, with no device ids
    // When: the mount holding a path is looked up / Then: the deepest real prefix wins
    #[test]
    fn mount_holding_without_devices_picks_the_deepest_real_prefix() {
        let mounts = [
            mount("/", None, 10, 100),
            mount("/home", None, 20, 200),
            mount("/home/user/data", None, 0, 0),
            mount("/homework", None, 30, 300),
        ];
        let found =
            |path: &str| mount_holding(Path::new(path), None, mounts).map(|mount| mount.point);
        assert_eq!(found("/home/user/data/db"), Some(Path::new("/home")));
        assert_eq!(found("/homework/db"), Some(Path::new("/homework")));
        assert_eq!(found("/var/lib/db"), Some(Path::new("/")));
        assert!(mount_holding(Path::new("/var"), None, [mount("/home", None, 1, 2)]).is_none());
    }

    // Given: macOS-like mounts where `/Users` is a firmlink into the data volume, so only `/` is a path prefix
    // When: the data directory's device is the data volume's / Then: the data volume is chosen, not `/`
    #[test]
    fn mount_holding_follows_the_device_across_firmlinks() {
        const SYSTEM: u64 = 1;
        const DATA: u64 = 2;
        let mounts = [
            mount("/", Some(SYSTEM), 10, 100),
            mount("/System/Volumes/Data", Some(DATA), 60, 100),
            mount("/System/Volumes/VM", Some(3), 5, 100),
        ];
        let data = Path::new("/Users/me/Library/Application Support/evorch");
        let found = mount_holding(data, Some(DATA), mounts).expect("a mount");
        assert_eq!(found.point, Path::new("/System/Volumes/Data"));
        assert_eq!(found.usage, usage(60, 100));
        let system = mount_holding(Path::new("/usr/bin"), Some(SYSTEM), mounts).expect("a mount");
        assert_eq!(system.point, Path::new("/"));
    }

    // Given: the data directory sits on a filesystem sysinfo does not list (a tmpfs, device 2), leaving only `/` (device 1)
    // When: looked up / Then: nothing is chosen rather than the prefix of a different filesystem
    #[test]
    fn mount_holding_never_picks_a_known_different_device() {
        let mounts = [mount("/", Some(1), 10, 100)];
        assert!(mount_holding(Path::new("/tmp/evorch"), Some(2), mounts).is_none());
        // An unknown mount device may still be matched by its path.
        let unknown = [mount("/", None, 10, 100)];
        let found = mount_holding(Path::new("/tmp/evorch"), Some(2), unknown).expect("a mount");
        assert_eq!(found.point, Path::new("/"));
    }

    // Given: bind-like mounts sharing one device, and a data directory whose device is unknown
    // When: looked up / Then: the deepest prefix among the shared device wins, and an unknown device falls back to prefixes
    #[test]
    fn mount_holding_prefers_prefixes_on_a_shared_device_and_falls_back() {
        let mounts = [
            mount("/", Some(1), 10, 100),
            mount("/srv", Some(1), 20, 200),
            mount("/mnt/other", Some(9), 30, 300),
        ];
        let found = mount_holding(Path::new("/srv/evorch"), Some(1), mounts).expect("a mount");
        assert_eq!(found.point, Path::new("/srv"));
        let fallback = mount_holding(Path::new("/srv/evorch"), None, mounts).expect("a mount");
        assert_eq!(fallback.point, Path::new("/srv"));
    }

    // Given: a sampler whose source takes readings on its own thread
    // When: the thread signals each reading / Then: `latest` returns the newest one and wakes the UI per reading
    #[test]
    fn sampler_delivers_readings_without_the_caller_waiting() {
        let readings = Arc::new(AtomicUsize::new(0));
        let woken = Arc::new(AtomicUsize::new(0));
        // The sampler waits `interval` between readings; the wake of the second reading is the
        // signal that the first two are both queued.
        let barrier = Arc::new(Barrier::new(2));
        let mut sampler = SystemSampler::spawn(
            Duration::ZERO,
            {
                let woken = Arc::clone(&woken);
                let barrier = Arc::clone(&barrier);
                move || {
                    if woken.fetch_add(1, Ordering::SeqCst) == 1 {
                        barrier.wait();
                    }
                }
            },
            {
                let readings = Arc::clone(&readings);
                move || {
                    move || {
                        let count = readings.fetch_add(1, Ordering::SeqCst);
                        #[allow(clippy::cast_precision_loss)]
                        snapshot(count as f32)
                    }
                }
            },
        )
        .expect("the sampler thread starts");
        barrier.wait();
        let newest = sampler.latest().expect("a reading arrived").cpu_percent;
        assert!(newest >= 1.0, "the second reading was delivered: {newest}");
    }

    // Given: a sampler that has delivered its first reading and now waits out a very long interval
    // When: it is dropped / Then: the thread leaves that wait and ends, which drops its source
    #[test]
    fn dropping_the_sampler_ends_a_waiting_thread() {
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let (woke_tx, woke_rx) = mpsc::channel::<()>();
        let sampler = SystemSampler::spawn(
            Duration::from_secs(3600),
            move || {
                let _ = woke_tx.send(());
            },
            move || {
                // Dropped when the thread's loop ends.
                let done = done_tx;
                move || {
                    let _ = &done;
                    snapshot(0.0)
                }
            },
        )
        .expect("the sampler thread starts");
        woke_rx.recv().expect("the first reading was announced");
        drop(sampler);
        assert!(
            done_rx.recv().is_err(),
            "the thread dropped its source and ended"
        );
    }
}
