//! Inbound HTTP/HTTPS proxy handler. Supports:
//!   - `CONNECT host:port HTTP/1.1` (HTTPS / TLS tunnels)
//!   - Plain HTTP forwarding (`GET http://example/foo HTTP/1.1`)

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::parser::parse_auth;
use crate::proxy::upstream;
use crate::AppState;

const MAX_HEADER_BYTES: usize = 16 * 1024;

pub async fn handle(state: Arc<AppState>, mut sock: TcpStream, peer: SocketAddr) -> Result<()> {
    let (head, leftover) = read_head(&mut sock).await?;
    let req = parse_request(&head)?;

    // Auth check.
    let creds = req.proxy_authorization.as_deref();
    let parsed_user = match check_auth(&state, creds) {
        Some(u) => u,
        None => {
            let body = b"HTTP/1.1 407 Proxy Authentication Required\r\n\
                Proxy-Authenticate: Basic realm=\"proxy-gateway\"\r\n\
                Content-Length: 0\r\n\
                Connection: close\r\n\r\n";
            let _ = sock.write_all(body).await;
            return Ok(());
        }
    };

    let (master_user, _) = state.auth();
    let parsed_auth = parse_auth(&parsed_user, &master_user);

    let target = if req.method.eq_ignore_ascii_case("CONNECT") {
        parse_authority(&req.target)?
    } else {
        match extract_host_port_from_uri(&req.target, &req.host) {
            Some(v) => v,
            None => bail!("cannot determine target from {}", req.target),
        }
    };

    let (upstream_stream, upstream_proxy) =
        match upstream::dial_with_retry(&state, &parsed_auth, &target.0, target.1).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("dial {}:{} failed: {e:#}", target.0, target.1);
                let body = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(body).await;
                return Ok(());
            }
        };

    tracing::debug!(
        "{peer} → {} {} via #{} {}",
        req.method,
        req.target,
        upstream_proxy.id,
        upstream_proxy.endpoint()
    );

    if req.method.eq_ignore_ascii_case("CONNECT") {
        sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        bridge(sock, upstream_stream, leftover).await
    } else {
        let rebuilt = match upstream_proxy.scheme {
            crate::parser::Scheme::Http => rebuild_for_http_proxy(&head, &upstream_proxy),
            crate::parser::Scheme::Socks5 => rebuild_for_origin(&head),
        };
        let mut up = upstream_stream;
        up.write_all(rebuilt.as_bytes()).await?;
        bridge(sock, up, leftover).await
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

struct Request {
    method: String,
    target: String,
    host: String,
    proxy_authorization: Option<String>,
}

fn parse_request(head: &str) -> Result<Request> {
    let mut lines = head.split("\r\n");
    let first = lines.next().ok_or_else(|| anyhow!("empty request"))?;
    let mut parts = first.split_whitespace();
    let method = parts.next().ok_or_else(|| anyhow!("no method"))?.to_string();
    let target = parts.next().ok_or_else(|| anyhow!("no target"))?.to_string();
    let _ver = parts.next();

    let mut host = String::new();
    let mut auth = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (k, v) = match line.split_once(':') {
            Some(kv) => kv,
            None => continue,
        };
        let k = k.trim();
        let v = v.trim();
        if k.eq_ignore_ascii_case("Host") {
            host = v.to_string();
        } else if k.eq_ignore_ascii_case("Proxy-Authorization") {
            auth = Some(v.to_string());
        }
    }
    Ok(Request {
        method,
        target,
        host,
        proxy_authorization: auth,
    })
}

/// Read until end-of-headers (`\r\n\r\n`). Returns (head_str, leftover bytes
/// that were already on the wire after the head — they belong to the body or
/// the upgraded stream).
async fn read_head(sock: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            bail!("client closed before sending headers");
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            let head = String::from_utf8_lossy(&buf[..pos + 4]).into_owned();
            let leftover = buf[pos + 4..].to_vec();
            return Ok((head, leftover));
        }
        if buf.len() > MAX_HEADER_BYTES {
            bail!("request headers too large");
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_authority(s: &str) -> Result<(String, u16)> {
    let (h, p) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("bad authority: {s}"))?;
    let port: u16 = p.parse().map_err(|_| anyhow!("bad port: {p}"))?;
    Ok((h.to_string(), port))
}

fn extract_host_port_from_uri(uri: &str, host_header: &str) -> Option<(String, u16)> {
    if let Some(rest) = uri.strip_prefix("http://").or_else(|| uri.strip_prefix("https://")) {
        let authority = rest.split('/').next().unwrap_or(rest);
        if let Some((h, p)) = authority.rsplit_once(':') {
            if let Ok(port) = p.parse::<u16>() {
                return Some((h.to_string(), port));
            }
        }
        return Some((authority.to_string(), 80));
    }
    // Origin-form fallback: use Host header.
    if !host_header.is_empty() {
        if let Some((h, p)) = host_header.rsplit_once(':') {
            if let Ok(port) = p.parse::<u16>() {
                return Some((h.to_string(), port));
            }
        }
        return Some((host_header.to_string(), 80));
    }
    None
}

fn check_auth(state: &Arc<AppState>, header: Option<&str>) -> Option<String> {
    let header = header?;
    let value = header.strip_prefix("Basic ").or_else(|| header.strip_prefix("basic "))?;
    let decoded = B64.decode(value.trim()).ok()?;
    let s = String::from_utf8(decoded).ok()?;
    let (u, p) = s.split_once(':')?;
    let (_cfg_u, cfg_p) = state.auth();
    // Only the password must match; the username is free-form and is treated
    // as a routing/session hint by `parse_auth`.
    if p == cfg_p {
        Some(u.to_string())
    } else {
        None
    }
}

fn rebuild_for_http_proxy(head: &str, upstream: &crate::store::Proxy) -> String {
    let mut out = String::with_capacity(head.len());
    let mut first = true;
    let injected_auth = false;
    for line in head.split("\r\n") {
        if first {
            out.push_str(line);
            out.push_str("\r\n");
            first = false;
            continue;
        }
        if line.is_empty() {
            // End of headers — inject upstream auth if not already set.
            if !injected_auth {
                if let (Some(u), Some(p)) = (&upstream.user, &upstream.pass) {
                    let token = B64.encode(format!("{u}:{p}"));
                    out.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
                }
            }
            out.push_str("\r\n");
            break;
        }
        // Strip our client-facing auth header.
        if line
            .split_once(':')
            .map(|(k, _)| k.trim().eq_ignore_ascii_case("Proxy-Authorization"))
            .unwrap_or(false)
        {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

fn rebuild_for_origin(head: &str) -> String {
    let mut out = String::with_capacity(head.len());
    let mut first = true;
    for line in head.split("\r\n") {
        if first {
            // Replace request-line target with origin-form.
            let mut parts = line.splitn(3, ' ');
            let method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("");
            let ver = parts.next().unwrap_or("HTTP/1.1");
            let origin = origin_form(target);
            out.push_str(&format!("{method} {origin} {ver}\r\n"));
            first = false;
            continue;
        }
        if line.is_empty() {
            out.push_str("\r\n");
            break;
        }
        if line
            .split_once(':')
            .map(|(k, _)| k.trim().eq_ignore_ascii_case("Proxy-Authorization"))
            .unwrap_or(false)
        {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

fn origin_form(target: &str) -> String {
    if let Some(rest) = target.strip_prefix("http://").or_else(|| target.strip_prefix("https://")) {
        if let Some(idx) = rest.find('/') {
            return rest[idx..].to_string();
        }
        return "/".to_string();
    }
    target.to_string()
}

async fn bridge(
    mut client: TcpStream,
    mut server: TcpStream,
    leftover: Vec<u8>,
) -> Result<()> {
    if !leftover.is_empty() {
        server.write_all(&leftover).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
    Ok(())
}
