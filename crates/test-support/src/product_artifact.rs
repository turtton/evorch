//! The sealed product artifact: the `evorch-core` binary as it is distributed,
//! built apart from every test build, with a manifest that describes it.
//!
//! On Linux the product is a closure of four files in one directory: the core,
//! the `evorch-process-guard` helper built by the same release build, the
//! helper's runtime bundle (`guard-bundle.json`) and the manifest. Elsewhere
//! the product is the core and its manifest alone, because no helper exists.
//!
//! CI builds it once in the `product-artifact` job
//! (`cargo build --locked --release -p evorch-core -p process-guard --bin
//! evorch-core --bin evorch-process-guard` on Linux, no feature), seals it with
//! `scripts/product-artifact.sh`, and hands the immutable copy to every job
//! that runs a product test (the offline test shards that include the cache
//! gate, `browser-e2e` and `cef-e2e`). Test builds never write to that copy,
//! and a test never falls back to a binary that a test build produced
//! (`CARGO_BIN_EXE_*`) or to a helper from anywhere else.
//!
//! The contract is fixed here:
//!
//! - [`EXE_ENV`] names the sealed executable and [`MANIFEST_ENV`] its manifest.
//! - The manifest is a schema 2 JSON object with the fields [`verify`] checks:
//!   see [`Manifest`]. Schema 1 is refused.
//! - On Linux the manifest records the helper ([`GuardReceipt`]) and the bundle
//!   ([`BundleReceipt`]); [`verify`] also opens the bundle with
//!   `process_guard::image::GuardImage` and requires its receipts to be the
//!   manifest's, field by field. Elsewhere both are `null`.
//!
//! A test that needs the product calls [`require_from_env`]. A missing
//! variable, file, manifest, hash, helper, or feature fails it: there is no skip.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Environment variable naming the sealed `evorch-core` executable.
pub const EXE_ENV: &str = "EVORCH_PRODUCT_CORE_EXE";
/// Environment variable naming the manifest of the sealed executable.
pub const MANIFEST_ENV: &str = "EVORCH_PRODUCT_MANIFEST";
/// The only manifest schema version this reader understands.
pub const SCHEMA_VERSION: u64 = 2;
/// The `kind` field of every product manifest.
pub const KIND: &str = "evorch-core-product";
/// File name of the sealed helper beside the core (Linux).
pub const GUARD_FILE: &str = "evorch-process-guard";
/// File name of the helper's runtime bundle beside the core (Linux).
pub const GUARD_BUNDLE_FILE: &str = "guard-bundle.json";
/// The package and binary selection of the Linux distribution (`flake.nix`
/// `evorch-core`), without a feature: the core and the helper.
pub const DISTRIBUTION_ARGS: [&str; 8] = [
    "-p",
    "evorch-core",
    "-p",
    "process-guard",
    "--bin",
    "evorch-core",
    "--bin",
    "evorch-process-guard",
];
/// The complete Cargo command the Linux product artifact is built with. The
/// bundle records this recipe too, not a standalone helper build.
pub const BUILD_ARGS: [&str; 11] = [
    "build",
    "--locked",
    "--release",
    "-p",
    "evorch-core",
    "-p",
    "process-guard",
    "--bin",
    "evorch-core",
    "--bin",
    "evorch-process-guard",
];
/// The package and binary selection where no helper exists (not Linux).
pub const CORE_ONLY_DISTRIBUTION_ARGS: [&str; 4] = ["-p", "evorch-core", "--bin", "evorch-core"];
/// The complete Cargo command where no helper exists (not Linux).
pub const CORE_ONLY_BUILD_ARGS: [&str; 7] = [
    "build",
    "--locked",
    "--release",
    "-p",
    "evorch-core",
    "--bin",
    "evorch-core",
];
/// The only profile of the product artifact: the one the distribution uses.
pub const PROFILE: &str = "release";

/// The helper as the manifest records it: the receipt of the bundle's
/// `helper` plus the ABI and compatibility identity of the guard protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardReceipt {
    pub package: String,
    pub bin: String,
    pub file: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub profile: String,
    pub features: Vec<String>,
    pub build_args: Vec<String>,
    pub guard_abi: u64,
    pub guard_compat_id: String,
}

/// The helper's runtime bundle file as the manifest records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleReceipt {
    pub file: String,
    pub size_bytes: u64,
    pub sha256: String,
}

/// The manifest fields a test may rely on after [`verify`] succeeded.
///
/// Every field is required in the JSON. `features` must be empty and
/// `build_args` must equal [`BUILD_ARGS`] (Linux) or [`CORE_ONLY_BUILD_ARGS`];
/// `file`, `size_bytes` and `sha256` describe the sealed executable; `target`,
/// `rustc`, `rustflags` and `source_sha` record how it was built. `guard` and
/// `guard_bundle` are both present on Linux and both `null` elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub schema_version: u64,
    pub kind: String,
    pub package: String,
    pub bin: String,
    pub distribution_args: Vec<String>,
    pub build_args: Vec<String>,
    pub features: Vec<String>,
    pub profile: String,
    pub target: String,
    pub rustc: String,
    pub rustflags: String,
    pub source_sha: String,
    pub file: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub guard: Option<GuardReceipt>,
    pub guard_bundle: Option<BundleReceipt>,
}

/// The verified helper and bundle files beside the core (Linux).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardFiles {
    pub helper: PathBuf,
    pub bundle: PathBuf,
}

/// A verified product artifact.
#[derive(Debug, Clone)]
pub struct ProductArtifact {
    pub exe: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
    /// The helper and bundle: `Some` on Linux, `None` where no helper exists.
    pub guard: Option<GuardFiles>,
}

impl ProductArtifact {
    /// Copies the whole closure into the existing directory `dest` and verifies
    /// the copy as a product of its own. The core is never copied alone.
    ///
    /// # Errors
    /// A file that cannot be copied, or a copy that [`verify`] refuses (a
    /// destination inside a Cargo `target` directory, for one).
    pub fn copy_closure_into(&self, dest: &Path) -> Result<Self, String> {
        let mut files = vec![(&self.exe, true), (&self.manifest_path, false)];
        if let Some(guard) = &self.guard {
            files.push((&guard.helper, true));
            files.push((&guard.bundle, false));
        }
        for (from, executable) in files {
            let name = from
                .file_name()
                .ok_or_else(|| format!("{} has no file name", from.display()))?;
            let to = dest.join(name);
            std::fs::copy(from, &to).map_err(|error| {
                format!(
                    "cannot copy {} to {}: {error}",
                    from.display(),
                    to.display()
                )
            })?;
            seal_copy(&to, executable)?;
        }
        verify(
            &dest.join(&self.manifest.file),
            &dest.join(
                self.manifest_path
                    .file_name()
                    .ok_or("manifest has no file name")?,
            ),
        )
    }
}

#[cfg(unix)]
fn seal_copy(path: &Path, executable: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o555 } else { 0o444 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("cannot seal {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn seal_copy(_: &Path, _: bool) -> Result<(), String> {
    Ok(())
}

/// Verifies the artifact that [`EXE_ENV`] and [`MANIFEST_ENV`] name.
///
/// # Errors
/// Describes the first violated rule, including how to produce the artifact
/// when a variable is unset.
pub fn verify_from_env() -> Result<ProductArtifact, String> {
    let read = |name: &str| {
        std::env::var_os(name).map(PathBuf::from).ok_or_else(|| {
            format!(
                "{name} is not set: build and seal the product artifact with \
                 `scripts/product-artifact.sh build <dir>` and export the variables it prints"
            )
        })
    };
    verify(&read(EXE_ENV)?, &read(MANIFEST_ENV)?)
}

/// [`verify_from_env`] that panics with the reason.
///
/// # Panics
/// Whenever the artifact is absent or does not match its manifest.
#[must_use]
pub fn require_from_env() -> ProductArtifact {
    match verify_from_env() {
        Ok(artifact) => artifact,
        Err(reason) => panic!("product artifact check failed: {reason}"),
    }
}

/// Verifies that `exe` is the sealed copy `manifest` describes.
///
/// # Errors
/// Describes the first violated rule: a missing or symlinked file, a path
/// inside a Cargo `target` directory, a manifest that is not the contract of
/// this module, a feature or argument that differs from the distribution, a
/// size or SHA-256 that differs from the manifest, or a copy that is writable
/// or not executable.
pub fn verify(exe: &Path, manifest_path: &Path) -> Result<ProductArtifact, String> {
    verify_for(exe, manifest_path, cfg!(target_os = "linux"))
}

/// [`verify`] for a platform that has the helper (`linux`) or not.
fn verify_for(exe: &Path, manifest_path: &Path, linux: bool) -> Result<ProductArtifact, String> {
    let exe_meta = regular_file(exe, "product executable")?;
    let manifest_meta = regular_file(manifest_path, "product manifest")?;
    let canonical = exe
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", exe.display()))?;
    if canonical
        .components()
        .any(|part| part.as_os_str() == "target")
    {
        return Err(format!(
            "{} lies inside a `target` directory: the product artifact must be a sealed \
             copy that no test build can overwrite",
            canonical.display()
        ));
    }
    let manifest = parse_manifest(manifest_path)?;
    check_contract(&manifest, linux)?;
    let name = exe.file_name().and_then(|name| name.to_str());
    if name != Some(manifest.file.as_str()) {
        return Err(format!(
            "executable is named {name:?} but the manifest says {:?}",
            manifest.file
        ));
    }
    if exe_meta.len() != manifest.size_bytes {
        return Err(format!(
            "executable has {} bytes, the manifest says {}",
            exe_meta.len(),
            manifest.size_bytes
        ));
    }
    let actual = sha256_hex(exe)?;
    if actual != manifest.sha256 {
        return Err(format!(
            "executable sha256 is {actual}, the manifest says {}",
            manifest.sha256
        ));
    }
    check_sealed(&exe_meta, true, "executable")?;
    check_sealed(&manifest_meta, false, "manifest")?;
    let guard = check_guard(&manifest, exe, linux)?;
    Ok(ProductArtifact {
        exe: exe.to_path_buf(),
        manifest_path: manifest_path.to_path_buf(),
        manifest,
        guard,
    })
}

/// Verifies the helper and the bundle beside the core, or their absence where
/// no helper exists.
fn check_guard(manifest: &Manifest, exe: &Path, linux: bool) -> Result<Option<GuardFiles>, String> {
    let Some(guard) = &manifest.guard else {
        return if linux {
            Err("manifest guard is null: the Linux product carries its helper".to_owned())
        } else {
            Ok(None)
        };
    };
    if !linux {
        return Err("manifest names a guard helper, but this platform has none".to_owned());
    }
    let bundle = manifest
        .guard_bundle
        .as_ref()
        .ok_or("manifest guard_bundle is null: the Linux product carries its bundle")?;
    if guard.file != GUARD_FILE || bundle.file != GUARD_BUNDLE_FILE {
        return Err(format!(
            "manifest names the helper {:?} and the bundle {:?}, expected {GUARD_FILE:?} and \
             {GUARD_BUNDLE_FILE:?}",
            guard.file, bundle.file
        ));
    }
    check_guard_receipt(manifest, guard)?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no directory", exe.display()))?;
    let helper_path = dir.join(&guard.file);
    let bundle_path = dir.join(&bundle.file);
    let helper_meta = regular_file(&helper_path, "guard helper")?;
    let bundle_meta = regular_file(&bundle_path, "guard bundle")?;
    check_file_receipt(
        &helper_path,
        &helper_meta,
        guard.size_bytes,
        &guard.sha256,
        "guard helper",
    )?;
    check_file_receipt(
        &bundle_path,
        &bundle_meta,
        bundle.size_bytes,
        &bundle.sha256,
        "guard bundle",
    )?;
    check_sealed(&helper_meta, true, "guard helper")?;
    check_sealed(&bundle_meta, false, "guard bundle")?;
    check_bundle(&bundle_path, manifest, guard)?;
    Ok(Some(GuardFiles {
        helper: helper_path,
        bundle: bundle_path,
    }))
}

fn check_file_receipt(
    path: &Path,
    meta: &std::fs::Metadata,
    size: u64,
    sha256: &str,
    what: &str,
) -> Result<(), String> {
    if meta.len() != size {
        return Err(format!(
            "{what} has {} bytes, the manifest says {size}",
            meta.len()
        ));
    }
    let actual = sha256_hex(path)?;
    if actual != sha256 {
        return Err(format!(
            "{what} sha256 is {actual}, the manifest says {sha256}"
        ));
    }
    Ok(())
}

fn check_guard_receipt(manifest: &Manifest, guard: &GuardReceipt) -> Result<(), String> {
    if guard.package != "process-guard" || guard.bin != GUARD_FILE {
        return Err(format!(
            "manifest guard is {}/{}, expected process-guard/{GUARD_FILE}",
            guard.package, guard.bin
        ));
    }
    if guard.profile != PROFILE {
        return Err(format!(
            "manifest guard profile is {:?}, expected {PROFILE:?}",
            guard.profile
        ));
    }
    if !guard.features.is_empty() {
        return Err(format!(
            "manifest guard features are {:?}: the helper is built without a feature",
            guard.features
        ));
    }
    if guard.build_args != manifest.build_args {
        return Err(format!(
            "manifest guard build_args are {:?}, expected the product recipe {:?}",
            guard.build_args, manifest.build_args
        ));
    }
    if !is_hex(&guard.sha256, 64) || !is_hex(&guard.guard_compat_id, 64) {
        return Err("manifest guard sha256 or guard_compat_id is not a SHA-256".to_owned());
    }
    Ok(())
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Opens the bundle as a library would (no installed caller) and requires its
/// receipts to be the manifest's: the nested receipt may not differ from the
/// root receipt in any field.
#[cfg(target_os = "linux")]
fn check_bundle(
    bundle_path: &Path,
    manifest: &Manifest,
    guard: &GuardReceipt,
) -> Result<(), String> {
    use process_guard::image::{CallerReceipt, GuardImage, HelperReceipt};
    let image = GuardImage::open(bundle_path, None)
        .map_err(|error| format!("guard bundle {} is refused: {error}", bundle_path.display()))?;
    let bundle = &image.bundle;
    let mismatch = |field: &str| {
        Err(format!(
            "guard bundle {} disagrees with the manifest on {field}",
            bundle_path.display()
        ))
    };
    if bundle.purpose != "distribution" {
        return mismatch("purpose");
    }
    if bundle.source_sha != manifest.source_sha {
        return mismatch("source_sha");
    }
    if bundle.target != manifest.target {
        return mismatch("target");
    }
    if u64::from(bundle.guard_abi) != guard.guard_abi {
        return mismatch("guard_abi");
    }
    if bundle.guard_compat_id != guard.guard_compat_id {
        return mismatch("guard_compat_id");
    }
    let helper = HelperReceipt {
        package: guard.package.clone(),
        bin: guard.bin.clone(),
        file: guard.file.clone(),
        size_bytes: guard.size_bytes,
        sha256: guard.sha256.clone(),
        profile: guard.profile.clone(),
        features: guard.features.clone(),
        build_args: guard.build_args.clone(),
    };
    if serde_json::to_value(&bundle.helper).ok() != serde_json::to_value(&helper).ok() {
        return mismatch("helper");
    }
    let core = CallerReceipt {
        role: "core".to_owned(),
        file: manifest.file.clone(),
        size_bytes: manifest.size_bytes,
        sha256: manifest.sha256.clone(),
    };
    if serde_json::to_value(&bundle.callers).ok() != serde_json::to_value([core]).ok() {
        return mismatch("callers (the core must be the only caller)");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn check_bundle(_: &Path, _: &Manifest, _: &GuardReceipt) -> Result<(), String> {
    Err("a guard bundle can be opened on Linux only".to_owned())
}

fn regular_file(path: &Path, what: &str) -> Result<std::fs::Metadata, String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| format!("{what} {} is unreadable: {error}", path.display()))?;
    if !meta.file_type().is_file() {
        return Err(format!(
            "{what} {} is not a regular file (a symlink is not a sealed copy)",
            path.display()
        ));
    }
    Ok(meta)
}

fn check_contract(manifest: &Manifest, linux: bool) -> Result<(), String> {
    let wanted = |actual: &str, expected: &str, field: &str| {
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "manifest {field} is {actual:?}, expected {expected:?}"
            ))
        }
    };
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "manifest schema_version is {}, expected {SCHEMA_VERSION}",
            manifest.schema_version
        ));
    }
    wanted(&manifest.kind, KIND, "kind")?;
    wanted(&manifest.package, "evorch-core", "package")?;
    wanted(&manifest.bin, "evorch-core", "bin")?;
    wanted(&manifest.profile, PROFILE, "profile")?;
    if !manifest.features.is_empty() {
        return Err(format!(
            "manifest features are {:?}: the product is built without a feature",
            manifest.features
        ));
    }
    let (distribution_args, build_args): (&[&str], &[&str]) = if linux {
        (&DISTRIBUTION_ARGS, &BUILD_ARGS)
    } else {
        (&CORE_ONLY_DISTRIBUTION_ARGS, &CORE_ONLY_BUILD_ARGS)
    };
    if manifest.distribution_args != distribution_args {
        return Err(format!(
            "manifest distribution_args are {:?}, expected {distribution_args:?}",
            manifest.distribution_args
        ));
    }
    if manifest.build_args != build_args {
        return Err(format!(
            "manifest build_args are {:?}, expected {build_args:?}",
            manifest.build_args
        ));
    }
    if manifest.sha256.len() != 64 || !manifest.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "manifest sha256 {:?} is not a SHA-256",
            manifest.sha256
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn check_sealed(meta: &std::fs::Metadata, executable: bool, what: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    if mode & 0o222 != 0 {
        return Err(format!(
            "{what} is writable (mode {mode:o}): it must be sealed read-only"
        ));
    }
    if executable && mode & 0o100 == 0 {
        return Err(format!("{what} is not executable (mode {mode:o})"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_sealed(_: &std::fs::Metadata, _: bool, _: &str) -> Result<(), String> {
    Ok(())
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        // Writing to a String cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

fn parse_manifest(path: &Path) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read manifest {}: {error}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("manifest {} is not JSON: {error}", path.display()))?;
    let object = value.as_object().ok_or("manifest is not a JSON object")?;
    let text_field = |name: &str| -> Result<String, String> {
        object
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("manifest field `{name}` is missing or not a string"))
    };
    let list_field = |name: &str| -> Result<Vec<String>, String> {
        object
            .get(name)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("manifest field `{name}` is missing or not a list"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("manifest field `{name}` holds a non-string"))
            })
            .collect()
    };
    let number_field = |name: &str| -> Result<u64, String> {
        object
            .get(name)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("manifest field `{name}` is missing or not a number"))
    };
    Ok(Manifest {
        schema_version: number_field("schema_version")?,
        kind: text_field("kind")?,
        package: text_field("package")?,
        bin: text_field("bin")?,
        distribution_args: list_field("distribution_args")?,
        build_args: list_field("build_args")?,
        features: list_field("features")?,
        profile: text_field("profile")?,
        target: text_field("target")?,
        rustc: text_field("rustc")?,
        rustflags: text_field("rustflags")?,
        source_sha: text_field("source_sha")?,
        file: text_field("file")?,
        size_bytes: number_field("size_bytes")?,
        sha256: text_field("sha256")?,
        guard: optional_object("guard", object)?
            .map(|guard| {
                Ok::<_, String>(GuardReceipt {
                    package: nested_text("guard", guard, "package")?,
                    bin: nested_text("guard", guard, "bin")?,
                    file: nested_text("guard", guard, "file")?,
                    size_bytes: nested_number("guard", guard, "size_bytes")?,
                    sha256: nested_text("guard", guard, "sha256")?,
                    profile: nested_text("guard", guard, "profile")?,
                    features: nested_list("guard", guard, "features")?,
                    build_args: nested_list("guard", guard, "build_args")?,
                    guard_abi: nested_number("guard", guard, "guard_abi")?,
                    guard_compat_id: nested_text("guard", guard, "guard_compat_id")?,
                })
            })
            .transpose()?,
        guard_bundle: optional_object("guard_bundle", object)?
            .map(|bundle| {
                Ok::<_, String>(BundleReceipt {
                    file: nested_text("guard_bundle", bundle, "file")?,
                    size_bytes: nested_number("guard_bundle", bundle, "size_bytes")?,
                    sha256: nested_text("guard_bundle", bundle, "sha256")?,
                })
            })
            .transpose()?,
    })
}

type Fields = serde_json::Map<String, Value>;

fn optional_object<'a>(name: &str, object: &'a Fields) -> Result<Option<&'a Fields>, String> {
    match object.get(name) {
        Some(Value::Null) => Ok(None),
        Some(Value::Object(fields)) => Ok(Some(fields)),
        _ => Err(format!(
            "manifest field `{name}` is missing or neither null nor an object"
        )),
    }
}

fn nested_text(parent: &str, fields: &Fields, name: &str) -> Result<String, String> {
    fields
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("manifest field `{parent}.{name}` is missing or not a string"))
}

fn nested_number(parent: &str, fields: &Fields, name: &str) -> Result<u64, String> {
    fields
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("manifest field `{parent}.{name}` is missing or not a number"))
}

fn nested_list(parent: &str, fields: &Fields, name: &str) -> Result<Vec<String>, String> {
    fields
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("manifest field `{parent}.{name}` is missing or not a list"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("manifest field `{parent}.{name}` holds a non-string"))
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const CORE: &[u8] = b"not a real evorch-core, only bytes to hash";
    const SOURCE: &str = "0123456789abcdef0123456789abcdef01234567";

    fn put(path: &Path, bytes: &[u8], mode: u32) {
        let _ = std::fs::remove_file(path);
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn digest(bytes: &[u8]) -> String {
        let mut hex = String::new();
        for byte in Sha256::digest(bytes) {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A sealed product in a temporary directory. `linux` selects the closure
    /// the verifier is asked to accept: four files, or the core and manifest.
    struct Product {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
        linux: bool,
    }

    impl Product {
        fn exe(&self) -> PathBuf {
            self.dir.join("evorch-core")
        }
        fn manifest(&self) -> PathBuf {
            self.dir.join("product-manifest.json")
        }
        fn helper(&self) -> PathBuf {
            self.dir.join(GUARD_FILE)
        }
        fn bundle(&self) -> PathBuf {
            self.dir.join(GUARD_BUNDLE_FILE)
        }
        fn verify(&self) -> Result<ProductArtifact, String> {
            verify_for(&self.exe(), &self.manifest(), self.linux)
        }
        fn reason(&self) -> String {
            self.verify().expect_err("must be refused")
        }
        fn read_json(path: &Path) -> Value {
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
        }
        fn edit_manifest(&self, edit: impl FnOnce(&mut Value)) {
            let mut json = Self::read_json(&self.manifest());
            edit(&mut json);
            put(
                &self.manifest(),
                &serde_json::to_vec_pretty(&json).unwrap(),
                0o444,
            );
        }
    }

    fn manifest_json(linux: bool, guard: &Value, guard_bundle: &Value) -> Value {
        let (distribution, build) = if linux {
            (DISTRIBUTION_ARGS.to_vec(), BUILD_ARGS.to_vec())
        } else {
            (
                CORE_ONLY_DISTRIBUTION_ARGS.to_vec(),
                CORE_ONLY_BUILD_ARGS.to_vec(),
            )
        };
        serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "kind": KIND,
            "package": "evorch-core",
            "bin": "evorch-core",
            "distribution_args": distribution,
            "build_args": build,
            "features": [],
            "profile": PROFILE,
            "target": target_triple(),
            "rustc": "rustc test",
            "rustflags": "",
            "source_sha": SOURCE,
            "file": "evorch-core",
            "size_bytes": CORE.len(),
            "sha256": digest(CORE),
            "guard": guard,
            "guard_bundle": guard_bundle,
        })
    }

    #[cfg(target_os = "linux")]
    fn target_triple() -> &'static str {
        process_guard::TARGET
    }

    #[cfg(not(target_os = "linux"))]
    fn target_triple() -> &'static str {
        "test-target"
    }

    /// The product of a platform without a helper: the core and its manifest.
    fn core_only() -> Product {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let product = Product {
            _tmp: tmp,
            dir,
            linux: false,
        };
        put(&product.exe(), CORE, 0o555);
        put(
            &product.manifest(),
            &serde_json::to_vec_pretty(&manifest_json(false, &Value::Null, &Value::Null)).unwrap(),
            0o444,
        );
        product
    }

    #[test]
    fn a_sealed_copy_that_matches_its_manifest_is_accepted() {
        let product = core_only();
        let artifact = product.verify().unwrap();
        assert_eq!(artifact.manifest.package, "evorch-core");
        assert!(artifact.manifest.features.is_empty());
        assert!(artifact.guard.is_none());
    }

    #[test]
    fn a_changed_executable_is_refused() {
        let product = core_only();
        chmod(&product.exe(), 0o755);
        std::fs::write(product.exe(), [CORE, b"!"].concat()).unwrap();
        assert!(product.reason().contains("bytes"), "{}", product.reason());
        std::fs::write(
            product.exe(),
            CORE.iter().map(|b| b ^ 1).collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(product.reason().contains("sha256"), "{}", product.reason());
    }

    #[test]
    fn a_feature_or_a_different_command_is_refused() {
        let with_feature = core_only();
        with_feature
            .edit_manifest(|json| json["features"] = serde_json::json!(["renderer-test-helper"]));
        assert!(with_feature.reason().contains("features"));
        let other_args = core_only();
        other_args.edit_manifest(|json| {
            json["build_args"] =
                serde_json::json!(["build", "-p", "evorch-core", "--bin", "evorch-core"]);
        });
        assert!(other_args.reason().contains("build_args"));
        let other_distribution = core_only();
        other_distribution.edit_manifest(|json| {
            json["distribution_args"] = serde_json::json!(["-p", "evorch-core"]);
        });
        assert!(other_distribution.reason().contains("distribution_args"));
        let debug = core_only();
        debug.edit_manifest(|json| json["profile"] = serde_json::json!("dev"));
        assert!(debug.reason().contains("profile"));
    }

    #[test]
    fn a_manifest_that_is_not_the_contract_is_refused() {
        let schema_one = core_only();
        schema_one.edit_manifest(|json| json["schema_version"] = serde_json::json!(1));
        assert!(schema_one.reason().contains("schema_version"));
        let schema_three = core_only();
        schema_three.edit_manifest(|json| json["schema_version"] = serde_json::json!(3));
        assert!(schema_three.reason().contains("schema_version"));
        let missing = core_only();
        missing.edit_manifest(|json| {
            json.as_object_mut().unwrap().remove("source_sha");
        });
        assert!(missing.reason().contains("source_sha"));
        let wrong_name = core_only();
        wrong_name.edit_manifest(|json| json["file"] = serde_json::json!("other"));
        assert!(wrong_name.reason().contains("manifest says"));
        let no_guard_field = core_only();
        no_guard_field.edit_manifest(|json| {
            json.as_object_mut().unwrap().remove("guard");
        });
        assert!(no_guard_field.reason().contains("guard"));
        let no_bundle_field = core_only();
        no_bundle_field.edit_manifest(|json| {
            json.as_object_mut().unwrap().remove("guard_bundle");
        });
        assert!(no_bundle_field.reason().contains("guard_bundle"));
    }

    #[test]
    fn a_helper_is_refused_where_no_helper_exists() {
        let product = core_only();
        product.edit_manifest(|json| {
            json["guard"] = serde_json::json!({
                "package": "process-guard", "bin": GUARD_FILE, "file": GUARD_FILE,
                "size_bytes": 1, "sha256": digest(b"x"), "profile": PROFILE,
                "features": [], "build_args": CORE_ONLY_BUILD_ARGS,
                "guard_abi": 1, "guard_compat_id": digest(b"y"),
            });
        });
        assert!(
            product.reason().contains("has none"),
            "{}",
            product.reason()
        );
    }

    #[test]
    fn the_linux_recipe_is_refused_where_no_helper_exists_and_the_core_only_one_on_linux() {
        let product = core_only();
        product.edit_manifest(|json| {
            json["distribution_args"] = serde_json::json!(DISTRIBUTION_ARGS);
            json["build_args"] = serde_json::json!(BUILD_ARGS);
        });
        assert!(product.reason().contains("distribution_args"));
        let core_only_recipe_with_a_helper_platform = Product {
            linux: true,
            ..core_only()
        };
        assert!(
            core_only_recipe_with_a_helper_platform
                .reason()
                .contains("distribution_args"),
            "{}",
            core_only_recipe_with_a_helper_platform.reason()
        );
    }

    #[test]
    fn a_writable_or_non_executable_copy_is_refused() {
        let writable = core_only();
        chmod(&writable.exe(), 0o755);
        assert!(writable.reason().contains("writable"));
        let not_executable = core_only();
        chmod(&not_executable.exe(), 0o444);
        assert!(not_executable.reason().contains("not executable"));
        let manifest_writable = core_only();
        chmod(&manifest_writable.manifest(), 0o644);
        assert!(manifest_writable.reason().contains("manifest is writable"));
    }

    #[test]
    fn a_symlink_a_missing_file_and_a_target_directory_are_refused() {
        let product = core_only();
        let link = product.dir.join("link");
        std::os::unix::fs::symlink(product.exe(), &link).unwrap();
        assert!(
            verify_for(&link, &product.manifest(), false)
                .unwrap_err()
                .contains("not a regular file")
        );
        let gone = product.dir.join("gone");
        assert!(
            verify_for(&gone, &product.manifest(), false)
                .unwrap_err()
                .contains("unreadable")
        );
        let in_target = tempfile::tempdir().unwrap().keep().join("target");
        std::fs::create_dir_all(&in_target).unwrap();
        let exe = in_target.join("evorch-core");
        std::fs::copy(product.exe(), &exe).unwrap();
        let manifest = in_target.join("product-manifest.json");
        std::fs::copy(product.manifest(), &manifest).unwrap();
        assert!(
            verify_for(&exe, &manifest, false)
                .unwrap_err()
                .contains("`target` directory")
        );
        std::fs::remove_dir_all(in_target.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_missing_environment_names_the_remedy_and_never_skips() {
        // The variables are read, not set, here: set_var is unsound under threads.
        if std::env::var_os(EXE_ENV).is_none() || std::env::var_os(MANIFEST_ENV).is_none() {
            let reason = verify_from_env().expect_err("an unset variable must fail");
            assert!(
                reason.contains("scripts/product-artifact.sh build"),
                "{reason}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    mod closure {
        use super::*;

        const HELPER: &[u8] = b"not a real helper, only bytes to hash";

        type Case = (&'static str, fn(&mut Value), &'static str);

        fn helper_receipt() -> Value {
            serde_json::json!({
                "package": "process-guard", "bin": GUARD_FILE, "file": GUARD_FILE,
                "size_bytes": HELPER.len(), "sha256": digest(HELPER), "profile": PROFILE,
                "features": [], "build_args": BUILD_ARGS,
            })
        }

        fn bundle_json() -> Value {
            serde_json::json!({
                "schema_version": 1, "kind": "evorch-guard-bundle", "purpose": "distribution",
                "target": process_guard::TARGET, "source_sha": SOURCE,
                "guard_abi": process_guard::wire::ABI,
                "guard_compat_id": process_guard::COMPAT_ID,
                "helper": helper_receipt(),
                "callers": [{
                    "role": "core", "file": "evorch-core",
                    "size_bytes": CORE.len(), "sha256": digest(CORE),
                }],
            })
        }

        /// The four-file closure that a Linux build seals, in a temporary directory.
        fn linux() -> Product {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().to_path_buf();
            let product = Product {
                _tmp: tmp,
                dir,
                linux: true,
            };
            put(&product.exe(), CORE, 0o555);
            put(&product.helper(), HELPER, 0o555);
            let bundle = serde_json::to_vec_pretty(&bundle_json()).unwrap();
            put(&product.bundle(), &bundle, 0o444);
            let mut guard = helper_receipt();
            guard["guard_abi"] = serde_json::json!(process_guard::wire::ABI);
            guard["guard_compat_id"] = serde_json::json!(process_guard::COMPAT_ID);
            let receipt = serde_json::json!({
                "file": GUARD_BUNDLE_FILE, "size_bytes": bundle.len(), "sha256": digest(&bundle),
            });
            put(
                &product.manifest(),
                &serde_json::to_vec_pretty(&manifest_json(true, &guard, &receipt)).unwrap(),
                0o444,
            );
            product
        }

        /// Rewrites the bundle and keeps the manifest's receipt of the file
        /// current, so only the content cross-check can refuse the result.
        fn rewrite_bundle(product: &Product, edit: impl FnOnce(&mut Value)) {
            let mut json = Product::read_json(&product.bundle());
            edit(&mut json);
            let bytes = serde_json::to_vec_pretty(&json).unwrap();
            put(&product.bundle(), &bytes, 0o444);
            product.edit_manifest(|manifest| {
                manifest["guard_bundle"]["size_bytes"] = serde_json::json!(bytes.len());
                manifest["guard_bundle"]["sha256"] = serde_json::json!(digest(&bytes));
            });
        }

        fn flip_first_byte(path: &Path) {
            let mut bytes = std::fs::read(path).unwrap();
            bytes[0] ^= 1;
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            put(path, &bytes, mode & 0o777);
        }

        #[test]
        fn the_four_file_closure_is_accepted_and_its_files_are_named() {
            let product = linux();
            let artifact = product.verify().unwrap();
            let guard = artifact.guard.expect("Linux carries the helper");
            assert_eq!(guard.helper, product.helper());
            assert_eq!(guard.bundle, product.bundle());
            let receipt = artifact.manifest.guard.expect("guard receipt");
            assert_eq!(receipt.guard_compat_id, process_guard::COMPAT_ID);
            assert_eq!(receipt.build_args, artifact.manifest.build_args);
        }

        #[test]
        fn a_one_byte_change_of_the_core_or_the_helper_or_the_bundle_is_refused() {
            let core = linux();
            flip_first_byte(&core.exe());
            assert!(
                core.reason().contains("executable sha256"),
                "{}",
                core.reason()
            );
            let helper = linux();
            flip_first_byte(&helper.helper());
            assert!(
                helper.reason().contains("guard helper sha256"),
                "{}",
                helper.reason()
            );
            let bundle = linux();
            flip_first_byte(&bundle.bundle());
            assert!(
                bundle.reason().contains("guard bundle sha256"),
                "{}",
                bundle.reason()
            );
        }

        #[test]
        fn a_helper_or_bundle_of_another_size_is_refused() {
            let helper = linux();
            put(&helper.helper(), &[HELPER, b"!"].concat(), 0o555);
            assert!(
                helper.reason().contains("guard helper has"),
                "{}",
                helper.reason()
            );
            let bundle = linux();
            let mut bytes = std::fs::read(bundle.bundle()).unwrap();
            bytes.push(b'\n');
            put(&bundle.bundle(), &bytes, 0o444);
            assert!(
                bundle.reason().contains("guard bundle has"),
                "{}",
                bundle.reason()
            );
        }

        #[test]
        fn the_core_alone_is_never_a_product_on_linux() {
            let alone = linux();
            std::fs::remove_file(alone.helper()).unwrap();
            assert!(
                alone.reason().contains("guard helper"),
                "{}",
                alone.reason()
            );
            let no_bundle = linux();
            std::fs::remove_file(no_bundle.bundle()).unwrap();
            assert!(
                no_bundle.reason().contains("guard bundle"),
                "{}",
                no_bundle.reason()
            );
            let nulled = linux();
            nulled.edit_manifest(|json| {
                json["guard"] = Value::Null;
                json["guard_bundle"] = Value::Null;
                json["distribution_args"] = serde_json::json!(CORE_ONLY_DISTRIBUTION_ARGS);
                json["build_args"] = serde_json::json!(CORE_ONLY_BUILD_ARGS);
            });
            assert!(nulled.reason().contains("distribution_args"));
            let nulled_recipe_kept = linux();
            nulled_recipe_kept.edit_manifest(|json| json["guard"] = Value::Null);
            assert!(
                nulled_recipe_kept.reason().contains("guard is null"),
                "{}",
                nulled_recipe_kept.reason()
            );
            let no_bundle_receipt = linux();
            no_bundle_receipt.edit_manifest(|json| json["guard_bundle"] = Value::Null);
            assert!(
                no_bundle_receipt.reason().contains("guard_bundle is null"),
                "{}",
                no_bundle_receipt.reason()
            );
        }

        #[test]
        fn schema_one_is_refused_on_linux() {
            let product = linux();
            product.edit_manifest(|json| json["schema_version"] = serde_json::json!(1));
            assert!(product.reason().contains("schema_version"));
        }

        #[test]
        fn a_writable_or_non_executable_or_symlinked_helper_or_bundle_is_refused() {
            let helper_writable = linux();
            chmod(&helper_writable.helper(), 0o755);
            assert!(
                helper_writable
                    .reason()
                    .contains("guard helper is writable"),
                "{}",
                helper_writable.reason()
            );
            let helper_not_executable = linux();
            chmod(&helper_not_executable.helper(), 0o444);
            assert!(
                helper_not_executable
                    .reason()
                    .contains("guard helper is not executable"),
                "{}",
                helper_not_executable.reason()
            );
            let bundle_writable = linux();
            chmod(&bundle_writable.bundle(), 0o644);
            assert!(
                bundle_writable
                    .reason()
                    .contains("guard bundle is writable"),
                "{}",
                bundle_writable.reason()
            );
            let helper_link = linux();
            let real = helper_link.dir.join("real-helper");
            std::fs::rename(helper_link.helper(), &real).unwrap();
            std::os::unix::fs::symlink(&real, helper_link.helper()).unwrap();
            assert!(
                helper_link.reason().contains("not a regular file"),
                "{}",
                helper_link.reason()
            );
            let bundle_link = linux();
            let real = bundle_link.dir.join("real-bundle");
            std::fs::rename(bundle_link.bundle(), &real).unwrap();
            std::os::unix::fs::symlink(&real, bundle_link.bundle()).unwrap();
            assert!(
                bundle_link.reason().contains("not a regular file"),
                "{}",
                bundle_link.reason()
            );
        }

        #[test]
        fn a_helper_receipt_that_is_not_the_contract_is_refused() {
            let cases: [Case; 6] = [
                (
                    "package",
                    |g| g["package"] = serde_json::json!("other"),
                    "process-guard",
                ),
                (
                    "profile",
                    |g| g["profile"] = serde_json::json!("dev"),
                    "guard profile",
                ),
                (
                    "features",
                    |g| g["features"] = serde_json::json!(["x"]),
                    "guard features",
                ),
                (
                    "standalone recipe",
                    |g| {
                        g["build_args"] = serde_json::json!([
                            "build",
                            "--locked",
                            "--release",
                            "-p",
                            "process-guard",
                            "--bin",
                            "evorch-process-guard"
                        ]);
                    },
                    "guard build_args",
                ),
                (
                    "file",
                    |g| g["file"] = serde_json::json!("other-helper"),
                    "expected",
                ),
                (
                    "compat id shape",
                    |g| g["guard_compat_id"] = serde_json::json!("ff"),
                    "not a SHA-256",
                ),
            ];
            for (name, edit, expected) in cases {
                let product = linux();
                product.edit_manifest(|json| edit(&mut json["guard"]));
                let reason = product.reason();
                assert!(reason.contains(expected), "{name}: {reason}");
            }
        }

        #[test]
        fn a_bundle_that_disagrees_with_the_manifest_is_refused() {
            let bundle_cases: [Case; 7] = [
                (
                    "purpose",
                    |b| b["purpose"] = serde_json::json!("development"),
                    "guard build recipe mismatch",
                ),
                // A development bundle with its own, valid development
                // recipe passes the image checks, so only the comparison
                // with the manifest can refuse it.
                (
                    "purpose with a recipe of its own",
                    |b| {
                        b["purpose"] = serde_json::json!("development");
                        b["helper"]["build_args"] = serde_json::json!([
                            "build",
                            "--locked",
                            "--release",
                            "-p",
                            "process-guard",
                            "--bin",
                            "evorch-process-guard"
                        ]);
                    },
                    "disagrees with the manifest on purpose",
                ),
                (
                    "source",
                    |b| b["source_sha"] = serde_json::json!("1".repeat(40)),
                    "source_sha",
                ),
                (
                    "helper recipe",
                    |b| {
                        b["helper"]["build_args"] = serde_json::json!([
                            "build",
                            "--locked",
                            "--release",
                            "-p",
                            "process-guard",
                            "--bin",
                            "evorch-process-guard"
                        ]);
                    },
                    "guard build recipe mismatch",
                ),
                (
                    "caller hash",
                    |b| b["callers"][0]["sha256"] = serde_json::json!(digest(b"another core")),
                    "callers",
                ),
                (
                    "caller role",
                    |b| b["callers"][0]["role"] = serde_json::json!("cli"),
                    "guard build recipe mismatch",
                ),
                (
                    "second caller",
                    |b| {
                        let caller = b["callers"][0].clone();
                        b["callers"].as_array_mut().unwrap().push(caller);
                    },
                    "guard build recipe mismatch",
                ),
            ];
            for (name, edit, expected) in bundle_cases {
                let product = linux();
                rewrite_bundle(&product, edit);
                let reason = product.reason();
                assert!(reason.contains(expected), "{name}: {reason}");
            }
            let manifest_cases: [Case; 3] = [
                (
                    "abi",
                    |m| m["guard"]["guard_abi"] = serde_json::json!(2),
                    "guard_abi",
                ),
                (
                    "compat id",
                    |m| m["guard"]["guard_compat_id"] = serde_json::json!(digest(b"other")),
                    "guard_compat_id",
                ),
                (
                    "target",
                    |m| m["target"] = serde_json::json!("other-target"),
                    "target",
                ),
            ];
            for (name, edit, expected) in manifest_cases {
                let product = linux();
                product.edit_manifest(edit);
                let reason = product.reason();
                assert!(reason.contains(expected), "{name}: {reason}");
            }
        }

        #[test]
        fn a_bundle_of_another_helper_build_is_refused_by_the_image() {
            let compat = linux();
            rewrite_bundle(&compat, |b| {
                b["guard_compat_id"] = serde_json::json!(digest(b"another helper source"));
            });
            compat.edit_manifest(|m| {
                m["guard"]["guard_compat_id"] = serde_json::json!(digest(b"another helper source"));
            });
            assert!(
                compat.reason().contains("is refused"),
                "{}",
                compat.reason()
            );
            let abi = linux();
            rewrite_bundle(&abi, |b| b["guard_abi"] = serde_json::json!(2));
            abi.edit_manifest(|m| m["guard"]["guard_abi"] = serde_json::json!(2));
            assert!(abi.reason().contains("is refused"), "{}", abi.reason());
        }

        #[test]
        fn the_closure_is_copied_whole_and_the_core_alone_is_not_a_copy() {
            let product = linux();
            let artifact = product.verify().unwrap();
            let dest = tempfile::tempdir().unwrap();
            let copy = artifact.copy_closure_into(dest.path()).unwrap();
            let guard = copy.guard.expect("the copy carries the helper");
            assert_eq!(guard.helper, dest.path().join(GUARD_FILE));
            assert_eq!(guard.bundle, dest.path().join(GUARD_BUNDLE_FILE));
            assert_eq!(std::fs::read(&guard.helper).unwrap(), HELPER);

            let core_only_dir = tempfile::tempdir().unwrap();
            std::fs::copy(product.exe(), core_only_dir.path().join("evorch-core")).unwrap();
            std::fs::copy(
                product.manifest(),
                core_only_dir.path().join("product-manifest.json"),
            )
            .unwrap();
            seal_copy(&core_only_dir.path().join("evorch-core"), true).unwrap();
            seal_copy(&core_only_dir.path().join("product-manifest.json"), false).unwrap();
            let reason = verify(
                &core_only_dir.path().join("evorch-core"),
                &core_only_dir.path().join("product-manifest.json"),
            )
            .expect_err("a core copied alone must be refused");
            assert!(reason.contains("guard helper"), "{reason}");

            let in_target = tempfile::tempdir().unwrap().keep().join("target");
            std::fs::create_dir_all(&in_target).unwrap();
            let reason = artifact
                .copy_closure_into(&in_target)
                .expect_err("a copy into a target directory must be refused");
            assert!(reason.contains("`target` directory"), "{reason}");
            std::fs::remove_dir_all(in_target.parent().unwrap()).unwrap();
        }

        #[test]
        fn a_corrupted_copy_of_the_closure_is_refused() {
            let product = linux();
            let artifact = product.verify().unwrap();
            let dest = tempfile::tempdir().unwrap();
            let copy = artifact.copy_closure_into(dest.path()).unwrap();
            let guard = copy.guard.expect("the copy carries the helper");
            flip_first_byte(&guard.helper);
            let reason = verify(&copy.exe, &copy.manifest_path).expect_err("corrupt helper");
            assert!(reason.contains("guard helper sha256"), "{reason}");
        }
    }
}
