//! Plain HTTP/HTTPS over wasi:sockets + rustls.
//!
//! The transport (std `TcpStream` + pure-Rust rustls TLS, request writing,
//! response parsing incl. chunked decoding) is the proven path from the sibling
//! `s3-endpoint` provider — with the SigV4 signing removed (plain HTTP needs no
//! signing) and arbitrary methods / byte-valued headers / a request body.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use serde_bytes::ByteBuf;

use crate::types::{Field, HttpError, HttpRequest, HttpResponse};

/// Parsed connection target from a URL.
struct Target {
    tls: bool,
    /// TCP connect host / TLS SNI (no port).
    host: String,
    port: u16,
    /// Path + query (request line target), always starts with '/'.
    request_target: String,
    /// `Host:` header value (host[:port] when port is non-default).
    host_header: String,
}

/// Parse an `http(s)://host[:port]/path?query` URL into a connection target.
fn parse_url(url: &str) -> Result<Target, HttpError> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(HttpError::InvalidUrl(format!(
            "url must begin with http:// or https://: {url:?}"
        )));
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if hostport.is_empty() {
        return Err(HttpError::InvalidUrl(format!("missing host in {url:?}")));
    }
    let default_port: u16 = if tls { 443 } else { 80 };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse()
                .map_err(|_| HttpError::InvalidUrl(format!("bad port in {url:?}")))?,
        ),
        None => (hostport.to_string(), default_port),
    };
    let host_header = if port == default_port {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    Ok(Target {
        tls,
        host,
        port,
        request_target: if path.is_empty() { "/".to_string() } else { path.to_string() },
        host_header,
    })
}

/// One raw HTTP response.
struct RawResponse {
    status: u16,
    headers: Vec<Field>,
    body: Vec<u8>,
}

/// Execute an HTTP request and return the parsed response.
pub fn execute(req: HttpRequest) -> Result<HttpResponse, HttpError> {
    let target = parse_url(&req.url)?;
    let body: &[u8] = match &req.body {
        Some(b) => b.as_ref(),
        None => &[],
    };
    let timeout = req.timeout_ms.map(|ms| Duration::from_millis(ms as u64));

    // Assemble the request: request line, caller headers (raw bytes values),
    // then the hop headers we own (Host, Content-Length, User-Agent, Accept,
    // Connection). A caller-supplied `host`/`content-length`/`connection` is
    // skipped so we don't duplicate them.
    let mut head = format!("{} {} HTTP/1.1\r\n", req.method, target.request_target).into_bytes();
    let owned = ["host", "content-length", "connection"];
    for (k, v) in &req.headers {
        if owned.contains(&k.to_ascii_lowercase().as_str()) {
            continue;
        }
        head.extend_from_slice(k.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(v);
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(format!("host: {}\r\n", target.host_header).as_bytes());
    head.extend_from_slice(format!("content-length: {}\r\n", body.len()).as_bytes());
    head.extend_from_slice(
        b"user-agent: datalink-http-endpoint/0.1\r\naccept: */*\r\nconnection: close\r\n\r\n",
    );

    let raw = if target.tls {
        send_tls(&target.host, target.port, &head, body, timeout)?
    } else {
        send_plain(&target.host, target.port, &head, body, timeout)?
    };
    let resp = parse_response(&raw)?;
    Ok(HttpResponse {
        status: resp.status,
        headers: resp.headers,
        body: ByteBuf::from(resp.body),
    })
}

fn set_timeout(stream: &TcpStream, timeout: Option<Duration>) {
    if let Some(t) = timeout {
        // Best-effort: wasip2 may not support socket timeouts; ignore errors.
        let _ = stream.set_read_timeout(Some(t));
        let _ = stream.set_write_timeout(Some(t));
    }
}

fn map_io(e: std::io::Error) -> HttpError {
    if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock {
        HttpError::TimedOut
    } else {
        HttpError::ConnectionError(e.to_string())
    }
}

fn send_plain(
    host: &str,
    port: u16,
    head: &[u8],
    body: &[u8],
    timeout: Option<Duration>,
) -> Result<Vec<u8>, HttpError> {
    let mut stream = TcpStream::connect((host, port))
        .map_err(|e| HttpError::ConnectionError(format!("connect {host}:{port}: {e}")))?;
    set_timeout(&stream, timeout);
    stream
        .write_all(head)
        .and_then(|_| stream.write_all(body))
        .map_err(map_io)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(map_io)?;
    if raw.is_empty() {
        return Err(HttpError::ProtocolError("empty response".into()));
    }
    Ok(raw)
}

fn send_tls(
    host: &str,
    port: u16,
    head: &[u8],
    body: &[u8],
    timeout: Option<Duration>,
) -> Result<Vec<u8>, HttpError> {
    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| HttpError::Other(format!("tls config: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| HttpError::InvalidUrl(format!("bad server name '{host}': {e}")))?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| HttpError::Other(format!("tls connect: {e}")))?;
    let mut sock = TcpStream::connect((host, port))
        .map_err(|e| HttpError::ConnectionError(format!("connect {host}:{port}: {e}")))?;
    set_timeout(&sock, timeout);
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);
    tls.write_all(head)
        .and_then(|_| tls.write_all(body))
        .map_err(map_io)?;
    // Servers often close without a TLS close_notify; tolerate that.
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    if raw.is_empty() {
        return Err(HttpError::ProtocolError("empty response".into()));
    }
    Ok(raw)
}

/// Split an HTTP/1.1 response into status, headers (raw bytes values), and a
/// (de-chunked if needed) body.
fn parse_response(raw: &[u8]) -> Result<RawResponse, HttpError> {
    let sep = b"\r\n\r\n";
    let pos = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| HttpError::ProtocolError("no header terminator".into()))?;
    let head = &raw[..pos];
    let body_start = pos + sep.len();

    let mut lines = head.split(|&b| b == b'\n');
    let status_line = lines.next().unwrap_or(&[]);
    let status_text = String::from_utf8_lossy(status_line);
    let status: u16 = status_text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| HttpError::ProtocolError(format!("bad status line '{}'", status_text.trim())))?;

    let mut headers: Vec<Field> = Vec::new();
    let mut chunked = false;
    for line in lines {
        // Trim a trailing '\r' (and any '\n' the split left).
        let line = match line.split_last() {
            Some((b'\r', rest)) => rest,
            _ => line,
        };
        if line.is_empty() {
            continue;
        }
        if let Some(colon) = line.iter().position(|&b| b == b':') {
            let name = String::from_utf8_lossy(&line[..colon]).trim().to_ascii_lowercase();
            let value = trim_ascii(&line[colon + 1..]);
            if name == "transfer-encoding"
                && String::from_utf8_lossy(value).to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
            headers.push((name, ByteBuf::from(value.to_vec())));
        }
    }

    let raw_body = &raw[body_start..];
    let body = if chunked { dechunk(raw_body) } else { raw_body.to_vec() };
    Ok(RawResponse { status, headers, body })
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).map(|i| i + 1).unwrap_or(start);
    &b[start..end]
}

/// Minimal HTTP/1.1 chunked-transfer decoder.
fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let nl = match data.windows(2).position(|w| w == b"\r\n") {
            Some(i) => i,
            None => break,
        };
        let size_str = String::from_utf8_lossy(&data[..nl]);
        let size = usize::from_str_radix(size_str.trim().split(';').next().unwrap_or("0").trim(), 16)
            .unwrap_or(0);
        data = &data[nl + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            out.extend_from_slice(data);
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_https_default_port() {
        let t = parse_url("https://example.com/a/b?x=1").unwrap();
        assert!(t.tls);
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, 443);
        assert_eq!(t.request_target, "/a/b?x=1");
        assert_eq!(t.host_header, "example.com");
    }

    #[test]
    fn parse_url_http_custom_port_and_root() {
        let t = parse_url("http://127.0.0.1:8080").unwrap();
        assert!(!t.tls);
        assert_eq!(t.port, 8080);
        assert_eq!(t.request_target, "/");
        assert_eq!(t.host_header, "127.0.0.1:8080");
    }

    #[test]
    fn parse_url_rejects_non_http() {
        assert!(matches!(parse_url("ftp://x/y"), Err(HttpError::InvalidUrl(_))));
    }

    #[test]
    fn parse_response_headers_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        let ct = r.headers.iter().find(|(k, _)| k == "content-type").unwrap();
        assert_eq!(ct.1.as_ref(), b"text/plain");
    }

    #[test]
    fn dechunk_basic() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(raw), b"Wikipedia");
    }
}
