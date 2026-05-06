pub mod http;
pub mod listener;
pub mod socks5;
pub mod upstream;

use std::sync::Arc;

use anyhow::Result;

use crate::AppState;

pub async fn serve(state: Arc<AppState>, addr: String) -> Result<()> {
    listener::run(state, addr).await
}
