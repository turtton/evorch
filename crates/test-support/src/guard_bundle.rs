//! Test-only binding of a private, already-built ELF after copy/strip.
//! Never builds, bypasses installed policy, or modifies a sealed product closure.
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
fn seal(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    fs::write(temp.path(), bytes)?;
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(mode))?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}
/// Rebind only the private ELF passed by a test, using the prepared helper.
/// Existing sealed artifacts must instead copy their entire verified closure.
pub fn bind_copy(exe: &Path, role: &str) -> io::Result<PathBuf> {
    let source = std::env::var_os("EVORCH_PROCESS_GUARD_BUNDLE")
        .ok_or_else(|| io::Error::other("prepare a guard bundle before tests"))?;
    let source = PathBuf::from(source);
    process_guard::GuardRuntime::from_bundle(&source)?;
    let mut bundle: serde_json::Value = serde_json::from_slice(&fs::read(&source)?)?;
    let helper = fs::read(
        source
            .parent()
            .unwrap()
            .join(bundle["helper"]["file"].as_str().unwrap()),
    )?;
    let relative = format!(
        ".evorch-guard/{}/evorch-process-guard",
        bundle["helper"]["sha256"].as_str().unwrap()
    );
    let target = exe.parent().unwrap().join(&relative);
    fs::create_dir_all(target.parent().unwrap())?;
    seal(&target, &helper, 0o555)?;
    let mut file = fs::File::open(exe)?;
    let mut hash = Sha256::new();
    let mut block = [0; 65536];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        hash.update(&block[..n]);
    }
    bundle["purpose"] = "development".into();
    bundle["helper"]["file"] = relative.into();
    let mut args = vec!["build", "--locked"];
    if bundle["helper"]["profile"] == "release" {
        args.push("--release");
    }
    args.extend(["-p", "process-guard", "--bin", "evorch-process-guard"]);
    bundle["helper"]["build_args"] = serde_json::to_value(args)?;
    bundle["callers"] = serde_json::json!([{"role":role,"file":exe.file_name().unwrap().to_str().unwrap(),"size_bytes":file.metadata()?.len(),"sha256":format!("{:x}",hash.finalize())}]);
    let manifest = exe.parent().unwrap().join("guard-bundle.json");
    seal(&manifest, &serde_json::to_vec(&bundle)?, 0o444)?;
    process_guard::GuardRuntime::from_bundle(&manifest)?;
    Ok(manifest)
}
