//! The product artifact: a feature-free `evorch-core` built separately with
//! the shipped arguments, handed over as a sealed immutable copy plus a manifest
//! by I0's contract (`test_support::product_artifact`).
//!
//! `ProductManifest` is this fixture's own view of an artifact. A sealed release
//! artifact is converted into one; a *measurement-only* manifest of the same
//! shape describes the debug build, which the sealed contract does not cover.
//!
//! A missing or inconsistent artifact is a failure of the fixture. It is never
//! skipped and never replaced by a binary Cargo happened to build for the test.
use super::names::{CARGO_HARNESS_BIN_ENV, ENV_MEASURE_MANIFEST, MANIFEST_SCHEMA};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fmt,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::Duration,
};

/// How the environment is read. A closure keeps negative tests from touching
/// the process environment.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<OsString>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductManifest {
    pub schema: u32,
    pub package: String,
    pub bin: String,
    /// "debug" or "release".
    pub profile: String,
    /// The exact Cargo arguments of the artifact build.
    pub cargo_args: Vec<String>,
    /// Enabled features of `evorch-core`. A product has none.
    pub features: Vec<String>,
    pub rustc: String,
    pub cargo: String,
    pub target: String,
    pub source_revision: String,
    /// Artifact file, relative to the manifest directory or absolute.
    pub artifact: String,
    pub elf_size: u64,
    /// `sha256:<hex>` of the whole ELF: the build identity the core reports.
    pub elf_sha256: String,
    /// Nothing is stripped to make a test pass; always false.
    pub stripped: bool,
}

#[derive(Debug)]
pub enum ArtifactError {
    /// The variable that names the manifest is not set.
    NotProvided(&'static str),
    Manifest(String),
    Mismatch(String),
    Io(String),
}
impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotProvided(name) => write!(
                f,
                "product artifact not provided: {name} is not set (the fixture fails; it does not skip and does not use CARGO_BIN_EXE_*)"
            ),
            Self::Manifest(m) => write!(f, "invalid product manifest: {m}"),
            Self::Mismatch(m) => write!(f, "product artifact does not match its manifest: {m}"),
            Self::Io(m) => write!(f, "product artifact unreadable: {m}"),
        }
    }
}
impl std::error::Error for ArtifactError {}

/// A verified artifact. Cloning shares the verification.
#[derive(Debug, Clone)]
pub struct ProductArtifact {
    pub manifest: ProductManifest,
    pub manifest_path: PathBuf,
    pub path: PathBuf,
    identity: FileIdentity,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}
impl FileIdentity {
    fn of(path: &Path) -> std::io::Result<Self> {
        let m = std::fs::metadata(path)?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
        })
    }
}

impl ProductArtifact {
    /// The sealed release artifact named by `EVORCH_PRODUCT_CORE_EXE` and
    /// `EVORCH_PRODUCT_MANIFEST`, verified by I0's reader (existence, sealed
    /// copy outside any `target` directory, manifest contract, features, size,
    /// SHA-256) and by this fixture (never the `CARGO_BIN_EXE_*` build).
    /// Panics on any failure; there is no skip. The first verification of a
    /// process is reused.
    #[track_caller]
    pub fn require() -> Self {
        static VERIFIED: OnceLock<ProductArtifact> = OnceLock::new();
        if let Some(artifact) = VERIFIED.get() {
            artifact.assert_unchanged();
            return artifact.clone();
        }
        let sealed = test_support::product_artifact::require_from_env();
        let artifact = Self::from_sealed(&sealed, &|name| std::env::var_os(name))
            .unwrap_or_else(|error| panic!("F4-P product fixture: {error}"));
        VERIFIED.get_or_init(|| artifact).clone()
    }

    /// MEASUREMENT ONLY: the artifact of `EVORCH_F4P_MEASURE_MANIFEST` (any
    /// profile, in this fixture's manifest format) when it is set, else the
    /// sealed release artifact.
    #[track_caller]
    pub fn require_for_measurement() -> Self {
        match std::env::var_os(ENV_MEASURE_MANIFEST).filter(|v| !v.is_empty()) {
            Some(path) => Self::open(Path::new(&path), &|name| std::env::var_os(name))
                .unwrap_or_else(|error| panic!("F4-P measurement artifact: {error}")),
            None => Self::require(),
        }
    }

    /// Converts the sealed artifact into this fixture's view of it.
    pub fn from_sealed(
        sealed: &test_support::product_artifact::ProductArtifact,
        lookup: Lookup<'_>,
    ) -> Result<Self, ArtifactError> {
        let path = sealed
            .exe
            .canonicalize()
            .map_err(|e| ArtifactError::Io(e.to_string()))?;
        Self::reject_harness_build(&path, lookup)?;
        let m = &sealed.manifest;
        let manifest = ProductManifest {
            schema: MANIFEST_SCHEMA,
            package: m.package.clone(),
            bin: m.bin.clone(),
            profile: m.profile.clone(),
            cargo_args: m.build_args.clone(),
            features: m.features.clone(),
            rustc: m.rustc.clone(),
            cargo: String::new(),
            target: m.target.clone(),
            source_revision: m.source_sha.clone(),
            artifact: m.file.clone(),
            elf_size: m.size_bytes,
            elf_sha256: format!("sha256:{}", m.sha256),
            stripped: false,
        };
        Self::check_manifest(&manifest)?;
        let identity = FileIdentity::of(&path).map_err(|e| ArtifactError::Io(e.to_string()))?;
        Ok(Self {
            manifest,
            manifest_path: sealed.manifest_path.clone(),
            path,
            identity,
        })
    }

    /// The harness build is never the product, even when handed over by
    /// mistake: refuse a file that is the `CARGO_BIN_EXE_*` build.
    fn reject_harness_build(path: &Path, lookup: Lookup<'_>) -> Result<(), ArtifactError> {
        let metadata = std::fs::metadata(path).map_err(|e| ArtifactError::Io(e.to_string()))?;
        if let Some(harness) = lookup(CARGO_HARNESS_BIN_ENV)
            && let Ok(harness) = Path::new(&harness).canonicalize()
            && let Ok(harness) = std::fs::metadata(harness)
            && harness.dev() == metadata.dev()
            && harness.ino() == metadata.ino()
        {
            return Err(ArtifactError::Mismatch(format!(
                "the artifact is the {CARGO_HARNESS_BIN_ENV} build, not a separately built sealed copy"
            )));
        }
        Ok(())
    }

    /// MEASUREMENT ONLY manifest, named by `EVORCH_F4P_MEASURE_MANIFEST`.
    pub fn open_measurement_from(lookup: Lookup<'_>) -> Result<Self, ArtifactError> {
        let manifest_path = lookup(ENV_MEASURE_MANIFEST)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(ArtifactError::NotProvided(ENV_MEASURE_MANIFEST))?;
        Self::open(&manifest_path, lookup)
    }

    pub fn open(manifest_path: &Path, lookup: Lookup<'_>) -> Result<Self, ArtifactError> {
        let text = std::fs::read_to_string(manifest_path)
            .map_err(|e| ArtifactError::Io(format!("{}: {e}", manifest_path.display())))?;
        let manifest: ProductManifest =
            serde_json::from_str(&text).map_err(|e| ArtifactError::Manifest(e.to_string()))?;
        Self::check_manifest(&manifest)?;
        let artifact = {
            let named = PathBuf::from(&manifest.artifact);
            if named.is_absolute() {
                named
            } else {
                manifest_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(named)
            }
        };
        let path = artifact
            .canonicalize()
            .map_err(|e| ArtifactError::Io(format!("{}: {e}", artifact.display())))?;
        let metadata = std::fs::metadata(&path).map_err(|e| ArtifactError::Io(e.to_string()))?;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return Err(ArtifactError::Mismatch(
                "not an executable regular file".into(),
            ));
        }
        if metadata.len() != manifest.elf_size {
            return Err(ArtifactError::Mismatch(format!(
                "size {} != manifest {}",
                metadata.len(),
                manifest.elf_size
            )));
        }
        Self::reject_harness_build(&path, lookup)?;
        let identity = FileIdentity::of(&path).map_err(|e| ArtifactError::Io(e.to_string()))?;
        let actual = core_instance::handoff::elf_identity(&path)
            .map_err(|e| ArtifactError::Io(e.to_string()))?;
        if actual != manifest.elf_sha256 {
            return Err(ArtifactError::Mismatch(format!(
                "hash {actual} != manifest {}",
                manifest.elf_sha256
            )));
        }
        Ok(Self {
            manifest,
            manifest_path: manifest_path.to_owned(),
            path,
            identity,
        })
    }

    /// What makes an artifact a product, independent of the file.
    pub fn check_manifest(m: &ProductManifest) -> Result<(), ArtifactError> {
        let bad = |why: &str| Err(ArtifactError::Manifest(why.into()));
        if m.schema != MANIFEST_SCHEMA {
            return bad("unsupported schema revision");
        }
        if m.package != "evorch-core" || m.bin != "evorch-core" {
            return bad("the artifact is not the evorch-core binary");
        }
        if !m.features.is_empty() {
            return bad("a product is built without features");
        }
        let has = |a: &str| m.cargo_args.iter().any(|x| x == a);
        if [
            "--features",
            "-F",
            "--all-features",
            "--no-default-features",
        ]
        .iter()
        .any(|a| has(a) || m.cargo_args.iter().any(|x| x.starts_with(&format!("{a}="))))
        {
            return bad("the build arguments select features");
        }
        let positional = |flag: &str, value: &str| {
            m.cargo_args
                .windows(2)
                .any(|w| w[0] == flag && w[1] == value)
        };
        if !positional("-p", "evorch-core") || !positional("--bin", "evorch-core") {
            return bad("the build arguments are not `-p evorch-core --bin evorch-core`");
        }
        match m.profile.as_str() {
            "release" if has("--release") => (),
            "debug" if !has("--release") && !has("--profile") => (),
            _ => return bad("profile does not match the build arguments"),
        }
        if m.stripped {
            return bad("a stripped artifact is not accepted");
        }
        if !m.elf_sha256.starts_with("sha256:") || m.elf_size == 0 {
            return bad("missing ELF size/hash");
        }
        Ok(())
    }

    /// Cheap immutability check (identity, size, mtime) for each launch.
    #[track_caller]
    pub fn assert_unchanged(&self) {
        let now = FileIdentity::of(&self.path)
            .unwrap_or_else(|e| panic!("product artifact vanished: {e}"));
        assert_eq!(now, self.identity, "the product artifact was modified");
    }

    /// Full re-hash, for the points where a launch must be provably this build.
    pub fn verify_hash(&self) -> Result<(), ArtifactError> {
        let actual = core_instance::handoff::elf_identity(&self.path)
            .map_err(|e| ArtifactError::Io(e.to_string()))?;
        if actual == self.manifest.elf_sha256 {
            Ok(())
        } else {
            Err(ArtifactError::Mismatch(format!("hash {actual}")))
        }
    }

    pub fn build_identity(&self) -> &str {
        &self.manifest.elf_sha256
    }

    /// `--describe-handoff`: the build identity the product itself reports.
    /// Runs the real entry; the output is parsed with the launcher's type.
    pub fn describe(&self) -> Result<core_launcher::handoff::HandoffDescription, String> {
        let mut command = Command::new(&self.path);
        command.arg("--describe-handoff");
        super::env::passthrough(&mut command);
        let output = command.output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "describe failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())
    }
}

/// `Read` helper kept here so measurement code can time one pass of the same
/// kind the launcher does, without reaching into private functions.
pub fn hash_pass(path: &Path) -> std::io::Result<Duration> {
    use sha2::{Digest, Sha256};
    let start = std::time::Instant::now();
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    std::hint::black_box(hash.finalize());
    Ok(start.elapsed())
}

/// The build data a manifest needs, taken from an artifact the caller built.
/// The caller states the arguments it really used; nothing is inferred.
pub fn describe_build(
    elf: &Path,
    profile: &str,
    cargo_args: &[&str],
    artifact_name: &str,
) -> Result<ProductManifest, String> {
    let tool = |program: &str| -> Result<String, String> {
        let out = Command::new(program)
            .arg("--version")
            .output()
            .map_err(|e| format!("{program}: {e}"))?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into());
    let verbose = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|e| e.to_string())?;
    let target = String::from_utf8_lossy(&verbose.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_owned))
        .unwrap_or_default();
    Ok(ProductManifest {
        schema: MANIFEST_SCHEMA,
        package: "evorch-core".into(),
        bin: "evorch-core".into(),
        profile: profile.into(),
        cargo_args: cargo_args.iter().map(|s| (*s).to_owned()).collect(),
        features: vec![],
        rustc: tool("rustc")?,
        cargo: tool("cargo")?,
        target,
        source_revision: revision,
        artifact: artifact_name.into(),
        elf_size: std::fs::metadata(elf).map_err(|e| e.to_string())?.len(),
        elf_sha256: core_instance::handoff::elf_identity(elf).map_err(|e| e.to_string())?,
        stripped: false,
    })
}
