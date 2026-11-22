//! Lightweight HTTP/1.1, chunked-transfer, and Docker `101 UPGRADED` TCP
//! hijacking helpers over Unix domain sockets.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub const API_VERSION: &str = "1.47";
pub const MIN_API_VERSION: &str = "1.24";
const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_JSON_BODY: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RequestHead {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: HashMap<String, String>,
    pub content_length: Option<u64>,
    pub chunked: bool,
    pub expect_continue: bool,
    pub close_after: bool,
}

impl RequestHead {
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn query_params(&self, key: &str) -> Vec<&str> {
        self.query.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str()).collect()
    }

    pub fn query_bool(&self, key: &str, default: bool) -> bool {
        match self.query_param(key) {
            Some(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
            None => default,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    pub fn wants_upgrade(&self) -> bool {
        self.header("connection")
            .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
            || self.header("upgrade").is_some()
    }
}

pub async fn read_request_head<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<Option<RequestHead>> {
    let mut line = String::new();
    let mut total_bytes = 0usize;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        total_bytes += n;
        if total_bytes > MAX_HEADER_BYTES {
            bail!("HTTP request headers too large");
        }
        if !line.trim().is_empty() {
            break;
        }
    }

    let mut parts = line.trim_end_matches(['\r', '\n']).split_whitespace();
    let method = parts.next().ok_or_else(|| anyhow!("missing HTTP method"))?.to_ascii_uppercase();
    let target = parts.next().ok_or_else(|| anyhow!("missing HTTP target"))?;
    let version = parts.next().unwrap_or("HTTP/1.1").to_ascii_uppercase();

    let (raw_path, raw_query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    let stripped = strip_api_version(raw_path);
    let path = percent_decode(stripped, false);
    let path = if path.is_empty() { "/".into() } else { path };

    let mut query = Vec::new();
    if let Some(q) = raw_query {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (percent_decode(k, true), percent_decode(v, true)),
                None => (percent_decode(pair, true), String::new()),
            };
            query.push((k, v));
        }
    }

    let mut headers = HashMap::<String, String>::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            bail!("unexpected EOF while reading HTTP headers");
        }
        total_bytes += n;
        if total_bytes > MAX_HEADER_BYTES {
            bail!("HTTP request headers exceeded {MAX_HEADER_BYTES} bytes");
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            headers
                .entry(key)
                .and_modify(|prev| {
                    prev.push_str(", ");
                    prev.push_str(&val);
                })
                .or_insert(val);
        }
    }

    let content_length = headers
        .get("content-length")
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let expect_continue = headers
        .get("expect")
        .is_some_and(|v| v.to_ascii_lowercase().contains("100-continue"));
    let conn_hdr = headers.get("connection").map(|v| v.to_ascii_lowercase());
    let close_after = match conn_hdr.as_deref() {
        Some(c) if c.contains("close") => true,
        Some(c) if c.contains("keep-alive") => false,
        _ => version == "HTTP/1.0",
    };

    Ok(Some(RequestHead {
        method,
        path,
        query,
        headers,
        content_length,
        chunked,
        expect_continue,
        close_after,
    }))
}

/// Strip a Docker version prefix like `/v1.47/containers/json` → `/containers/json`.
pub fn strip_api_version(path: &str) -> &str {
    let Some(after_v) = path.strip_prefix("/v") else {
        return path;
    };
    let (seg, rest) = match after_v.find('/') {
        Some(idx) => (&after_v[..idx], &after_v[idx..]),
        None => (after_v, "/"),
    };
    if !seg.is_empty() && seg.as_bytes()[0].is_ascii_digit() && seg.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        rest
    } else {
        path
    }
}

pub fn percent_decode(input: &str, plus_as_space: bool) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        if plus_as_space && bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub async fn maybe_send_continue<W: AsyncWrite + Unpin>(head: &RequestHead, write: &mut W) -> Result<()> {
    if head.expect_continue {
        write.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        write.flush().await?;
    }
    Ok(())
}

pub async fn read_body_bytes<R, W>(reader: &mut BufReader<R>, write: &mut W, head: &RequestHead, max_bytes: usize) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    maybe_send_continue(head, write).await?;
    let mut buf = Vec::new();
    if head.chunked {
        stream_chunked(reader, max_bytes as u64, |chunk| {
            buf.extend_from_slice(chunk);
            Ok(())
        })
        .await?;
    } else if let Some(len) = head.content_length {
        if len as usize > max_bytes {
            bail!("request body exceeds limit ({len} > {max_bytes})");
        }
        buf.resize(len as usize, 0);
        reader.read_exact(&mut buf).await.context("reading fixed-length HTTP body")?;
    }
    Ok(buf)
}

pub async fn read_body_to_file<R, W>(
    reader: &mut BufReader<R>,
    write: &mut W,
    head: &RequestHead,
    path: &Path,
    max_bytes: u64,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;
    maybe_send_continue(head, write).await?;
    let mut file = tokio::fs::File::create(path).await.with_context(|| format!("creating {}", path.display()))?;
    let mut written = 0u64;
    if head.chunked {
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                bail!("unexpected EOF reading chunked request body");
            }
            let size_hex = line.trim().split(';').next().unwrap_or("").trim();
            let chunk_len = u64::from_str_radix(size_hex, 16).with_context(|| format!("invalid chunk size `{size_hex}`"))?;
            if chunk_len == 0 {
                skip_trailers(reader).await?;
                break;
            }
            written = written.checked_add(chunk_len).ok_or_else(|| anyhow!("chunked body size overflow"))?;
            if written > max_bytes {
                bail!("request upload exceeds {} GB limit", max_bytes >> 30);
            }
            let mut limited = reader.take(chunk_len);
            let copied = tokio::io::copy(&mut limited, &mut file).await?;
            if copied != chunk_len {
                bail!("truncated chunked request body");
            }
            consume_crlf(reader).await?;
        }
    } else if let Some(len) = head.content_length {
        if len > max_bytes {
            bail!("request upload exceeds {} GB limit", max_bytes >> 30);
        }
        let mut limited = reader.take(len);
        written = tokio::io::copy(&mut limited, &mut file).await?;
        if written != len {
            bail!("request body ended early ({written} of {len} bytes)");
        }
    }
    file.flush().await?;
    Ok(written)
}

pub async fn drain_body<R, W>(reader: &mut BufReader<R>, write: &mut W, head: &RequestHead) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if !head.chunked && head.content_length.unwrap_or(0) == 0 {
        return Ok(());
    }
    maybe_send_continue(head, write).await?;
    if head.chunked {
        stream_chunked(reader, u64::MAX, |_| Ok(())).await?;
    } else if let Some(len) = head.content_length {
        let mut sink = tokio::io::sink();
        let mut limited = reader.take(len);
        tokio::io::copy(&mut limited, &mut sink).await?;
    }
    Ok(())
}

async fn stream_chunked<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    max_bytes: u64,
    mut on_chunk: impl FnMut(&[u8]) -> Result<()>,
) -> Result<u64> {
    let mut total = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            bail!("unexpected EOF in chunked body");
        }
        let size_hex = line.trim().split(';').next().unwrap_or("").trim();
        let chunk_len = u64::from_str_radix(size_hex, 16).with_context(|| format!("invalid chunk size `{size_hex}`"))?;
        if chunk_len == 0 {
            skip_trailers(reader).await?;
            return Ok(total);
        }
        total = total.checked_add(chunk_len).ok_or_else(|| anyhow!("chunked body size overflow"))?;
        if total > max_bytes {
            bail!("request body exceeds limit");
        }
        let mut chunk = vec![0u8; chunk_len as usize];
        reader.read_exact(&mut chunk).await.context("reading chunk payload")?;
        consume_crlf(reader).await?;
        on_chunk(&chunk)?;
    }
}

async fn consume_crlf<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<()> {
    let mut b = [0u8; 2];
    reader.read_exact(&mut b[..1]).await?;
    if b[0] == b'\r' {
        reader.read_exact(&mut b[1..2]).await?;
    }
    Ok(())
}

async fn skip_trailers<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 || line.trim_end_matches(['\r', '\n']).is_empty() {
            return Ok(());
        }
    }
}

fn status_reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "OK",
    }
}

fn write_common_headers(out: &mut String, extra: &[(&str, &str)], close: bool) {
    out.push_str(&format!("Api-Version: {API_VERSION}\r\n"));
    out.push_str(&format!("Server: Graviton/{}\r\n", env!("CARGO_PKG_VERSION")));
    out.push_str("Ostype: linux\r\n");
    out.push_str("Builder-Version: 1\r\n");
    out.push_str("Docker-Experimental: false\r\n");
    for (k, v) in extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if close {
        out.push_str("Connection: close\r\n");
    }
}

pub async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(
    write: &mut W,
    status: u16,
    value: &T,
    close: bool,
) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write_bytes(write, status, "application/json", &body, &[], close).await
}

pub async fn write_error<W: AsyncWrite + Unpin>(write: &mut W, status: u16, message: &str, close: bool) -> Result<()> {
    write_json(write, status, &json!({ "message": message }), close).await
}

pub async fn write_text<W: AsyncWrite + Unpin>(
    write: &mut W,
    status: u16,
    text: &str,
    head_only: bool,
    close: bool,
) -> Result<()> {
    let mut hdr = format!("HTTP/1.1 {status} {}\r\n", status_reason(status));
    hdr.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    hdr.push_str(&format!("Content-Length: {}\r\n", text.len()));
    write_common_headers(&mut hdr, &[], close);
    hdr.push_str("\r\n");
    write.write_all(hdr.as_bytes()).await?;
    if !head_only {
        write.write_all(text.as_bytes()).await?;
    }
    write.flush().await?;
    Ok(())
}

pub async fn write_bytes<W: AsyncWrite + Unpin>(
    write: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, &str)],
    close: bool,
) -> Result<()> {
    let mut hdr = format!("HTTP/1.1 {status} {}\r\n", status_reason(status));
    hdr.push_str(&format!("Content-Type: {content_type}\r\n"));
    hdr.push_str(&format!("Content-Length: {}\r\n", body.len()));
    write_common_headers(&mut hdr, extra, close);
    hdr.push_str("\r\n");
    write.write_all(hdr.as_bytes()).await?;
    if !body.is_empty() {
        write.write_all(body).await?;
    }
    write.flush().await?;
    Ok(())
}

pub async fn write_empty<W: AsyncWrite + Unpin>(
    write: &mut W,
    status: u16,
    extra: &[(&str, &str)],
    close: bool,
) -> Result<()> {
    let mut hdr = format!("HTTP/1.1 {status} {}\r\n", status_reason(status));
    let needs_content_length = !(100..200).contains(&status) && status != 204 && status != 304;
    if needs_content_length && !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
        hdr.push_str("Content-Length: 0\r\n");
    }
    write_common_headers(&mut hdr, extra, close);
    hdr.push_str("\r\n");
    write.write_all(hdr.as_bytes()).await?;
    write.flush().await?;
    Ok(())
}

pub async fn write_chunked_head<W: AsyncWrite + Unpin>(
    write: &mut W,
    status: u16,
    content_type: &str,
    extra: &[(&str, &str)],
) -> Result<()> {
    let mut hdr = format!("HTTP/1.1 {status} {}\r\n", status_reason(status));
    hdr.push_str(&format!("Content-Type: {content_type}\r\n"));
    hdr.push_str("Transfer-Encoding: chunked\r\n");
    write_common_headers(&mut hdr, extra, false);
    hdr.push_str("\r\n");
    write.write_all(hdr.as_bytes()).await?;
    write.flush().await?;
    Ok(())
}

pub async fn write_chunk<W: AsyncWrite + Unpin>(write: &mut W, data: &[u8]) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let prefix = format!("{:x}\r\n", data.len());
    write.write_all(prefix.as_bytes()).await?;
    write.write_all(data).await?;
    write.write_all(b"\r\n").await?;
    write.flush().await?;
    Ok(())
}

pub async fn write_chunk_json<W: AsyncWrite + Unpin, T: Serialize>(write: &mut W, value: &T) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    write_chunk(write, &line).await
}

pub async fn finish_chunked<W: AsyncWrite + Unpin>(write: &mut W) -> Result<()> {
    write.write_all(b"0\r\n\r\n").await?;
    write.flush().await?;
    Ok(())
}

pub async fn write_hijack_head<W: AsyncWrite + Unpin>(write: &mut W, content_type: &str) -> Result<()> {
    let mut hdr = String::from("HTTP/1.1 101 UPGRADED\r\n");
    hdr.push_str(&format!("Content-Type: {content_type}\r\n"));
    hdr.push_str("Connection: Upgrade\r\n");
    hdr.push_str("Upgrade: tcp\r\n");
    write_common_headers(&mut hdr, &[], false);
    hdr.push_str("\r\n");
    write.write_all(hdr.as_bytes()).await?;
    write.flush().await?;
    Ok(())
}

pub async fn write_stream_start<W: AsyncWrite + Unpin>(write: &mut W, head: &RequestHead, content_type: &str) -> Result<()> {
    if head.wants_upgrade() {
        write_hijack_head(write, content_type).await
    } else {
        let mut hdr = String::from("HTTP/1.1 200 OK\r\n");
        hdr.push_str(&format!("Content-Type: {content_type}\r\n"));
        write_common_headers(&mut hdr, &[], true);
        hdr.push_str("\r\n");
        write.write_all(hdr.as_bytes()).await?;
        write.flush().await?;
        Ok(())
    }
}

/// Encode bytes into Docker's 8-byte multiplexed stream header format:
/// `[stream_type, 0, 0, 0, size_be32]` + `payload`.
/// `stream_type`: `1` = stdout, `2` = stderr.
pub fn encode_mux_frame(stream_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.push(stream_type);
    out.extend_from_slice(&[0u8; 3]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}
