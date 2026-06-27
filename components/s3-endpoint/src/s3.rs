//! DB-agnostic S3 object operations over HTTPS.
//!
//! The op surface (get/put/delete/head/list/copy) + URL building + XML/metadata
//! parsing mirror sqlink's native `host/src/s3.rs` (which used aws-sigv4 +
//! reqwest — neither builds for wasm). The signing is `crate::sigv4` (reused
//! from ducklink's s3fs-component), and the transport is a generalized version
//! of s3fs's `https_get`/`http_get` (std `TcpStream` + pure-Rust rustls TLS)
//! that handles any method, request headers, and a request body — the
//! wasm32-wasip2-portable path.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use crate::sigv4;
use crate::types::*;

/// One signed-and-sent S3 request's raw outcome.
struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k == &want)
            .map(|(_, v)| v.as_str())
    }
    fn body_preview(&self) -> String {
        String::from_utf8_lossy(&self.body)
            .chars()
            .take(512)
            .collect()
    }
}

/// Parsed connection target for a request.
struct Target {
    tls: bool,
    /// TCP connect host / TLS SNI (no port).
    connect_host: String,
    port: u16,
    /// `Host:` header value (host[:port] when port is non-default).
    host_header: String,
    /// Absolute, percent-encoded request path (no query), always starts '/'.
    path: String,
    /// Canonical query string (already encoded + sorted), or empty.
    query: String,
}

/// Build the connection target for a bucket/key against the configured
/// endpoint. `path_style` selects path (host/bucket/key) vs virtual-host
/// (bucket.host) addressing. `query` is the raw (name, value) param list; it is
/// canonicalized here and reused for signing.
fn build_target(
    endpoint: &S3EndpointConfig,
    bucket: &str,
    key: &str,
    query: &[(String, String)],
) -> Result<Target, S3Error> {
    let base = endpoint.url.trim_end_matches('/');
    let (tls, rest) = if let Some(r) = base.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = base.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(S3Error::InvalidRequest(format!(
            "endpoint URL must begin with http:// or https://: {base:?}"
        )));
    };
    // host[:port][/path-prefix]
    let (hostport, path_prefix) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let default_port: u16 = if tls { 443 } else { 80 };
    let (host_only, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
        None => (hostport.to_string(), default_port),
    };

    let enc_key = sigv4::uri_encode_path(key);

    let (connect_host, host_no_port, path) = if endpoint.path_style {
        let path = if bucket.is_empty() {
            format!("{path_prefix}/")
        } else if key.is_empty() {
            format!("{path_prefix}/{bucket}")
        } else {
            format!("{path_prefix}/{bucket}/{enc_key}")
        };
        (host_only.clone(), host_only.clone(), path)
    } else {
        // Virtual-host style: bucket becomes a subdomain.
        let vhost = if bucket.is_empty() {
            host_only.clone()
        } else {
            format!("{bucket}.{host_only}")
        };
        let path = if key.is_empty() {
            format!("{path_prefix}/")
        } else {
            format!("{path_prefix}/{enc_key}")
        };
        (vhost.clone(), vhost, path)
    };

    let host_header = if port == default_port {
        host_no_port
    } else {
        format!("{host_no_port}:{port}")
    };

    Ok(Target {
        tls,
        connect_host,
        port,
        host_header,
        path: if path.is_empty() { "/".to_string() } else { path },
        query: sigv4::canonical_query(query),
    })
}

/// Sign + send one S3 request. `extra_headers` carries the per-method
/// functional headers (range, content-type, x-amz-meta-*, x-amz-copy-source,
/// …) — all of which are signed. `body` is the request payload (empty for
/// GET/HEAD/DELETE/LIST/COPY).
fn send_signed(
    method: &str,
    target: &Target,
    endpoint: &S3EndpointConfig,
    credentials: &S3Credentials,
    body: &[u8],
    extra_headers: &[(String, String)],
) -> Result<RawResponse, S3Error> {
    let amz_date = sigv4::amz_date_now();
    let payload_hash = if body.is_empty() {
        sigv4::EMPTY_PAYLOAD_SHA256.to_string()
    } else {
        sigv4::sha256_hex(body)
    };

    // The signed header set: host + the x-amz markers + the functional extras.
    let mut signed: Vec<(String, String)> = vec![
        ("host".to_string(), target.host_header.clone()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    for (k, v) in extra_headers {
        signed.push((k.to_ascii_lowercase(), v.clone()));
    }

    let creds_present = !credentials.access_key_id.trim().is_empty()
        && !credentials.secret_access_key.trim().is_empty();
    if creds_present {
        if let Some(tok) = credentials.session_token.as_ref().filter(|s| !s.is_empty()) {
            signed.push(("x-amz-security-token".to_string(), tok.clone()));
        }
        let sig_creds = sigv4::Credentials {
            access_key: credentials.access_key_id.clone(),
            secret_key: credentials.secret_access_key.clone(),
            session_token: credentials.session_token.clone(),
        };
        let authz = sigv4::sign_v4(
            &sig_creds,
            method,
            &endpoint.region,
            "s3",
            &target.path,
            &target.query,
            &signed,
            &payload_hash,
            &amz_date,
        );
        signed.push(("authorization".to_string(), authz));
    }
    // else: anonymous mode (public bucket) — no Authorization header.

    let request_target = if target.query.is_empty() {
        target.path.clone()
    } else {
        format!("{}?{}", target.path, target.query)
    };

    transport(target, method, &request_target, &signed, body)
}

/// HTTP(S) request over std `TcpStream` (+ rustls when TLS). Writes the request
/// line, the signed headers, a few unsigned hop headers, and the body; reads
/// the full response and parses status/headers/body.
fn transport(
    target: &Target,
    method: &str,
    request_target: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<RawResponse, S3Error> {
    let mut req = format!("{method} {request_target} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    // Unsigned hop headers. Content-Length is required for a body; S3 does not
    // require it to be signed.
    req.push_str(&format!("content-length: {}\r\n", body.len()));
    req.push_str("user-agent: datalink-s3-endpoint/0.1\r\naccept: */*\r\nconnection: close\r\n\r\n");

    let raw = if target.tls {
        send_tls(&target.connect_host, target.port, req.as_bytes(), body)?
    } else {
        send_plain(&target.connect_host, target.port, req.as_bytes(), body)?
    };
    parse_response(&raw)
}

fn send_plain(host: &str, port: u16, head: &[u8], body: &[u8]) -> Result<Vec<u8>, S3Error> {
    let mut stream = TcpStream::connect((host, port))
        .map_err(|e| S3Error::NetworkError(format!("connect {host}:{port}: {e}")))?;
    stream
        .write_all(head)
        .and_then(|_| stream.write_all(body))
        .map_err(|e| S3Error::NetworkError(format!("send: {e}")))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| S3Error::NetworkError(format!("read: {e}")))?;
    if raw.is_empty() {
        return Err(S3Error::NetworkError("empty response".into()));
    }
    Ok(raw)
}

fn send_tls(host: &str, port: u16, head: &[u8], body: &[u8]) -> Result<Vec<u8>, S3Error> {
    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| S3Error::Internal(format!("tls config: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| S3Error::InvalidRequest(format!("bad server name '{host}': {e}")))?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| S3Error::Internal(format!("tls connect: {e}")))?;
    let mut sock = TcpStream::connect((host, port))
        .map_err(|e| S3Error::NetworkError(format!("connect {host}:{port}: {e}")))?;
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);
    tls.write_all(head)
        .and_then(|_| tls.write_all(body))
        .map_err(|e| S3Error::NetworkError(format!("send: {e}")))?;
    // S3 often closes without a TLS close_notify; tolerate that.
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    if raw.is_empty() {
        return Err(S3Error::NetworkError("empty response".into()));
    }
    Ok(raw)
}

/// Split an HTTP/1.1 response into status code, headers, and a (de-chunked if
/// needed) body.
fn parse_response(raw: &[u8]) -> Result<RawResponse, S3Error> {
    let sep = b"\r\n\r\n";
    let pos = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| S3Error::ParseError("no header terminator".into()))?;
    let head = &raw[..pos];
    let body_start = pos + sep.len();

    let mut lines = head.split(|&b| b == b'\n');
    let status_line = lines.next().unwrap_or(&[]);
    let status_text = String::from_utf8_lossy(status_line);
    let code: u16 = status_text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| S3Error::ParseError(format!("bad status line '{}'", status_text.trim())))?;

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let raw_body = &raw[body_start..];
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let body = if chunked {
        dechunk(raw_body)
    } else {
        raw_body.to_vec()
    };

    Ok(RawResponse {
        status: code,
        headers,
        body,
    })
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

/// Map a non-2xx status into the appropriate `S3Error`.
fn check_status(status: u16, body_preview: &str) -> Result<(), S3Error> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    match status {
        403 => Err(S3Error::AccessDenied),
        404 => {
            if body_preview.contains("NoSuchBucket") {
                Err(S3Error::NoSuchBucket)
            } else {
                Err(S3Error::NoSuchKey)
            }
        }
        400 => {
            if body_preview.contains("InvalidBucketName") {
                Err(S3Error::InvalidBucketName)
            } else {
                Err(S3Error::InvalidRequest(format!("HTTP 400: {body_preview}")))
            }
        }
        _ => Err(S3Error::Internal(format!("HTTP {status}: {body_preview}"))),
    }
}

fn extract_metadata(resp: &RawResponse) -> S3ObjectMetadata {
    let content_length = resp.header("content-length").and_then(|s| s.parse::<u64>().ok());
    let last_modified = resp.header("last-modified").and_then(httpdate_parse);
    let custom = resp
        .headers
        .iter()
        .filter_map(|(k, v)| k.strip_prefix("x-amz-meta-").map(|n| (n.to_string(), v.clone())))
        .collect();
    S3ObjectMetadata {
        content_type: resp.header("content-type").map(|s| s.to_string()),
        content_length,
        etag: resp.header("etag").map(|s| s.trim_matches('"').to_string()),
        last_modified,
        custom,
    }
}

// ---- public ops, mirroring sqlink's host/src/s3.rs op_* surface ----

pub fn op_get(req: GetReq) -> Result<GetResp, S3Error> {
    let target = build_target(&req.endpoint, &req.bucket, &req.key, &[])?;
    let mut extras = Vec::new();
    if let Some((start, end)) = req.range {
        extras.push(("range".to_string(), format!("bytes={start}-{end}")));
    }
    if let Some(m) = req.if_match {
        extras.push(("if-match".to_string(), m));
    }
    if let Some(m) = req.if_none_match {
        extras.push(("if-none-match".to_string(), m));
    }
    let resp = send_signed("GET", &target, &req.endpoint, &req.credentials, &[], &extras)?;
    if !(200..300).contains(&resp.status) {
        check_status(resp.status, &resp.body_preview())?;
    }
    let metadata = extract_metadata(&resp);
    Ok(GetResp {
        body: serde_bytes::ByteBuf::from(resp.body),
        metadata,
    })
}

pub fn op_put(req: PutReq) -> Result<PutResp, S3Error> {
    let target = build_target(&req.endpoint, &req.bucket, &req.key, &[])?;
    let mut extras = Vec::new();
    if let Some(ct) = req.content_type {
        extras.push(("content-type".to_string(), ct));
    }
    if let Some(cc) = req.cache_control {
        extras.push(("cache-control".to_string(), cc));
    }
    for (k, v) in req.metadata {
        extras.push((format!("x-amz-meta-{k}"), v));
    }
    let resp = send_signed(
        "PUT",
        &target,
        &req.endpoint,
        &req.credentials,
        &req.body,
        &extras,
    )?;
    if !(200..300).contains(&resp.status) {
        check_status(resp.status, &resp.body_preview())?;
    }
    Ok(PutResp {
        etag: resp
            .header("etag")
            .map(|s| s.trim_matches('"').to_string())
            .unwrap_or_default(),
    })
}

pub fn op_delete(req: DeleteReq) -> Result<(), S3Error> {
    let target = build_target(&req.endpoint, &req.bucket, &req.key, &[])?;
    let resp = send_signed("DELETE", &target, &req.endpoint, &req.credentials, &[], &[])?;
    if !(200..300).contains(&resp.status) {
        check_status(resp.status, &resp.body_preview())?;
    }
    Ok(())
}

pub fn op_head(req: HeadReq) -> Result<HeadResp, S3Error> {
    let target = build_target(&req.endpoint, &req.bucket, &req.key, &[])?;
    let resp = send_signed("HEAD", &target, &req.endpoint, &req.credentials, &[], &[])?;
    if !(200..300).contains(&resp.status) {
        check_status(resp.status, "")?;
    }
    Ok(HeadResp {
        metadata: extract_metadata(&resp),
    })
}

pub fn op_list(req: ListReq) -> Result<ListResp, S3Error> {
    let mut query = vec![("list-type".to_string(), "2".to_string())];
    if let Some(p) = req.prefix {
        query.push(("prefix".to_string(), p));
    }
    if let Some(d) = req.delimiter {
        query.push(("delimiter".to_string(), d));
    }
    if let Some(m) = req.max_keys {
        query.push(("max-keys".to_string(), m.to_string()));
    }
    if let Some(t) = req.continuation_token {
        query.push(("continuation-token".to_string(), t));
    }
    let target = build_target(&req.endpoint, &req.bucket, "", &query)?;
    let resp = send_signed("GET", &target, &req.endpoint, &req.credentials, &[], &[])?;
    let body_str = String::from_utf8_lossy(&resp.body).into_owned();
    if !(200..300).contains(&resp.status) {
        check_status(resp.status, &body_str)?;
    }
    parse_list_response(&body_str)
}

pub fn op_copy(req: CopyReq) -> Result<PutResp, S3Error> {
    let target = build_target(&req.endpoint, &req.dest_bucket, &req.dest_key, &[])?;
    let copy_source = format!("/{}/{}", req.source_bucket, req.source_key);
    let extras = vec![("x-amz-copy-source".to_string(), copy_source)];
    let resp = send_signed("PUT", &target, &req.endpoint, &req.credentials, &[], &extras)?;
    let body_str = String::from_utf8_lossy(&resp.body).into_owned();
    if !(200..300).contains(&resp.status) {
        check_status(resp.status, &body_str)?;
    }
    let etag = resp
        .header("etag")
        .map(|s| s.trim_matches('"').to_string())
        .or_else(|| xml_tag(&body_str, "ETag").map(|s| s.trim_matches('"').to_string()))
        .unwrap_or_default();
    Ok(PutResp { etag })
}

/// Dry-run: build + sign the request WITHOUT sending it. Returns the method,
/// URL, host, signed headers and the Authorization value — used by the
/// standalone harness to verify the signing/request-construction path offline
/// (no live S3 needed).
pub fn op_sign(req: SignReq) -> Result<SignResp, S3Error> {
    let query: Vec<(String, String)> = req.query.unwrap_or_default();
    let target = build_target(&req.endpoint, &req.bucket, &req.key, &query)?;
    let body = req.body.map(|b| b.into_vec()).unwrap_or_default();
    let amz_date = req
        .amz_date
        .unwrap_or_else(sigv4::amz_date_now);
    let payload_hash = if body.is_empty() {
        sigv4::EMPTY_PAYLOAD_SHA256.to_string()
    } else {
        sigv4::sha256_hex(&body)
    };
    let mut signed: Vec<(String, String)> = vec![
        ("host".to_string(), target.host_header.clone()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    for (k, v) in req.extra_headers.unwrap_or_default() {
        signed.push((k.to_ascii_lowercase(), v));
    }
    let mut authorization = None;
    if !req.credentials.access_key_id.trim().is_empty() {
        if let Some(tok) = req.credentials.session_token.as_ref().filter(|s| !s.is_empty()) {
            signed.push(("x-amz-security-token".to_string(), tok.clone()));
        }
        let sig_creds = sigv4::Credentials {
            access_key: req.credentials.access_key_id.clone(),
            secret_key: req.credentials.secret_access_key.clone(),
            session_token: req.credentials.session_token.clone(),
        };
        let authz = sigv4::sign_v4(
            &sig_creds,
            &req.method,
            &req.endpoint.region,
            "s3",
            &target.path,
            &target.query,
            &signed,
            &payload_hash,
            &amz_date,
        );
        signed.push(("authorization".to_string(), authz.clone()));
        authorization = Some(authz);
    }
    let scheme = if target.tls { "https" } else { "http" };
    let url = if target.query.is_empty() {
        format!("{scheme}://{}{}", target.host_header, target.path)
    } else {
        format!("{scheme}://{}{}?{}", target.host_header, target.path, target.query)
    };
    Ok(SignResp {
        method: req.method,
        url,
        host: target.host_header,
        amz_date,
        headers: signed,
        authorization,
    })
}

// ---- minimal XML parsing (mirrors sqlink) ----

fn parse_list_response(body: &str) -> Result<ListResp, S3Error> {
    let mut objects = Vec::new();
    let mut common_prefixes = Vec::new();
    for block in xml_blocks(body, "Contents") {
        let key = xml_tag(block, "Key").unwrap_or_default();
        let size: u64 = xml_tag(block, "Size").and_then(|s| s.parse().ok()).unwrap_or(0);
        let etag = xml_tag(block, "ETag").map(|s| s.trim_matches('"').to_string());
        let last_modified = xml_tag(block, "LastModified").and_then(|s| iso8601_to_epoch(&s));
        let storage_class = xml_tag(block, "StorageClass");
        objects.push(S3ObjectInfo {
            key,
            size,
            etag,
            last_modified,
            storage_class,
        });
    }
    for block in xml_blocks(body, "CommonPrefixes") {
        if let Some(p) = xml_tag(block, "Prefix") {
            common_prefixes.push(p);
        }
    }
    let is_truncated = xml_tag(body, "IsTruncated")
        .map(|s| s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let next_continuation_token = xml_tag(body, "NextContinuationToken");
    Ok(ListResp {
        objects,
        common_prefixes,
        next_continuation_token,
        is_truncated,
    })
}

fn xml_blocks<'a>(body: &'a str, tag: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut cursor = 0;
    while let Some(start) = body[cursor..].find(&open) {
        let abs_start = cursor + start + open.len();
        if let Some(end) = body[abs_start..].find(&close) {
            let abs_end = abs_start + end;
            out.push(&body[abs_start..abs_end]);
            cursor = abs_end + close.len();
        } else {
            break;
        }
    }
    out
}

fn xml_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)?;
    Some(body[start..start + end].to_string())
}

fn iso8601_to_epoch(s: &str) -> Option<u64> {
    let s = s.trim_end_matches('Z');
    let (date_part, time_part) = s.split_once('T')?;
    let date_bits: Vec<&str> = date_part.split('-').collect();
    if date_bits.len() != 3 {
        return None;
    }
    let year: i32 = date_bits[0].parse().ok()?;
    let month: u32 = date_bits[1].parse().ok()?;
    let day: u32 = date_bits[2].parse().ok()?;
    let time_clean = time_part.split('.').next().unwrap_or(time_part);
    let time_bits: Vec<&str> = time_clean.split(':').collect();
    if time_bits.len() != 3 {
        return None;
    }
    let hour: u32 = time_bits[0].parse().ok()?;
    let minute: u32 = time_bits[1].parse().ok()?;
    let second: u32 = time_bits[2].parse().ok()?;
    days_epoch(year, month, day).map(|days| {
        days * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64
    })
    .filter(|s| *s >= 0)
    .map(|s| s as u64)
}

fn httpdate_parse(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    let day: u32 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1, "Feb" => 2, "Mar" => 3, "Apr" => 4, "May" => 5, "Jun" => 6,
        "Jul" => 7, "Aug" => 8, "Sep" => 9, "Oct" => 10, "Nov" => 11, "Dec" => 12,
        _ => return None,
    };
    let year: i32 = parts[3].parse().ok()?;
    let hms: Vec<&str> = parts[4].split(':').collect();
    if hms.len() != 3 {
        return None;
    }
    let hour: u32 = hms[0].parse().ok()?;
    let minute: u32 = hms[1].parse().ok()?;
    let second: u32 = hms[2].parse().ok()?;
    days_epoch(year, month, day)
        .map(|days| days * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64)
        .filter(|s| *s >= 0)
        .map(|s| s as u64)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_epoch(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = (y - era * 400) as u32;
    let month_i: i32 = month as i32 + if month > 2 { -3 } else { 9 };
    let doy = (153 * month_i as u32 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe as i32) as i64 - 719468)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(url: &str, path_style: bool) -> S3EndpointConfig {
        S3EndpointConfig {
            url: url.to_string(),
            region: "us-east-1".to_string(),
            path_style,
        }
    }

    #[test]
    fn target_path_style() {
        let t = build_target(&ep("http://localhost:9000", true), "mybucket", "foo/bar.txt", &[]).unwrap();
        assert!(!t.tls);
        assert_eq!(t.connect_host, "localhost");
        assert_eq!(t.port, 9000);
        assert_eq!(t.host_header, "localhost:9000");
        assert_eq!(t.path, "/mybucket/foo/bar.txt");
    }

    #[test]
    fn target_virtual_host() {
        let t = build_target(&ep("https://s3.amazonaws.com", false), "mybucket", "foo.txt", &[]).unwrap();
        assert!(t.tls);
        assert_eq!(t.connect_host, "mybucket.s3.amazonaws.com");
        assert_eq!(t.port, 443);
        assert_eq!(t.host_header, "mybucket.s3.amazonaws.com");
        assert_eq!(t.path, "/foo.txt");
    }

    #[test]
    fn list_query_is_canonical() {
        let t = build_target(
            &ep("https://s3.amazonaws.com", false),
            "b",
            "",
            &[("list-type".into(), "2".into()), ("prefix".into(), "a b".into())],
        )
        .unwrap();
        assert_eq!(t.query, "list-type=2&prefix=a%20b");
    }

    #[test]
    fn xml_helpers() {
        let body = "<a><Key>k1</Key><Size>10</Size></a><a><Key>k2</Key><Size>20</Size></a>";
        let blocks = xml_blocks(body, "a");
        assert_eq!(blocks.len(), 2);
        assert_eq!(xml_tag(blocks[0], "Key").as_deref(), Some("k1"));
        assert_eq!(xml_tag(blocks[1], "Size").as_deref(), Some("20"));
    }

    #[test]
    fn iso_and_httpdate() {
        assert_eq!(iso8601_to_epoch("2024-01-15T08:12:31Z").unwrap(), 1705306351);
        assert_eq!(httpdate_parse("Mon, 15 Jan 2024 08:12:31 GMT").unwrap(), 1705306351);
    }

    #[test]
    fn dechunk_basic() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(raw), b"Wikipedia");
    }
}
