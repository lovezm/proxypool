mod admin;
mod config;
mod health;
mod parser;
mod proxy;
mod selector;
mod store;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(name = "proxy-gateway", version)]
struct Cli {
    /// Path to config file (TOML).
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

pub struct AppState {
    pub config: parking_lot::RwLock<config::Config>,
    pub store: Arc<store::ProxyStore>,
    pub selector: Arc<selector::Selector>,
    pub admin_tokens: dashmap::DashMap<String, ()>,
}

impl AppState {
    pub fn auth(&self) -> (String, String) {
        let c = self.config.read();
        (c.auth.username.clone(), c.auth.password.clone())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "proxy_gateway=info,tower_http=warn".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    let cfg = config::Config::load_or_init(&cli.config)?;
    tracing::info!(?cfg, "loaded config");

    let store = Arc::new(store::ProxyStore::open(&cfg.db_path).await?);
    store.reload().await?;
    tracing::info!(count = store.len(), "proxies loaded");

    let selector = Arc::new(selector::Selector::new(store.clone()));

    let state = Arc::new(AppState {
        config: parking_lot::RwLock::new(cfg.clone()),
        store: store.clone(),
        selector: selector.clone(),
        admin_tokens: dashmap::DashMap::new(),
    });

    let admin_addr = format!("{}:{}", cfg.admin_bind, cfg.admin_port);
    let proxy_addr = format!("{}:{}", cfg.proxy_bind, cfg.proxy_port);

    let admin = tokio::spawn(admin::serve(state.clone(), admin_addr));
    let proxy = tokio::spawn(proxy::serve(state.clone(), proxy_addr));

    {
        let s = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                s.selector.gc();
            }
        });
    }

    // Auto-revive proxies that have been disabled longer than the grace period.
    {
        let s = state.clone();
        let grace_secs: u64 = 12 * 3600;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                match s.store.auto_enable_expired(grace_secs).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(
                        "auto-enabled {n} proxy(ies) (disabled >{grace_secs}s)"
                    ),
                    Err(e) => tracing::warn!("auto-enable scan failed: {e:#}"),
                }
            }
        });
    }

    tokio::select! {
        r = admin => { r??; }
        r = proxy => { r??; }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("ctrl-c received, shutting down");
        }
    }
    Ok(())
}
