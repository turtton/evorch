//! Preflight, describe and exec always use this same copy. The debug product
//! ELF is ~565 MB. Strip debug and unneeded static symbols, asserting executable
//! text/data remain byte-identical, to fit the mandatory 60-second watchdog.
//! Cargo's original/shared artifacts are NEVER rewritten.
use std::{
    path::{Path, PathBuf},
    process::Command,
};
const SECTIONS: [&str; 4] = [".text", ".data", ".rodata", ".data.rel.ro"];
fn dump_sections(binary: &Path, label: &str) {
    let mut command = Command::new("objcopy");
    for section in SECTIONS {
        command.arg("--dump-section").arg(format!(
            "{section}={}",
            binary.with_extension(format!("{label}{section}")).display()
        ));
    }
    // objcopy rewrites only our private copy, never the Cargo source artifact.
    assert!(command.arg(binary).status().unwrap().success());
}
pub fn stripped_copy(source: &Path, destination: &Path) -> PathBuf {
    std::fs::copy(source, destination).unwrap();
    dump_sections(destination, "before");
    assert!(
        Command::new("objcopy")
            .args(["--strip-debug", "--strip-unneeded"])
            .arg(destination)
            .status()
            .unwrap()
            .success()
    );
    dump_sections(destination, "after");
    for section in SECTIONS {
        let before = destination.with_extension(format!("before{section}"));
        let after = destination.with_extension(format!("after{section}"));
        assert!(
            std::fs::read(&before).unwrap() == std::fs::read(&after).unwrap(),
            "stripping changed {section}"
        );
        std::fs::remove_file(before).unwrap();
        std::fs::remove_file(after).unwrap();
    }
    eprintln!(
        "D3B ELF copy: {} -> {} bytes",
        std::fs::metadata(source).unwrap().len(),
        std::fs::metadata(destination).unwrap().len()
    );
    destination.to_path_buf()
}
pub fn product_copy(directory: &Path) -> PathBuf {
    let product = stripped_copy(
        Path::new(&std::env::var("CARGO_BIN_EXE_evorch-core").unwrap()),
        &directory.join("product-core"),
    );
    test_support::guard_bundle::bind_copy(&product, "core").unwrap();
    product
}
