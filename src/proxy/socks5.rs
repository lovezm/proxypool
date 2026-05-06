//! Inbound SOCKS5 server (CONNECT only).
//!
//! Username/Password auth (RFC 1929) is required — that is how the client
//! tells us which strategy to use (e.g. `time-5-user`).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::parser::parse_auth;
use crate::proxy::upstream;
use crate::AppState;

const VER: u8 = 0x05;
const AUTH_USERPASS: u8 = 0x02;
const AUTH_NO_ACCEPTABLE: u8 = 0xFF;

const CMD_CONNECT: u8 = 0x01;

const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

const REP_SUCCESS: u8 = 0x00;
const REP_NETWORK_UNREACHABLE: u8 = 0x03;
const REP_HOST_UNREACHABLE: u8 = 0x04;
const REP_CMD_NOT_SUPPORTED: u8 = 0x07;

pub async fn handle(state: Arc<AppState>, mut sock: TcpStream, peer: SocketAddr) -> Result<()> {
    // Greeting: VER NMETHODS METHODS…
    let mut hdr = [0u8; 2];
    sock.read_exact(&mut hdr).await?;
    if hdr[0] != VER {
        bail!("unsupported SOCKS version: {}", hdr[0]);
    }
    let n = hdr[1] as usize;
    let mut methods = vec![0u8; n];
    sock.read_exact(&mut methods).await?;

    if !methods.contains(&AUTH_USERPASS) {
        sock.write_all(&[VER, AUTH_NO_ACCEPTABLE]).await?;
        return Ok(());
    }
    sock.write_all(&[VER, AUTH_USERPASS]).await?;

    // RFC 1929: VER ULEN UNAME PLEN PASSWD
    let mut auth_hdr = [0u8; 2];
    sock.read_exact(&mut auth_hdr).await?;
    if auth_hdr[0] != 0x01 {
        bail!("bad userpass auth ver: {}", auth_hdr[0]);
    }
    let ulen = auth_hdr[1] as usize;
    let mut uname = vec![0u8; ulen];
    sock.read_exact(&mut uname).await?;
    let mut plen_buf = [0u8; 1];
    sock.read_exact(&mut plen_buf).await?;
    let plen = plen_buf[0] as usize;
    let mut passwd = vec![0u8; plen];
    sock.read_exact(&mut passwd).await?;

    let username = String::from_utf8(uname).map_err(|_| anyhow!("non-utf8 username"))?;
    let password = String::from_utf8(passwd).map_err(|_| anyhow!("non-utf8 password"))?;

    let (_cfg_u, cfg_p) = state.auth();
    // Only verify the password; the username is a free-form session hint.
    if password != cfg_p {
        sock.write_all(&[0x01, 0x01]).await?; // auth fail
        return Ok(());
    }
    sock.write_all(&[0x01, 0x00]).await?; // auth ok

    // Request: VER CMD RSV ATYP DST.ADDR DST.PORT
    let mut req = [0u8; 4];
    sock.read_exact(&mut req).await?;
    if req[0] != VER {
        bail!("bad SOCKS5 request version");
    }
    if req[1] != CMD_CONNECT {
        reply(&mut sock, REP_CMD_NOT_SUPPORTED).await?;
        return Ok(());
    }
    let host = match req[3] {
        ATYP_IPV4 => {
            let mut a = [0u8; 4];
            sock.read_exact(&mut a).await?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        ATYP_IPV6 => {
            let mut a = [0u8; 16];
            sock.read_exact(&mut a).await?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            sock.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            sock.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| anyhow!("non-utf8 domain"))?
        }
        other => bail!("unsupported ATYP: {other}"),
    };
    let mut port_buf = [0u8; 2];
    sock.read_exact(&mut port_buf).await?;
    let port = u16::from_be_bytes(port_buf);

    let (master_user, _) = state.auth();
    let parsed = parse_auth(&username, &master_user);

    let (upstream_stream, upstream_proxy) =
        match upstream::dial_with_retry(&state, &parsed, &host, port).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("dial {host}:{port} failed: {e:#}");
                let rep = if e.to_string().to_lowercase().contains("network") {
                    REP_NETWORK_UNREACHABLE
                } else {
                    REP_HOST_UNREACHABLE
                };
                reply(&mut sock, rep).await?;
                return Ok(());
            }
        };

    tracing::debug!(
        "{peer} SOCKS5 → {host}:{port} via #{} {}",
        upstream_proxy.id,
        upstream_proxy.endpoint()
    );

    reply(&mut sock, REP_SUCCESS).await?;

    let mut client = sock;
    let mut server = upstream_stream;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
    Ok(())
}

async fn reply(sock: &mut TcpStream, rep: u8) -> Result<()> {
    sock.write_all(&[VER, rep, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}
