//! Opt-in startup measurements (`RUST_LOG=info,gui::restore=info`).
//! Memory is process-wide, best-effort, and absent on non-Linux platforms.

use std::time::Instant;

pub struct RestoreTimer {
    stage: &'static str,
    started: Option<Instant>,
}

impl RestoreTimer {
    pub fn new(stage: &'static str) -> Self {
        Self {
            stage,
            started: tracing::enabled!(target: "gui::restore", tracing::Level::INFO)
                .then(Instant::now),
        }
    }

    pub fn finish(self) {
        if let Some(started) = self.started {
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            let (vm_rss_kib, vm_hwm_kib) = process_memory();
            tracing::info!(
                target: "gui::restore",
                stage = self.stage,
                elapsed_ms,
                vm_rss_kib,
                vm_hwm_kib,
                "restore stage complete"
            );
        }
    }
}

pub(crate) fn process_memory() -> (Option<u64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        return memory_from_status(&status);
    }
    (None, None)
}

#[cfg(any(target_os = "linux", test))]
fn memory_from_status(status: &str) -> (Option<u64>, Option<u64>) {
    let field = |name| {
        status.lines().find_map(|line| {
            let value = line.strip_prefix(name)?;
            let mut words = value.split_whitespace();
            let number = words.next()?.parse().ok()?;
            (words.next()? == "kB").then_some(number)
        })
    };
    (field("VmRSS:"), field("VmHWM:"))
}

#[cfg(test)]
mod tests {
    use super::memory_from_status;

    #[test]
    fn proc_memory_is_optional_and_uses_kib() {
        assert_eq!(
            memory_from_status("Name:\ttest\nVmHWM:\t123 kB\nVmRSS:\t42 kB\n"),
            (Some(42), Some(123))
        );
        assert_eq!(
            memory_from_status("VmRSS: bad kB\nVmHWM: 4 MB\n"),
            (None, None)
        );
        assert_eq!(memory_from_status(""), (None, None));
    }
}
