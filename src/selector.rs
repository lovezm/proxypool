//! Picking the upstream proxy: sticky sessions + random.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

pub struct Selector {
    store: Arc<ProxyStore>,
    sessions: DashMap<String, Session>,
}

impl Selector {
    pub fn new(store: Arc<ProxyStore>) -> Self {
        Self {
            store,
            sessions: DashMap::new(),
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

    /// Extract up to `count` distinct random proxies for the API endpoint.
    /// If `count` exceeds the pool size, the whole pool is returned.
    pub fn extract(&self, count: usize) -> Vec<Arc<Proxy>> {
        if count == 0 {
            return Vec::new();
        }
        let mut live = self.store.live();
        if live.is_empty() {
            return Vec::new();
        }
        let mut rng = rand::thread_rng();
        live.shuffle(&mut rng);
        live.truncate(count);
        live
    }
}
