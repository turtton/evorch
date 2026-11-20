//! Verified immutable helper inode, never a pathname-based spawn fallback.
use crate::{
    COMPAT_ID, TARGET,
    wire::{ABI, invalid},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileReceipt {
    pub file: String,
    pub size_bytes: u64,
    pub sha256: String,
}
fn valid_recipe(b: &Bundle) -> bool {
    use std::collections::BTreeSet;
    let mut args = b.helper.build_args.iter().map(String::as_str);
    if args.next() != Some("build") {
        return false;
    }
    let (mut locked, mut release) = (false, false);
    let (mut packages, mut bins, mut features) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    while let Some(flag) = args.next() {
        match flag {
            "--locked" if !locked => locked = true,
            "--release" if !release => release = true,
            "-p" | "--bin" => {
                let Some(value) = args.next() else {
                    return false;
                };
                if !(if flag == "-p" {
                    &mut packages
                } else {
                    &mut bins
                })
                .insert(value)
                {
                    return false;
                }
            }
            "--features" if features.is_empty() => {
                let Some(value) = args.next() else {
                    return false;
                };
                for feature in value.split(',') {
                    if !matches!(feature, "gui/cef" | "browser-cef/cef")
                        || !features.insert(feature)
                    {
                        return false;
                    }
                }
            }
            _ => return false,
        }
    }
    if !locked || release != (b.helper.profile == "release") {
        return false;
    }
    let set = |v: &[&'static str]| v.iter().copied().collect::<BTreeSet<_>>();
    if b.purpose != "distribution" {
        return packages == set(&["process-guard"])
            && bins == set(&["evorch-process-guard"])
            && features.is_empty();
    }
    if !release {
        return false;
    }
    let roles = b
        .callers
        .iter()
        .map(|c| c.role.as_str())
        .collect::<BTreeSet<_>>();
    if roles.len() != b.callers.len() {
        return false;
    }
    (packages == set(&["evorch-core", "process-guard"])
        && bins == set(&["evorch-core", "evorch-process-guard"])
        && features.is_empty()
        && roles == set(&["core"]))
        || (packages == set(&["evorch", "evorch-core", "process-guard"])
            && bins == set(&["evorch", "evorch-core", "evorch-process-guard"])
            && features.is_empty()
            && roles == set(&["cli", "core"]))
        || (packages == set(&["gui", "browser-cef", "evorch-renderer", "process-guard"])
            && bins
                == set(&[
                    "evorch-gui",
                    "evorch-cef-helper",
                    "evorch-renderer",
                    "evorch-process-guard",
                ])
            && features == set(&["gui/cef", "browser-cef/cef"])
            && roles == set(&["gui"]))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelperReceipt {
    pub package: String,
    pub bin: String,
    pub file: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub profile: String,
    pub features: Vec<String>,
    pub build_args: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallerReceipt {
    pub role: String,
    pub file: String,
    pub size_bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bundle {
    pub schema_version: u32,
    pub kind: String,
    pub purpose: String,
    pub target: String,
    pub source_sha: String,
    pub guard_abi: u16,
    pub guard_compat_id: String,
    pub helper: HelperReceipt,
    pub callers: Vec<CallerReceipt>,
}
#[derive(Debug)]
pub struct GuardImage {
    file: File,
    pub bundle: Bundle,
    pub manifest_path: PathBuf,
    dev: u64,
    ino: u64,
}
pub fn digest(file: &mut File) -> io::Result<(u64, String)> {
    let mut h = Sha256::new();
    let mut size = 0;
    let mut b = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
        size += n as u64;
    }
    Ok((size, format!("{:x}", h.finalize())))
}
fn sealed(f: &File, mode: u32) -> io::Result<()> {
    let m = f.metadata()?;
    if !m.is_file() || m.mode() & 0o777 != mode {
        return Err(invalid(
            "guard bundle requires regular 0444 manifest and 0555 helper (including no owner-write bit)",
        ));
    }
    Ok(())
}
fn open_at(dir: &File, name: &str, flags: i32) -> io::Result<File> {
    let c = std::ffi::CString::new(name).map_err(|_| invalid("NUL in guard path"))?;
    let n = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(n) })
}
fn hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
impl GuardImage {
    pub fn open(manifest: &Path, installed_role: Option<&str>) -> io::Result<Self> {
        if !manifest.is_absolute() {
            return Err(invalid(
                "EVORCH_PROCESS_GUARD_BUNDLE must be an absolute manifest path; run scripts/guard-artifact.sh prepare",
            ));
        }
        let parent = manifest
            .parent()
            .ok_or_else(|| invalid("guard manifest parent"))?;
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY)
            .open(parent)?;
        let name = manifest
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| invalid("guard manifest filename"))?;
        let mut mf = open_at(&dir, name, libc::O_RDONLY)?;
        sealed(&mf, 0o444)?;
        if mf.metadata()?.len() > 64 * 1024 {
            return Err(invalid("guard manifest too large"));
        }
        let b: Bundle = serde_json::from_reader(&mut mf)
            .map_err(|e| invalid(&format!("guard manifest JSON: {e}")))?;
        if b.schema_version != 1
            || b.kind != "evorch-guard-bundle"
            || b.target != TARGET
            || b.guard_abi != ABI
            || b.guard_compat_id != COMPAT_ID
            || !hex(&b.source_sha, 40)
        {
            return Err(invalid("guard schema/target/source/ABI/compat mismatch"));
        }
        if !["distribution", "development", "test-helper"].contains(&b.purpose.as_str())
            || (b.purpose != "test-helper" && b.callers.is_empty())
            || (installed_role.is_some() && b.purpose == "test-helper")
        {
            return Err(invalid("guard bundle purpose/caller binding mismatch"));
        }
        let h = &b.helper;
        if h.package != "process-guard"
            || h.bin != "evorch-process-guard"
            || !h.features.is_empty()
            || !hex(&h.sha256, 64)
            || !["release", "dev"].contains(&h.profile.as_str())
        {
            return Err(invalid("guard helper receipt mismatch"));
        }
        if !valid_recipe(&b) {
            return Err(invalid("guard build recipe mismatch"));
        }
        let expected_dev = format!(".evorch-guard/{}/evorch-process-guard", h.sha256);
        if h.file != "evorch-process-guard"
            && !(b.purpose == "development" && h.file == expected_dev)
        {
            return Err(invalid("noncanonical guard helper relative path"));
        }
        let mut d = dir;
        let components: Vec<_> = h.file.split('/').collect();
        for part in &components[..components.len() - 1] {
            d = open_at(&d, part, libc::O_RDONLY | libc::O_DIRECTORY)?;
        }
        let mut file = crate::non_stdio(open_at(
            &d,
            components[components.len() - 1],
            libc::O_RDONLY,
        )?)?;
        sealed(&file, 0o555)?;
        if digest(&mut file)? != (h.size_bytes, h.sha256.clone()) {
            return Err(invalid("guard helper hash/size mismatch"));
        }
        let m = file.metadata()?;
        if let Some(role) = installed_role {
            let mut exe = File::open("/proc/self/exe")?;
            let (size, sha) = digest(&mut exe)?;
            let actual = std::env::current_exe()?.canonicalize()?;
            let filename = actual
                .file_name()
                .and_then(|p| p.to_str())
                .ok_or_else(|| invalid("caller ELF filename"))?;
            if actual.parent() != Some(parent)
                || !b.callers.iter().any(|c| {
                    c.role == role && c.file == filename && c.size_bytes == size && c.sha256 == sha
                })
            {
                return Err(invalid("running caller ELF is not bound to guard bundle"));
            }
        }
        Ok(Self {
            file,
            bundle: b,
            manifest_path: manifest.into(),
            dev: m.dev(),
            ino: m.ino(),
        })
    }
    pub fn inherited_exec_path(&self) -> PathBuf {
        format!("/proc/self/fd/{}", self.file.as_raw_fd()).into()
    }
    /// PTY-only locator: portable-pty closes inherited fd >2 before exec.
    /// Caller must retain this image and share its PID namespace until exec.
    /// Missing proc access fails closed; this never reopens the distribution path.
    pub fn pty_exec_path(&self) -> PathBuf {
        format!("/proc/{}/fd/{}", std::process::id(), self.file.as_raw_fd()).into()
    }
    pub fn identity_args(&self) -> [String; 2] {
        [self.dev.to_string(), self.ino.to_string()]
    }
    pub fn identity(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
}
/// Must run before connecting or forking. Protects compatible guard images;
/// an arbitrary non-guard ELF cannot be forced to perform this check.
pub fn verify_running_identity(dev: u64, ino: u64) -> io::Result<()> {
    let m = std::fs::metadata("/proc/self/exe")?;
    if (m.dev(), m.ino()) != (dev, ino) {
        return Err(invalid("guard running inode identity mismatch"));
    }
    Ok(())
}
