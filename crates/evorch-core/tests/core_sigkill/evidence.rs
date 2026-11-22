//! Persist independent observations when CI requests an evidence artifact.
use std::{io, path::Path};
pub fn emit(cell: &str, kind: &str, value: &serde_json::Value) -> io::Result<()> {
    println!("{value}");
    if let Some(dir) = std::env::var_os("EVORCH_B3_EVIDENCE_DIR") {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            Path::new(&dir).join(format!("{cell}-{}-{kind}.json", std::process::id())),
            serde_json::to_vec_pretty(value)?,
        )?;
    }
    Ok(())
}

pub fn digest(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let len = file.read(&mut bytes)?;
        if len == 0 {
            break;
        }
        hash.update(&bytes[..len]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
