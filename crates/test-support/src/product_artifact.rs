//! The sealed product artifact: the `evorch-core` binary as it is distributed,
//! built apart from every test build, with a manifest that describes it.
//!
//! CI builds it once in the `product-artifact` job
//! (`cargo build --locked --release -p evorch-core --bin evorch-core`, no
//! feature), seals it with `scripts/product-artifact.sh`, and hands the
//! immutable copy and its manifest to every job that runs a product test (the
//! offline test shards that include the cache gate, `browser-e2e` and
//! `cef-e2e`). Test builds never write to that copy, and a test never falls
//! back to a binary that a test build produced (`CARGO_BIN_EXE_*`).
//!
//! The contract is fixed here by the integration PR that registered it:
//!
//! - [`EXE_ENV`] names the sealed executable and [`MANIFEST_ENV`] its manifest.
//! - The manifest is a JSON object with the fields [`verify`] checks: see
//!   [`Manifest`].
//!
//! A test that needs the product calls [`require_from_env`]. A missing
//! variable, file, manifest, hash, or feature fails it: there is no skip.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Environment variable naming the sealed `evorch-core` executable.
pub const EXE_ENV: &str = "EVORCH_PRODUCT_CORE_EXE";
/// Environment variable naming the manifest of the sealed executable.
pub const MANIFEST_ENV: &str = "EVORCH_PRODUCT_MANIFEST";
/// The only manifest schema version this reader understands.
pub const SCHEMA_VERSION: u64 = 1;
/// The `kind` field of every product manifest.
pub const KIND: &str = "evorch-core-product";
/// The package and binary selection of the distribution (`flake.nix`
/// `evorch-core`: `-p evorch-core --bin evorch-core`), without a feature.
pub const DISTRIBUTION_ARGS: [&str; 4] = ["-p", "evorch-core", "--bin", "evorch-core"];
/// The complete Cargo command the product artifact is built with.
pub const BUILD_ARGS: [&str; 7] = [
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

/// The manifest fields a test may rely on after [`verify`] succeeded.
///
/// Every field is required in the JSON. `features` must be empty and
/// `build_args` must equal [`BUILD_ARGS`]; `file`, `size_bytes` and `sha256`
/// describe the sealed executable; `target`, `rustc`, `rustflags` and
/// `source_sha` record how it was built.
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
}

/// A verified product artifact.
#[derive(Debug, Clone)]
pub struct ProductArtifact {
    pub exe: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
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
    check_contract(&manifest)?;
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
    Ok(ProductArtifact {
        exe: exe.to_path_buf(),
        manifest_path: manifest_path.to_path_buf(),
        manifest,
    })
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

fn check_contract(manifest: &Manifest) -> Result<(), String> {
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
    if manifest.distribution_args != DISTRIBUTION_ARGS {
        return Err(format!(
            "manifest distribution_args are {:?}, expected {DISTRIBUTION_ARGS:?}",
            manifest.distribution_args
        ));
    }
    if manifest.build_args != BUILD_ARGS {
        return Err(format!(
            "manifest build_args are {:?}, expected {BUILD_ARGS:?}",
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
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const BODY: &[u8] = b"not a real evorch-core, only bytes to hash";

    struct Sealed {
        _dir: tempfile::TempDir,
        exe: PathBuf,
        manifest: PathBuf,
    }

    fn manifest_json(sha256: &str, size: u64) -> Value {
        serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "kind": KIND,
            "package": "evorch-core",
            "bin": "evorch-core",
            "distribution_args": DISTRIBUTION_ARGS,
            "build_args": BUILD_ARGS,
            "features": [],
            "profile": PROFILE,
            "target": "test-target",
            "rustc": "rustc test",
            "rustflags": "",
            "source_sha": "0000000000000000000000000000000000000000",
            "file": "evorch-core",
            "size_bytes": size,
            "sha256": sha256,
        })
    }

    fn seal_with(edit: impl FnOnce(&mut Value)) -> Sealed {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("evorch-core");
        std::fs::write(&exe, BODY).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o555)).unwrap();
        let mut json = manifest_json(&sha256_hex(&exe).unwrap(), BODY.len() as u64);
        edit(&mut json);
        let manifest = dir.path().join("product-manifest.json");
        std::fs::write(&manifest, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
        std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o444)).unwrap();
        Sealed {
            _dir: dir,
            exe,
            manifest,
        }
    }

    fn reason(sealed: &Sealed) -> String {
        verify(&sealed.exe, &sealed.manifest).expect_err("must be refused")
    }

    #[test]
    fn a_sealed_copy_that_matches_its_manifest_is_accepted() {
        let sealed = seal_with(|_| {});
        let artifact = verify(&sealed.exe, &sealed.manifest).unwrap();
        assert_eq!(artifact.manifest.package, "evorch-core");
        assert!(artifact.manifest.features.is_empty());
    }

    #[test]
    fn a_changed_executable_is_refused() {
        let sealed = seal_with(|_| {});
        std::fs::set_permissions(&sealed.exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&sealed.exe, [BODY, b"!"].concat()).unwrap();
        assert!(reason(&sealed).contains("bytes"), "{}", reason(&sealed));
        std::fs::write(&sealed.exe, BODY.iter().map(|b| b ^ 1).collect::<Vec<_>>()).unwrap();
        assert!(reason(&sealed).contains("sha256"), "{}", reason(&sealed));
    }

    #[test]
    fn a_feature_or_a_different_command_is_refused() {
        let with_feature =
            seal_with(|json| json["features"] = serde_json::json!(["renderer-test-helper"]));
        assert!(reason(&with_feature).contains("features"));
        let other_args = seal_with(|json| {
            json["build_args"] =
                serde_json::json!(["build", "-p", "evorch-core", "--bin", "evorch-core"]);
        });
        assert!(reason(&other_args).contains("build_args"));
        let other_distribution = seal_with(|json| {
            json["distribution_args"] = serde_json::json!(["-p", "evorch-core"]);
        });
        assert!(reason(&other_distribution).contains("distribution_args"));
        let debug = seal_with(|json| json["profile"] = serde_json::json!("dev"));
        assert!(reason(&debug).contains("profile"));
    }

    #[test]
    fn a_manifest_that_is_not_the_contract_is_refused() {
        let wrong_schema = seal_with(|json| json["schema_version"] = serde_json::json!(2));
        assert!(reason(&wrong_schema).contains("schema_version"));
        let missing = seal_with(|json| {
            json.as_object_mut().unwrap().remove("source_sha");
        });
        assert!(reason(&missing).contains("source_sha"));
        let wrong_name = seal_with(|json| json["file"] = serde_json::json!("other"));
        assert!(reason(&wrong_name).contains("manifest says"));
    }

    #[test]
    fn a_writable_or_non_executable_copy_is_refused() {
        let writable = seal_with(|_| {});
        std::fs::set_permissions(&writable.exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(reason(&writable).contains("writable"));
        let not_executable = seal_with(|_| {});
        std::fs::set_permissions(&not_executable.exe, std::fs::Permissions::from_mode(0o444))
            .unwrap();
        assert!(reason(&not_executable).contains("not executable"));
        let manifest_writable = seal_with(|_| {});
        std::fs::set_permissions(
            &manifest_writable.manifest,
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(reason(&manifest_writable).contains("manifest is writable"));
    }

    #[test]
    fn a_symlink_a_missing_file_and_a_target_directory_are_refused() {
        let sealed = seal_with(|_| {});
        let link = sealed.exe.with_file_name("link");
        std::os::unix::fs::symlink(&sealed.exe, &link).unwrap();
        assert!(
            verify(&link, &sealed.manifest)
                .unwrap_err()
                .contains("not a regular file")
        );
        let gone = sealed.exe.with_file_name("gone");
        assert!(
            verify(&gone, &sealed.manifest)
                .unwrap_err()
                .contains("unreadable")
        );
        let in_target = tempfile::tempdir()
            .unwrap()
            .keep()
            .join("target")
            .join("release");
        std::fs::create_dir_all(&in_target).unwrap();
        let exe = in_target.join("evorch-core");
        std::fs::copy(&sealed.exe, &exe).unwrap();
        let manifest = in_target.join("product-manifest.json");
        std::fs::copy(&sealed.manifest, &manifest).unwrap();
        assert!(
            verify(&exe, &manifest)
                .unwrap_err()
                .contains("`target` directory")
        );
        std::fs::remove_dir_all(in_target.parent().unwrap().parent().unwrap()).unwrap();
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
}
