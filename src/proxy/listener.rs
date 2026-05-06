//! Multi-protocol listener on the proxy port.
//!
//! We peek the first byte of every accepted connection:
//!   * 0x05  → SOCKS5
//!   * else  → HTTP proxy (CONNECT or forwarded request)

use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;

use crate::proxy::{http, socks5};
use crate::AppState;

pub async fn run(state: Arc<AppState>, addr: String) -> Result<()> {
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("proxy listening on {addr}");
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("accept error: {e}");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _ = sock.set_nodelay(true);

            let mut head = [0u8; 1];
            match sock.peek(&mut head).await {
                Ok(0) => return,
                Err(e) => {
                    tracing::debug!("peek failed from {peer}: {e}");
                    return;
                }
                _ => {}
            }
            let res = if head[0] == 0x05 {
                socks5::handle(state, sock, peer).await
            } else {
                http::handle(state, sock, peer).await
            };
            if let Err(e) = res {
                tracing::debug!("session {peer} ended: {e:#}");
            }
        });
    }
}

