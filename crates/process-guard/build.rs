use sha2::{Digest, Sha256};
fn main() {
    let version = std::process::Command::new(std::env::var_os("RUSTC").expect("Cargo rustc"))
        .arg("-vV")
        .output()
        .expect("rustc provenance");
    assert!(version.status.success(), "rustc provenance");
    let compiler = String::from_utf8(version.stdout)
        .expect("rustc version UTF-8")
        .trim()
        .replace('\n', " | ");
    println!("cargo:rustc-env=GUARD_COMPILER={compiler}");
    let mut hash = Sha256::new();
    // No workspace files, lockfile, git state, profile, or feature-dependent inputs.
    for file in [
        "Cargo.toml",
        "build.rs",
        "src/lib.rs",
        "src/main.rs",
        "src/image.rs",
        "src/wire.rs",
        "src/guard.rs",
        "src/helper.rs",
        "src/pty.rs",
    ] {
        println!("cargo:rerun-if-changed={file}");
        let bytes = std::fs::read(file).expect("package source");
        hash.update((file.len() as u64).to_le_bytes());
        hash.update(file);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    let target = std::env::var("TARGET").expect("Cargo target");
    hash.update(target.as_bytes());
    println!("cargo:rustc-env=GUARD_TARGET={target}");
    println!("cargo:rustc-env=GUARD_COMPAT_ID={:x}", hash.finalize());
}
