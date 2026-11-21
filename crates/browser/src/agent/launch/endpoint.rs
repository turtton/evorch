use std::io;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    time::Instant,
};

pub(super) const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const TOTAL: usize = 256 * 1024;
const LINE: usize = 16 * 1024;
const PREFIX: &str = "DevTools listening on ";

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

pub(super) async fn read(reader: impl AsyncRead + Unpin, deadline: Instant) -> io::Result<String> {
    tokio::time::timeout_at(deadline, parse(reader))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Chromium endpoint exceeded absolute 20s deadline",
            )
        })?
}

async fn parse(reader: impl AsyncRead + Unpin) -> io::Result<String> {
    let mut reader = BufReader::new(reader);
    let mut total = 0;
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Chromium stderr ended before endpoint",
            ));
        }
        let used = bytes
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        total += used;
        if total > TOTAL {
            return Err(invalid("Chromium stderr exceeds 256 KiB"));
        }
        if line.len() + used > LINE {
            return Err(invalid("Chromium stderr line exceeds 16 KiB"));
        }
        line.extend_from_slice(&bytes[..used]);
        reader.consume(used);
        if line.last() != Some(&b'\n') {
            continue;
        }
        let text =
            std::str::from_utf8(&line).map_err(|_| invalid("Chromium stderr is not UTF-8"))?;
        if let Some(endpoint) = text.trim_end_matches(['\r', '\n']).strip_prefix(PREFIX) {
            validate(endpoint)?;
            return Ok(endpoint.to_owned());
        }
        line.clear();
    }
}

fn validate(endpoint: &str) -> io::Result<()> {
    let url = url::Url::parse(endpoint).map_err(|_| invalid("invalid Chromium endpoint"))?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if url.scheme() != "ws"
        || !loopback
        || url.port().is_none_or(|p| p == 0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url
            .path()
            .strip_prefix("/devtools/browser/")
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
        || endpoint.chars().any(char::is_whitespace)
    {
        return Err(invalid(
            "Chromium endpoint must be a loopback browser WebSocket",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "endpoint_tests.rs"]
mod tests;
