use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use super::browser::BrowserAuthError;

pub struct CallbackServer {
    listener: TcpListener,
    port: u16,
}

impl CallbackServer {
    pub fn bind_ports(ports: &[u16]) -> Result<Self, BrowserAuthError> {
        for port in ports {
            match Self::bind_addr(SocketAddr::from(([127, 0, 0, 1], *port))) {
                Ok(server) => return Ok(server),
                Err(BrowserAuthError::Io(error)) if error.kind() == io::ErrorKind::AddrInUse => {},
                Err(error) => return Err(error),
            }
        }
        Err(BrowserAuthError::CallbackPortBusy)
    }

    pub fn bind_addr(addr: SocketAddr) -> Result<Self, BrowserAuthError> {
        if addr.ip() != std::net::Ipv4Addr::LOCALHOST {
            return Err(BrowserAuthError::Rejected);
        }
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}/auth/callback", self.port)
    }

    pub fn wait_for_code(
        &self,
        expected_state: &str,
        timeout: Duration,
    ) -> Result<String, BrowserAuthError> {
        let started = Instant::now();
        loop {
            let remaining = timeout.checked_sub(started.elapsed()).filter(|d| !d.is_zero())
                .ok_or(BrowserAuthError::Timeout)?;
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    let result = read_callback(&mut stream, expected_state, remaining);
                    let status = match &result {
                        Ok(_) => "200 OK",
                        Err(_) => "400 Bad Request",
                    };
                    stream.set_write_timeout(Some(remaining.min(Duration::from_secs(1))))?;
                    let response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n");
                    stream.write_all(response.as_bytes())?;
                    return result;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn read_callback(
    stream: &mut TcpStream,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, BrowserAuthError> {
    let started = Instant::now();
    let mut headers = Vec::with_capacity(1024);
    let mut byte = [0];
    while !headers.ends_with(b"\r\n\r\n") {
        if headers.len() >= 8192 {
            return Err(BrowserAuthError::Rejected);
        }
        let remaining = timeout.checked_sub(started.elapsed()).filter(|d| !d.is_zero())
            .ok_or(BrowserAuthError::Timeout)?;
        stream.set_read_timeout(Some(remaining))?;
        stream.read_exact(&mut byte).map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => BrowserAuthError::Timeout,
            _ => BrowserAuthError::Io(error),
        })?;
        headers.push(byte[0]);
    }
    let text = std::str::from_utf8(&headers).map_err(|_| BrowserAuthError::Rejected)?;
    let mut line = text.lines().next().ok_or(BrowserAuthError::Rejected)?.split_whitespace();
    let method = line.next();
    let target = line.next().ok_or(BrowserAuthError::Rejected)?;
    if method != Some("GET") || line.next() != Some("HTTP/1.1") || line.next().is_some()
        || !target.starts_with("/auth/callback?") {
        return Err(BrowserAuthError::Rejected);
    }
    let url = reqwest::Url::parse(&format!("http://localhost{target}"))
        .map_err(|_| BrowserAuthError::Rejected)?;
    let mut state = None;
    let mut code = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" if state.is_none() => state = Some(value.into_owned()),
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" | "code" | "error" => return Err(BrowserAuthError::Rejected),
            _ => {},
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Err(BrowserAuthError::Rejected);
    }
    code.filter(|code| !code.is_empty()).ok_or(BrowserAuthError::Rejected)
}
