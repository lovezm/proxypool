//! Picking the upstream proxy: sticky sessions + random / sequential extract.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rand::seq::SliceRandom;

use crate::parser::{ParsedAuth, Strategy};
use crate::store::{Proxy, ProxyStore};

#[derive(Clone)]
struct Session {
    proxy_id: i64,
    /// `None` means no auto-expiry (long-term session).
    expire_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum ExtractOrder {
    #[default]
    Random,
    /// Round-robin by proxy id, persistent cursor across calls.
    Sequential,
}

pub struct Selector {
    store: Arc<ProxyStore>,
    sessions: DashMap<String, Session>,
    /// Last proxy id returned by sequential extract. -1 = no prior pick.
    seq_last_id: AtomicI64,
}

impl Selector {
    pub fn new(store: Arc<ProxyStore>) -> Self {
        Self {
            store,
            sessions: DashMap::new(),
            seq_last_id: AtomicI64::new(-1),
        }
    }

    pub fn sessions_count(&self) -> usize {
        self.sessions.len()
    }

    /// Drop expired sticky sessions. Long-term (`expire_at = None`) sessions
    /// are kept indefinitely.
    pub fn gc(&self) {
        let now = Instant::now();
        self.sessions.retain(|_, s| match s.expire_at {
            Some(e) => e > now,
            None => true,
        });
    }

    /// Pick a proxy for an inbound connection.
    pub fn pick_for(&self, auth: &ParsedAuth) -> Option<Arc<Proxy>> {
        match &auth.strategy {
            Strategy::Sticky { session_key, ttl } => self.pick_sticky(session_key, *ttl),
            Strategy::Random => self.pick_random(),
        }
    }

    fn pick_sticky(&self, key: &str, ttl: Option<Duration>) -> Option<Arc<Proxy>> {
        let now = Instant::now();
        if let Some(s) = self.sessions.get(key) {
            let alive_session = match s.expire_at {
                Some(e) => e > now,
                None => true,
            };
            if alive_session {
                if let Some(p) = self.store.get(s.proxy_id) {
                    if p.enabled.load(Ordering::Relaxed) && p.alive.load(Ordering::Relaxed) {
                        return Some(p);
                    }
                }
            }
        }
        let pick = self.pick_random()?;
        self.sessions.insert(
            key.to_string(),
            Session {
                proxy_id: pick.id,
                expire_at: ttl.map(|d| now + d),
            },
        );
        Some(pick)
    }

    /// Force-evict a session and pick a fresh proxy. Excludes the previously
    /// bound proxy id so we don't immediately re-pick the failing one. Used
    /// after an upstream error to auto-rotate.
    pub fn rotate(&self, key: &str, ttl: Option<Duration>) -> Option<Arc<Proxy>> {
        let prev = self.sessions.get(key).map(|s| s.proxy_id);
        let now = Instant::now();
        let live: Vec<Arc<Proxy>> = self
            .store
            .live()
            .into_iter()
            .filter(|p| Some(p.id) != prev)
            .collect();
        let pick = if live.is_empty() {
            // Pool too small — fall back to whole live set.
            self.pick_random()?
        } else {
            let mut rng = rand::thread_rng();
            live.choose(&mut rng).cloned()?
        };
        self.sessions.insert(
            key.to_string(),
            Session {
                proxy_id: pick.id,
                expire_at: ttl.map(|d| now + d),
            },
        );
        Some(pick)
    }

    fn pick_random(&self) -> Option<Arc<Proxy>> {
        let live = self.store.live();
        if live.is_empty() {
            return None;
        }
        let mut rng = rand::thread_rng();
        live.choose(&mut rng).cloned()
    }

    /// Extract up to `count` distinct proxies for the API endpoint.
    ///
    /// * `cooldown_secs` — minimum seconds between two `extract` calls
    ///   returning the same proxy. `0` disables the cooldown. Applies
    ///   globally across all clients.
    /// * `order` — `Random` (shuffle each call) or `Sequential` (round-robin
    ///   by id, cursor persists between calls).
    ///
    /// Returns fewer than `count` if the pool can't satisfy the constraints.
    pub fn extract(
        &self,
        count: usize,
        cooldown_secs: u64,
        order: ExtractOrder,
    ) -> Vec<Arc<Proxy>> {
        if count == 0 {
            return Vec::new();
        }
        let now_ms = unix_ms();
        let cd_ms = cooldown_secs.saturating_mul(1000);

        // Live pool sorted by id (so sequential mode is deterministic; random
        // mode reorders below anyway).
        let mut live = self.store.live();
        if live.is_empty() {
            return Vec::new();
        }
        live.sort_by_key(|p| p.id);

        let candidates: Vec<Arc<Proxy>> = live
            .into_iter()
            .filter(|p| {
                if cd_ms == 0 {
                    return true;
                }
                let last = p.last_extracted_ms.load(Ordering::Relaxed);
                now_ms.saturating_sub(last) >= cd_ms
            })
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }

        let n = count.min(candidates.len());
        let chosen: Vec<Arc<Proxy>> = match order {
            ExtractOrder::Random => {
                let mut shuffled = candidates;
                shuffled.shuffle(&mut rand::thread_rng());
                shuffled.into_iter().take(n).collect()
            }
            ExtractOrder::Sequential => {
                // Walk through `candidates` (already sorted by id) starting
                // from the smallest id strictly greater than the last
                // returned. Wraps around at the end.
                let last = self.seq_last_id.load(Ordering::Relaxed);
                let start_idx = candidates
                    .iter()
                    .position(|p| p.id > last)
                    .unwrap_or(0);
                let len = candidates.len();
                let out: Vec<Arc<Proxy>> = (0..n)
                    .map(|i| candidates[(start_idx + i) % len].clone())
                    .collect();
                if let Some(p) = out.last() {
                    self.seq_last_id.store(p.id, Ordering::Relaxed);
                }
                out
            }
        };

        // Stamp the cooldown timestamp on each picked proxy.
        for p in &chosen {
            p.last_extracted_ms.store(now_ms, Ordering::Relaxed);
        }
        chosen
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
