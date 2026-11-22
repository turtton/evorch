//! Persist independent observations when CI requests an evidence artifact.
use std::{io, path::Path};
pub fn emit(cell: &str, kind: &str, value: &serde_json::Value) -> io::Result<()> {
    println!("{value}");
    if let Some(dir) = std::env::var_os("EVORCH_B3_EVIDENCE_DIR") {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            Path::new(&dir).join(format!("b3-08-{cell}-{}-{kind}.json", std::process::id())),
            serde_json::to_vec_pretty(value)?,
        )?;
    }
    Ok(())
}
