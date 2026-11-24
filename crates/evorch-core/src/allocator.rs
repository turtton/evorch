//! Caps the number of glibc malloc arenas for the core process.
//!
//! Reading a large history on several worker threads leaves one arena per thread holding its
//! freed pages. Capping the arenas lets the freed memory be reused across threads. The cap is a
//! process-wide default for the core binary only: an operator who sets `MALLOC_ARENA_MAX` or the
//! `glibc.malloc.arena_max` tunable keeps full control, and the environment is never rewritten.

use std::ffi::{OsStr, OsString};

/// Upper bound on the number of arenas, main arena included.
pub const ARENA_MAX: i32 = 2;

/// Emitted when glibc rejects the cap. Fixed text: nothing from the environment is echoed.
pub const WARNING: &str =
    "evorch-core: warning: could not set the malloc arena limit; continuing with the glibc default";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The cap was set.
    Applied,
    /// `MALLOC_ARENA_MAX` or `GLIBC_TUNABLES` names the arena limit; glibc owns it.
    ExternalOverride,
    /// glibc rejected the call; start-up continues with the default.
    Failed,
    /// Not a glibc target: nothing to do.
    Unsupported,
}

const TUNABLE: &str = "glibc.malloc.arena_max";

/// True when the operator already chose the arena limit. An empty or malformed value still
/// counts: glibc decides what it means, this process does not second-guess it.
pub fn externally_configured(get: impl Fn(&str) -> Option<OsString>) -> bool {
    get("MALLOC_ARENA_MAX").is_some()
        || get("GLIBC_TUNABLES").is_some_and(|value| names_arena_max(&value))
}

fn names_arena_max(tunables: &OsStr) -> bool {
    tunables.to_string_lossy().contains(TUNABLE)
}

/// Decision and effect separated so that the policy is testable without touching the process
/// allocator: `set` returns whether glibc accepted the value, `warn` receives [`WARNING`].
pub fn apply_with(
    get: impl Fn(&str) -> Option<OsString>,
    set: impl FnOnce(i32) -> bool,
    warn: impl FnOnce(&str),
) -> Outcome {
    if externally_configured(get) {
        return Outcome::ExternalOverride;
    }
    if set(ARENA_MAX) {
        Outcome::Applied
    } else {
        warn(WARNING);
        Outcome::Failed
    }
}

/// Called once at process entry, before any thread is created, for both cold starts and the
/// planned-restart exec (the new image re-runs `main`).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn configure_at_entry() -> Outcome {
    apply_with(
        |key| std::env::var_os(key),
        // SAFETY: `mallopt` takes plain integers and is safe to call at any time.
        |value| unsafe { libc::mallopt(libc::M_ARENA_MAX, value) } == 1,
        |message| eprintln!("{message}"),
    )
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn configure_at_entry() -> Outcome {
    Outcome::Unsupported
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    fn run(pairs: &[(&str, &str)], accepted: bool) -> (Outcome, Option<i32>, Option<String>) {
        let value = Cell::new(None);
        let warning = Cell::new(None);
        let outcome = apply_with(
            env(pairs),
            |v| {
                value.set(Some(v));
                accepted
            },
            |m| warning.set(Some(m.to_owned())),
        );
        (outcome, value.get(), warning.take())
    }

    #[test]
    fn an_unconfigured_environment_gets_the_cap_without_a_warning() {
        assert_eq!(run(&[], true), (Outcome::Applied, Some(ARENA_MAX), None));
        assert_eq!(
            run(&[("GLIBC_TUNABLES", "glibc.malloc.tcache_count=0")], true),
            (Outcome::Applied, Some(ARENA_MAX), None)
        );
    }

    #[test]
    fn any_external_mention_of_the_limit_is_left_to_glibc() {
        for pairs in [
            &[("MALLOC_ARENA_MAX", "4")][..],
            &[("MALLOC_ARENA_MAX", "")],
            &[("MALLOC_ARENA_MAX", "not-a-number")],
            &[("GLIBC_TUNABLES", "glibc.malloc.arena_max=3")],
            &[(
                "GLIBC_TUNABLES",
                "glibc.malloc.tcache_count=0:glibc.malloc.arena_max=",
            )],
            &[("GLIBC_TUNABLES", "glibc.malloc.arena_max")],
        ] {
            assert_eq!(run(pairs, true), (Outcome::ExternalOverride, None, None));
        }
    }

    #[test]
    fn a_rejected_cap_warns_with_the_fixed_text_and_start_up_continues() {
        let (outcome, value, warning) = run(&[], false);
        assert_eq!(outcome, Outcome::Failed);
        assert_eq!(value, Some(ARENA_MAX));
        assert_eq!(warning.as_deref(), Some(WARNING));
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    mod process {
        use super::super::*;
        use std::process::Command;
        use std::sync::{Arc, Barrier};

        const CASE: &str = "EVORCH_ALLOCATOR_PROBE_CASE";
        const THREADS: usize = 16;

        fn arena_count() -> usize {
            // SAFETY: `open_memstream` owns `buf`/`size` until `fclose`; the buffer is read
            // as a NUL-terminated string once the stream is closed, then freed.
            let xml = unsafe {
                let mut buf: *mut libc::c_char = std::ptr::null_mut();
                let mut size: libc::size_t = 0;
                let stream = libc::open_memstream(&mut buf, &mut size);
                assert!(!stream.is_null());
                assert_eq!(libc::malloc_info(0, stream), 0);
                assert_eq!(libc::fclose(stream), 0);
                let text = std::ffi::CStr::from_ptr(buf).to_string_lossy().into_owned();
                libc::free(buf.cast());
                text
            };
            xml.matches("<heap nr=").count()
        }

        /// Runs only inside a child started by `probe`; a plain run is a no-op.
        #[test]
        fn child_probe() {
            if std::env::var_os(CASE).is_none() {
                return;
            }
            let outcome = configure_at_entry();
            let allocated = Arc::new(Barrier::new(THREADS + 1));
            let release = Arc::new(Barrier::new(THREADS + 1));
            let workers: Vec<_> = (0..THREADS)
                .map(|_| {
                    let (allocated, release) = (allocated.clone(), release.clone());
                    std::thread::spawn(move || {
                        let held: Vec<Vec<u8>> = (0..256).map(|_| vec![1u8; 4096]).collect();
                        allocated.wait();
                        release.wait();
                        held.len()
                    })
                })
                .collect();
            allocated.wait();
            let arenas = arena_count();
            release.wait();
            for worker in workers {
                worker.join().unwrap();
            }
            println!("\nPROBE outcome={outcome:?} arenas={arenas}");
        }

        fn probe(env: &[(&str, &str)]) -> (String, usize) {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "allocator::tests::process::child_probe",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env_remove("MALLOC_ARENA_MAX")
                .env_remove("GLIBC_TUNABLES")
                .env(CASE, "1");
            for (key, value) in env {
                command.env(key, value);
            }
            let output = command.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            assert!(output.status.success(), "child failed: {stdout}");
            let line = stdout
                .lines()
                .find_map(|l| l.strip_prefix("PROBE "))
                .unwrap_or_else(|| panic!("no probe line: {stdout}"));
            let outcome = line
                .split_whitespace()
                .find_map(|w| w.strip_prefix("outcome="))
                .unwrap()
                .to_owned();
            let arenas = line
                .split_whitespace()
                .find_map(|w| w.strip_prefix("arenas="))
                .unwrap()
                .parse()
                .unwrap();
            (outcome, arenas)
        }

        #[test]
        fn threads_holding_memory_at_once_share_at_most_the_capped_arenas() {
            let (outcome, arenas) = probe(&[]);
            assert_eq!(outcome, "Applied");
            assert!(
                (1..=ARENA_MAX as usize).contains(&arenas),
                "arenas={arenas}"
            );
        }

        #[test]
        fn an_unrelated_tunable_does_not_suppress_the_cap() {
            let (outcome, arenas) = probe(&[("GLIBC_TUNABLES", "glibc.malloc.tcache_count=1")]);
            assert_eq!(outcome, "Applied");
            assert!(arenas <= ARENA_MAX as usize, "arenas={arenas}");
        }

        #[test]
        fn an_operator_chosen_limit_is_honoured_and_not_overwritten() {
            let (outcome, arenas) = probe(&[("MALLOC_ARENA_MAX", "4")]);
            assert_eq!(outcome, "ExternalOverride");
            assert!(arenas <= 4, "arenas={arenas}");
            let (outcome, arenas) = probe(&[("GLIBC_TUNABLES", "glibc.malloc.arena_max=3")]);
            assert_eq!(outcome, "ExternalOverride");
            assert!(arenas <= 3, "arenas={arenas}");
            let (outcome, _) = probe(&[("MALLOC_ARENA_MAX", "")]);
            assert_eq!(outcome, "ExternalOverride");
        }
    }
}
