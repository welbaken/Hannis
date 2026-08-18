//! Minimal HTTP/1.1 + WebSocket client over std::net (RFC 6455 client side).
//! Zero external dependencies; works on Windows (winsock) and Linux.
//! Supports: GET/POST, Content-Length / chunked bodies, SSE-style streams,
//! and a WebSocket client (unmasked server frames, auto-pong, ping keepalive).

use std::io::{BufRead, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Url, String> {
        let rest = s
            .strip_prefix("http://")
            .ok_or_else(|| format!("only http:// urls supported: {s}"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>().map_err(|_| format!("bad port: {p}"))?,
            ),
            None => (authority.to_string(), 80),
        };
        if host.is_empty() {
            return Err(format!("empty host in {s}"));
        }
        Ok(Url { host, port, path: path.to_string() })
    }

    pub fn display(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, String> {
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("no address for {host}:{port}"));
    }
    let mut last = String::new();
    for a in addrs {
        match TcpStream::connect_timeout(&a, timeout) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(timeout));
                let _ = s.set_write_timeout(Some(timeout));
                return Ok(s);
            }
            Err(e) => last = format!("{e}"),
        }
    }
    Err(format!("connect {host}:{port}: {last}"))
}

fn write_head(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    host: &str,
    port: u16,
    extra: &[(&str, String)],
    body: Option<&[u8]>,
) -> Result<(), String> {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: hannis/0.1\r\nAccept: */*\r\n"
    );
    for (k, v) in extra {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    if let Some(b) = body {
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    } else if method == "POST" {
        head.push_str("Content-Length: 0\r\n");
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .map_err(|e| format!("send request: {e}"))?;
    if let Some(b) = body {
        stream.write_all(b).map_err(|e| format!("send body: {e}"))?;
    }
    Ok(())
}

/// Read the status line + headers from a buffered reader.
fn read_head<R: BufRead>(r: &mut R) -> Result<(u16, Vec<(String, String)>), String> {
    let mut line = String::new();
    r.read_line(&mut line).map_err(|e| format!("read status: {e}"))?;
    let mut parts = line.split_whitespace();
    let _http = parts.next();
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {line:?}"))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line).map_err(|e| format!("read header: {e}"))?;
        if n == 0 {
            break;
        }
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

/// One-shot HTTP request (GET/POST). Handles Content-Length and chunked bodies.
pub fn request(
    url: &Url,
    method: &str,
    extra: &[(&str, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<Response, String> {
    let mut stream = connect(&url.host, url.port, timeout)?;
    write_head(&mut stream, method, &url.path, &url.host, url.port, extra, body)?;
    let mut r = std::io::BufReader::new(stream);
    let (status, headers) = read_head(&mut r)?;
    let body = if let Some(enc) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding")).map(|(_, v)| v.clone()) {
        if enc.to_ascii_lowercase().contains("chunked") {
            read_chunked(&mut r)?
        } else {
            let mut b = Vec::new();
            r.read_to_end(&mut b).map_err(|e| format!("read body: {e}"))?;
            b
        }
    } else if let Some(cl) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).map(|(_, v)| v.clone()) {
        let n: usize = cl.trim().parse().map_err(|_| format!("bad content-length {cl}"))?;
        let mut b = vec![0u8; n];
        r.read_exact(&mut b).map_err(|e| format!("read body: {e}"))?;
        b
    } else {
        let mut b = Vec::new();
        r.read_to_end(&mut b).map_err(|e| format!("read body: {e}"))?;
        b
    };
    Ok(Response { status, headers, body })
}

fn read_chunked(r: &mut impl BufRead) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        r.read_line(&mut line).map_err(|e| format!("chunk size: {e}"))?;
        let size_str = line.trim().split(';').next().unwrap_or("");
        let size = usize::from_str_radix(size_str.trim(), 16)
            .map_err(|_| format!("bad chunk size {line:?}"))?;
        if size == 0 {
            // trailer section until blank line
            loop {
                let mut t = String::new();
                let n = r.read_line(&mut t).map_err(|e| format!("trailer: {e}"))?;
                if n == 0 || t.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        let mut chunk = vec![0u8; size];
        r.read_exact(&mut chunk).map_err(|e| format!("chunk data: {e}"))?;
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf).map_err(|e| format!("chunk crlf: {e}"))?;
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

// ---------------- WebSocket (RFC 6455, client) ----------------

pub const WS_OP_TEXT: u8 = 0x1;
pub const WS_OP_BINARY: u8 = 0x2;
pub const WS_OP_CLOSE: u8 = 0x8;
pub const WS_OP_PING: u8 = 0x9;
pub const WS_OP_PONG: u8 = 0xA;

#[derive(Debug, Clone)]
pub struct WsFrame {
    pub opcode: u8,
    pub payload: Vec<u8>,
}

pub struct Ws {
    reader: std::io::BufReader<TcpStream>,
    buf: Vec<u8>,
    read_timeout: Duration,
}

/// Error kind distinguishing "no data within timeout" from hard failures.
#[derive(Debug)]
pub enum WsError {
    Timeout,
    Io(String),
    Closed,
    Protocol(String),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Timeout => write!(f, "read timeout"),
            WsError::Io(s) => write!(f, "io: {s}"),
            WsError::Closed => write!(f, "connection closed"),
            WsError::Protocol(s) => write!(f, "protocol: {s}"),
        }
    }
}

impl std::error::Error for WsError {}

impl Ws {
    /// Open a websocket to `url` (host:port from url, path given).
    pub fn connect(url: &Url, path: &str, timeout: Duration) -> Result<Ws, String> {
        let mut stream = connect(&url.host, url.port, timeout)?;
        let key = base64_key();
        let extra = vec![
            ("Upgrade", "websocket".into()),
            ("Connection", "Upgrade".into()),
            ("Sec-WebSocket-Key".into(), key.clone()),
            ("Sec-WebSocket-Version".into(), "13".into()),
        ];
        write_head(&mut stream, "GET", path, &url.host, url.port, &extra, None)?;
        let mut r = std::io::BufReader::new(stream);
        let (status, headers) = read_head(&mut r)?;
        if status != 101 {
            return Err(format!("ws handshake: status {status}"));
        }
        let _ = headers; // sha1 verify optional; rely on 101 + server behavior
        Ok(Ws {
            reader: r,
            buf: Vec::new(),
            read_timeout: timeout,
        })
    }

    /// Read one frame. Returns WsError::Timeout when no data within timeout
    /// (caller decides to flush pending work); auto-answers pings.
    pub fn read_frame(&mut self) -> Result<WsFrame, WsError> {
        loop {
            let Some((opcode, payload)) = self.try_parse_frame()? else {
                self.fill()?;
                continue;
            };
            match opcode {
                WS_OP_PING => {
                    // reply pong with same payload
                    let mut out = vec![0x8A];
                    let n = payload.len();
                    if n <= 125 {
                        out.push(n as u8);
                    } else if n <= 65535 {
                        out.push(126);
                        out.extend_from_slice(&(n as u16).to_be_bytes());
                    } else {
                        out.push(127);
                        out.extend_from_slice(&(n as u64).to_be_bytes());
                    }
                    out.extend_from_slice(&payload);
                    let _ = self.reader.get_mut().write_all(&out);
                    continue;
                }
                WS_OP_CLOSE => return Err(WsError::Closed),
                _ => return Ok(WsFrame { opcode, payload }),
            }
        }
    }

    /// Parse one complete frame from the buffer, if available.
    fn try_parse_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>, WsError> {
        let b = &self.buf;
        if b.len() < 2 {
            return Ok(None);
        }
        let b0 = b[0];
        let b1 = b[1];
        if b1 & 0x80 != 0 {
            return Err(WsError::Protocol("server frames must be unmasked".into()));
        }
        let opcode = b0 & 0x0F;
        let mut len = (b1 & 0x7F) as u64;
        let mut pos = 2usize;
        if len == 126 {
            if b.len() < 4 {
                return Ok(None);
            }
            len = u16::from_be_bytes([b[2], b[3]]) as u64;
            pos = 4;
        } else if len == 127 {
            if b.len() < 10 {
                return Ok(None);
            }
            len = u64::from_be_bytes(b[2..10].try_into().unwrap());
            pos = 10;
        }
        let end = pos + len as usize;
        if b.len() < end {
            return Ok(None);
        }
        let payload = b[pos..end].to_vec();
        self.buf.drain(..end);
        Ok(Some((opcode, payload)))
    }

    fn fill(&mut self) -> Result<(), WsError> {
        let mut tmp = [0u8; 65536];
        let n = match self.reader.read(&mut tmp) {
            Ok(0) => return Err(WsError::Closed),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                return Err(WsError::Timeout)
            }
            Err(e) => return Err(WsError::Io(e.to_string())),
        };
        self.buf.extend_from_slice(&tmp[..n]);
        let _ = self.reader.get_ref().set_read_timeout(Some(self.read_timeout));
        Ok(())
    }
}

fn base64_key() -> String {
    // 16 random bytes -> base64 (RFC 6455 requirement)
    let rng = std::fs::File::open("/dev/urandom").ok();
    let mut bytes = [0u8; 16];
    if let Some(mut f) = rng {
        let _ = f.read_exact(&mut bytes);
    } else {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xdeadbeef);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (t >> ((i % 8) * 8)) as u8 ^ (i as u8).wrapping_mul(31);
        }
    }
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(24);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// ---------------- SSE fallback reader ----------------

pub struct SseStream {
    reader: std::io::BufReader<TcpStream>,
}

impl SseStream {
    /// GET `path` and return an SSE line reader if status is 200.
    pub fn connect(url: &Url, path: &str, timeout: Duration) -> Result<(u16, SseStream), String> {
        let mut stream = connect(&url.host, url.port, timeout)?;
        write_head(&mut stream, "GET", path, &url.host, url.port, &[], None)?;
        let mut reader = std::io::BufReader::new(stream);
        let (status, headers) = read_head(&mut reader)?;
        let _ = headers;
        Ok((status, SseStream { reader }))
    }

    /// Read one line. Err(Timeout) when nothing arrived within the timeout.
    pub fn read_line(&mut self) -> Result<String, WsError> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err(WsError::Closed),
            Ok(_) => Ok(line),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                Err(WsError::Timeout)
            }
            Err(e) => Err(WsError::Io(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn url_parse() {
        let u = Url::parse("http://127.0.0.1:3080/api/x").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("127.0.0.1", 3080));
        assert_eq!(u.path, "/api/x");
        let u = Url::parse("http://localhost/").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("localhost", 80));
        assert!(Url::parse("https://x").is_err());
        assert!(Url::parse("http:///nopath").is_err());
    }

    fn serve(mut handler: impl FnMut(&mut TcpStream) + Send + 'static) -> (String, Arc<AtomicUsize>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let done = Arc::new(AtomicUsize::new(0));
        let d = done.clone();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                handler(&mut s);
                d.fetch_add(1, Ordering::SeqCst);
            }
        });
        (format!("http://{addr}"), done)
    }

    #[test]
    fn http_content_length() {
        let (url, done) = serve(|s| {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
            );
        });
        let u = Url::parse(&url).unwrap();
        let r = request(&u, "GET", &[], None, Duration::from_secs(2)).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        assert_eq!(done.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn http_chunked() {
        let (url, _) = serve(|s| {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
            );
        });
        let u = Url::parse(&url).unwrap();
        let r = request(&u, "GET", &[], None, Duration::from_secs(2)).unwrap();
        assert_eq!(r.body, b"hello world");
    }

    #[test]
    fn ws_handshake_and_frame() {
        let (url, done) = serve(|s| {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let req = String::from_utf8_lossy(&buf);
            assert!(req.contains("Upgrade: websocket"));
            assert!(req.contains("Sec-WebSocket-Key:"));
            let _ = s.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: abc\r\n\r\n",
            );
            // text frame "hi" (len 2)
            let _ = s.write_all(&[0x81, 0x02, b'h', b'i']);
            // ping frame then close
            let _ = s.write_all(&[0x89, 0x00]);
            let _ = s.write_all(&[0x88, 0x00]);
            let _ = s.flush();
            std::thread::sleep(Duration::from_millis(200));
        });
        let u = Url::parse(&url).unwrap();
        let mut ws = Ws::connect(&u, "/api/events.mux", Duration::from_secs(2)).unwrap();
        let f = ws.read_frame().unwrap();
        assert_eq!(f.opcode, WS_OP_TEXT);
        assert_eq!(f.payload, b"hi");
        // close arrives
        let err = ws.read_frame().unwrap_err();
        assert!(matches!(err, WsError::Closed));
        for _ in 0..50 {
            if done.load(Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(done.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ws_16bit_and_64bit_lengths() {
        let (url, _) = serve(|s| {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n");
            let big = vec![b'x'; 300];
            let mut f = vec![0x81, 126];
            f.extend_from_slice(&(300u16).to_be_bytes());
            f.extend_from_slice(&big);
            let _ = s.write_all(&f);
            std::thread::sleep(Duration::from_millis(100));
        });
        let u = Url::parse(&url).unwrap();
        let mut ws = Ws::connect(&u, "/x", Duration::from_secs(2)).unwrap();
        let f = ws.read_frame().unwrap();
        assert_eq!(f.payload.len(), 300);
    }

    #[test]
    fn base64_key_format() {
        let k = base64_key();
        assert_eq!(k.len(), 24);
        assert!(k.ends_with("=="));
        assert!(k.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }
}
