//! Latency / liveness testing through static proxies.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::time::timeout;

use crate::proxy::upstream;
use crate::store::Proxy;

const DEFAULT_TARGET_HOST: &str = "apple.com";
const DEFAULT_TARGET_PORT: u16 = 443;
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub id: i64,
    pub ok: bool,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// Test one proxy: open a tunnel through it to `target`, measure handshake
/// time. Updates the proxy's atomic latency / alive flags.
pub async fn test_one(p: &Arc<Proxy>, target_host: &str, target_port: u16) -> TestResult {
    let start = Instant::now();
    let res = timeout(
        TEST_TIMEOUT,
        upstream::connect_through(p, target_host, target_port),
    )
    .await;

    match res {
        Ok(Ok(_stream)) => {
            let elapsed = start.elapsed().as_millis() as u64;
            p.last_latency_ms.store(elapsed, Ordering::Relaxed);
            p.last_check_ms.store(unix_ms(), Ordering::Relaxed);
            p.alive.store(true, Ordering::Relaxed);
            TestResult {
                id: p.id,
                ok: true,
                latency_ms: elapsed,
                error: None,
            }
        }
        Ok(Err(e)) => {
            p.last_latency_ms.store(0, Ordering::Relaxed);
            p.last_check_ms.store(unix_ms(), Ordering::Relaxed);
            p.alive.store(false, Ordering::Relaxed);
            TestResult {
                id: p.id,
                ok: false,
                latency_ms: 0,
                error: Some(format!("{e:#}")),
            }
        }
        Err(_) => {
            p.last_latency_ms.store(0, Ordering::Relaxed);
            p.last_check_ms.store(unix_ms(), Ordering::Relaxed);
            p.alive.store(false, Ordering::Relaxed);
            TestResult {
                id: p.id,
                ok: false,
                latency_ms: 0,
                error: Some("timeout".into()),
            }
        }
    }
}

/// Test all live proxies in parallel (bounded concurrency).
pub async fn test_all(
    proxies: Vec<Arc<Proxy>>,
    target_host: &str,
    target_port: u16,
    parallelism: usize,
) -> Vec<TestResult> {
    use tokio::sync::Semaphore;
    let sem = Arc::new(Semaphore::new(parallelism.max(1)));
    let host = target_host.to_string();
    let mut handles = Vec::with_capacity(proxies.len());
    for p in proxies {
        let sem = sem.clone();
        let host = host.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            test_one(&p, &host, target_port).await
        }));
    }
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(r) = h.await {
            out.push(r);
        }
    }
    out
}

pub fn default_target() -> (&'static str, u16) {
    (DEFAULT_TARGET_HOST, DEFAULT_TARGET_PORT)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
