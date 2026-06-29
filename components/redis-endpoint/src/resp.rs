//! RESP2 (REdis Serialization Protocol) over wasi:sockets + std `TcpStream`.
//!
//! This is the load-bearing proof of the pilot: a Redis "client" is just a
//! TCP wire client, and wasm32-wasip2 can open TCP sockets (the same path the
//! postgres_scanner / mysql_scanner wasm components and the sibling
//! http-endpoint provider use). No native host bridge is required.
//!
//! We implement the small subset of RESP2 needed to issue an arbitrary command
//! (an array of bulk strings) and parse any reply. Connections are stateless
//! per request — connect, optional AUTH/SELECT, send the command, read one
//! reply, close — mirroring the http-endpoint provider's connection model.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::types::{RedisError, RedisReply};

/// Connect, run any preamble (AUTH / SELECT), send one command, return the reply.
pub fn command(
    addr: &str,
    password: Option<&[u8]>,
    db: Option<i64>,
    args: &[Vec<u8>],
    timeout: Option<Duration>,
) -> Result<RedisReply, RedisError> {
    if args.is_empty() {
        return Err(RedisError::Protocol("empty command".into()));
    }
    let stream = TcpStream::connect(addr)
        .map_err(|e| RedisError::Connection(format!("connect {addr}: {e}")))?;
    if let Some(t) = timeout {
        // Best-effort: wasip2 may not honor socket timeouts; ignore errors.
        let _ = stream.set_read_timeout(Some(t));
        let _ = stream.set_write_timeout(Some(t));
    }
    // wasi:sockets does not support `try_clone`, so we drive both directions
    // through shared `&TcpStream` references (std implements Read + Write for
    // `&TcpStream`). No buffering: replies are read a byte at a time, which is
    // fine for the request/reply command model.
    let mut w = &stream;
    let mut r = &stream;

    // AUTH first if a password was supplied.
    if let Some(pw) = password {
        write_command(&mut w, &[b"AUTH".to_vec(), pw.to_vec()])?;
        if let RedisReply::Error(e) = read_reply(&mut r)? {
            return Err(RedisError::Command(format!("AUTH: {e}")));
        }
    }
    // SELECT a non-default database if requested.
    if let Some(idx) = db {
        write_command(&mut w, &[b"SELECT".to_vec(), idx.to_string().into_bytes()])?;
        if let RedisReply::Error(e) = read_reply(&mut r)? {
            return Err(RedisError::Command(format!("SELECT: {e}")));
        }
    }

    write_command(&mut w, args)?;
    read_reply(&mut r)
}

/// Encode a command as a RESP2 array of bulk strings and write it.
fn write_command<W: Write>(w: &mut W, args: &[Vec<u8>]) -> Result<(), RedisError> {
    let mut buf = Vec::new();
    buf.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    w.write_all(&buf).map_err(map_io)?;
    w.flush().map_err(map_io)
}

fn map_io(e: std::io::Error) -> RedisError {
    if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock {
        RedisError::TimedOut
    } else {
        RedisError::Connection(e.to_string())
    }
}

/// Read a single CRLF-terminated line (without the trailing CRLF).
fn read_line<R: Read>(r: &mut R) -> Result<Vec<u8>, RedisError> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = r.read(&mut byte).map_err(map_io)?;
        if n == 0 {
            return Err(RedisError::Protocol("unexpected eof reading line".into()));
        }
        if byte[0] == b'\r' {
            // consume the following '\n'
            let n2 = r.read(&mut byte).map_err(map_io)?;
            if n2 == 0 || byte[0] != b'\n' {
                return Err(RedisError::Protocol("malformed line terminator".into()));
            }
            return Ok(out);
        }
        out.push(byte[0]);
    }
}

fn parse_int(line: &[u8]) -> Result<i64, RedisError> {
    std::str::from_utf8(line)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .ok_or_else(|| RedisError::Protocol(format!("bad integer: {:?}", String::from_utf8_lossy(line))))
}

/// Parse one RESP2 reply. Arrays recurse.
pub fn read_reply<R: Read>(r: &mut R) -> Result<RedisReply, RedisError> {
    let mut prefix = [0u8; 1];
    let n = r.read(&mut prefix).map_err(map_io)?;
    if n == 0 {
        return Err(RedisError::Protocol("unexpected eof reading reply".into()));
    }
    match prefix[0] {
        b'+' => Ok(RedisReply::Simple(
            String::from_utf8_lossy(&read_line(r)?).into_owned(),
        )),
        b'-' => Ok(RedisReply::Error(
            String::from_utf8_lossy(&read_line(r)?).into_owned(),
        )),
        b':' => Ok(RedisReply::Int(parse_int(&read_line(r)?)?)),
        b'$' => {
            let len = parse_int(&read_line(r)?)?;
            if len < 0 {
                return Ok(RedisReply::Nil);
            }
            let len = len as usize;
            let mut body = vec![0u8; len];
            r.read_exact(&mut body).map_err(map_io)?;
            // consume trailing CRLF
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf).map_err(map_io)?;
            Ok(RedisReply::Bulk(body))
        }
        b'*' => {
            let count = parse_int(&read_line(r)?)?;
            if count < 0 {
                return Ok(RedisReply::Nil);
            }
            let mut items = Vec::with_capacity(count as usize);
            for _ in 0..count {
                items.push(read_reply(r)?);
            }
            Ok(RedisReply::Array(items))
        }
        other => Err(RedisError::Protocol(format!(
            "unknown reply prefix {:?}",
            other as char
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_string() {
        let mut data = &b"+PONG\r\n"[..];
        assert!(matches!(read_reply(&mut data).unwrap(), RedisReply::Simple(s) if s == "PONG"));
    }

    #[test]
    fn parse_error() {
        let mut data = &b"-ERR no such key\r\n"[..];
        assert!(matches!(read_reply(&mut data).unwrap(), RedisReply::Error(s) if s.contains("no such key")));
    }

    #[test]
    fn parse_integer() {
        let mut data = &b":42\r\n"[..];
        assert!(matches!(read_reply(&mut data).unwrap(), RedisReply::Int(42)));
    }

    #[test]
    fn parse_bulk_string() {
        let mut data = &b"$5\r\nhello\r\n"[..];
        assert!(matches!(read_reply(&mut data).unwrap(), RedisReply::Bulk(b) if b == b"hello"));
    }

    #[test]
    fn parse_nil_bulk() {
        let mut data = &b"$-1\r\n"[..];
        assert!(matches!(read_reply(&mut data).unwrap(), RedisReply::Nil));
    }

    #[test]
    fn parse_array() {
        let mut data = &b"*2\r\n$3\r\nfoo\r\n:7\r\n"[..];
        match read_reply(&mut data).unwrap() {
            RedisReply::Array(items) => {
                assert_eq!(items.len(), 2);
                assert!(matches!(&items[0], RedisReply::Bulk(b) if b == b"foo"));
                assert!(matches!(&items[1], RedisReply::Int(7)));
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn encode_command() {
        let mut buf = Vec::new();
        write_command(&mut buf, &[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]).unwrap();
        assert_eq!(buf, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
    }
}
