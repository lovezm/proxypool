//! Establishing a TCP tunnel to `target` through one of our static proxies.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::parser::{ParsedAuth, Scheme, Strategy};
use crate::store::Proxy;
use crate::AppState;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

pub async fn connect_through(
    upstream: &Proxy,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream> {
    let addr = format!("{}:{}", upstream.host, upstream.port);
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
        .await
        .map_err(|_| anyhow!("upstream connect timeout: {addr}"))??;
    let _ = stream.set_nodelay(true);

    match upstream.scheme {
        Scheme::Http => {
            let s = timeout(
                HANDSHAKE_TIMEOUT,
                http_connect(stream, upstream, target_host, target_port),
            )
            .await
            .map_err(|_| anyhow!("upstream HTTP CONNECT timeout"))??;
            Ok(s)
        }
        Scheme::Socks5 => {
            let s = timeout(
                HANDSHAKE_TIMEOUT,
                socks5_connect(stream, upstream, target_host, target_port),
            )
            .await
            .map_err(|_| anyhow!("upstream SOCKS5 CONNECT timeout"))??;
            Ok(s)
        }
    }
}

async fn http_connect(
    mut stream: TcpStream,
    upstream: &Proxy,
    host: &str,
    port: u16,
) -> Result<TcpStream> {
    let mut req = format!(
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: keep-alive\r\n"
    );
    if let (Some(u), Some(p)) = (&upstream.user, &upstream.pass) {
        let token = B64.encode(format!("{u}:{p}"));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;

    // Read until \r\n\r\n.
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 256];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            bail!("upstream closed during CONNECT");
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16 * 1024 {
            bail!("CONNECT response too large");
        }
    }
    let head = std::str::from_utf8(&buf).unwrap_or("");
    let status_line = head.lines().next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    let _ver = parts.next();
    let code: u16 = parts
        .next()
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow!("bad CONNECT response: {status_line}"))?;
    if !(200..300).contains(&code) {
        bail!("upstream CONNECT failed: {status_line}");
    }
    Ok(stream)
}

async fn socks5_connect(
    stream: TcpStream,
    upstream: &Proxy,
    host: &str,
    port: u16,
) -> Result<TcpStream> {
    use tokio_socks::tcp::Socks5Stream;
    let target = (host, port);
    let s = if let (Some(u), Some(p)) = (&upstream.user, &upstream.pass) {
        Socks5Stream::connect_with_password_and_socket(stream, target, u, p).await?
    } else {
        Socks5Stream::connect_with_socket(stream, target).await?
    };
    Ok(s.into_inner())
}

/// Pick an upstream and connect through to `target`, automatically rotating
/// the bound proxy on failure (for sticky sessions). Returns the connected
/// stream + the proxy that succeeded.
pub async fn dial_with_retry(
    state: &Arc<AppState>,
    parsed: &ParsedAuth,
    host: &str,
    port: u16,
) -> Result<(TcpStream, Arc<Proxy>)> {
    let max_attempts = match parsed.strategy {
        Strategy::Sticky { .. } => 3,
        Strategy::Random => 1,
    };
    let mut last_err: anyhow::Error = anyhow!("no upstream available");
    for attempt in 0..max_attempts {
        let chosen = if attempt == 0 {
            state.selector.pick_for(parsed)
        } else {
            match &parsed.strategy {
                Strategy::Sticky { session_key, ttl } => {
                    state.selector.rotate(session_key, *ttl)
                }
                Strategy::Random => break,
            }
        };
        let chosen = match chosen {
            Some(p) => p,
            None => break,
        };
        match connect_through(&chosen, host, port).await {
            Ok(s) => return Ok((s, chosen)),
            Err(e) => {
                tracing::warn!(
                    "upstream {} failed (attempt {}/{}): {e:#}",
                    chosen.endpoint(),
                    attempt + 1,
                    max_attempts
                );
                last_err = e;
            }
        }
    }
    Err(last_err)
}
