//! GUI history also inserts user input from sidebar.json, outside the event DB.

use std::{io::Read, path::Path, time::SystemTime};

#[derive(serde::Deserialize)]
struct History {
    #[serde(default)]
    user_messages: Vec<UserMessage>,
}

#[derive(serde::Deserialize)]
struct UserMessage {
    at: SystemTime,
}

pub(super) fn boundaries(path: &Path) -> Result<Vec<SystemTime>, &'static str> {
    // No thread bindings or conversation bodies need to be retained. All history
    // timestamps are conservative boundaries, including legacy status-less input.
    let mut data = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| "cannot open sidebar")?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut data)
        .map_err(|_| "cannot read sidebar")?;
    if data.len() > 64 * 1024 * 1024 {
        return Err("sidebar exceeds 64 MiB read limit");
    }
    if data.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err("sidebar is not a JSON object");
    }
    let saved: History =
        serde_json::from_slice(&data).map_err(|_| "invalid sidebar time envelope")?;
    let mut times: Vec<_> = saved
        .user_messages
        .into_iter()
        .map(|message| message.at)
        .collect();
    times.sort_unstable();
    times.dedup();
    Ok(times)
}
