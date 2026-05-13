//! In-memory proxy pool backed by SQLite.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::parser::{ParsedProxy, Scheme};

#[derive(Debug)]
pub struct Proxy {
    pub id: i64,
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub tag: Option<String>,
    pub enabled: AtomicBool,
    pub alive: AtomicBool,
    /// Last latency measurement in ms (0 = never tested or last test failed).
    pub last_latency_ms: AtomicU64,
    /// Unix ms of last successful health check (0 = never).
    pub last_check_ms: AtomicU64,
    /// Unix ms when this proxy was disabled (0 = enabled, or pre-tracking).
    /// Used by the auto-recovery task to re-enable proxies that have been
    /// disabled longer than the grace period.
    pub disabled_at_ms: AtomicU64,
    /// Unix ms when this proxy was last returned by `/api/extract`. Volatile,
    /// not persisted. Used to enforce the cross-request cooldown.
    pub last_extracted_ms: AtomicU64,
    pub created_at: i64,
}

impl Proxy {
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn snapshot(&self) -> ProxyView {
        ProxyView {
            id: self.id,
            scheme: self.scheme,
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            pass: self.pass.clone(),
            tag: self.tag.clone(),
            enabled: self.enabled.load(Ordering::Relaxed),
            alive: self.alive.load(Ordering::Relaxed),
            last_latency_ms: self.last_latency_ms.load(Ordering::Relaxed),
            last_check_ms: self.last_check_ms.load(Ordering::Relaxed),
            disabled_at_ms: self.disabled_at_ms.load(Ordering::Relaxed),
            last_extracted_ms: self.last_extracted_ms.load(Ordering::Relaxed),
            created_at: self.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyView {
    pub id: i64,
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub tag: Option<String>,
    pub enabled: bool,
    pub alive: bool,
    pub last_latency_ms: u64,
    pub last_check_ms: u64,
    pub disabled_at_ms: u64,
    pub last_extracted_ms: u64,
    pub created_at: i64,
}

pub struct ProxyStore {
    pool: SqlitePool,
    /// Snapshot of the live pool. We swap the whole Arc<Vec<…>> on changes.
    snapshot: ArcSwap<Vec<Arc<Proxy>>>,
}

impl ProxyStore {
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .with_context(|| format!("open sqlite at {}", path.display()))?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS proxies (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                scheme       TEXT NOT NULL,
                host         TEXT NOT NULL,
                port         INTEGER NOT NULL,
                user         TEXT,
                pass         TEXT,
                tag          TEXT,
                enabled      INTEGER NOT NULL DEFAULT 1,
                alive        INTEGER NOT NULL DEFAULT 1,
                disabled_at  INTEGER NOT NULL DEFAULT 0,
                created_at   INTEGER NOT NULL,
                UNIQUE(scheme, host, port, user, pass)
            )"#,
        )
        .execute(&pool)
        .await?;

        // Migration for older DBs (column may already exist — ignore the error).
        let _ = sqlx::query(
            "ALTER TABLE proxies ADD COLUMN disabled_at INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await;

        Ok(Self {
            pool,
            snapshot: ArcSwap::from_pointee(Vec::new()),
        })
    }

    pub async fn reload(&self) -> Result<()> {
        let rows = sqlx::query(
            "SELECT id, scheme, host, port, user, pass, tag, enabled, alive, \
                    disabled_at, created_at \
             FROM proxies ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;

        // Preserve volatile (in-memory only) atomics across reload.
        let prev: std::collections::HashMap<i64, (u64, u64, u64)> = self
            .snapshot
            .load()
            .iter()
            .map(|p| {
                (
                    p.id,
                    (
                        p.last_latency_ms.load(Ordering::Relaxed),
                        p.last_check_ms.load(Ordering::Relaxed),
                        p.last_extracted_ms.load(Ordering::Relaxed),
                    ),
                )
            })
            .collect();

        let mut list: Vec<Arc<Proxy>> = Vec::with_capacity(rows.len());
        for r in rows {
            let scheme: String = r.try_get("scheme")?;
            let scheme = Scheme::parse(&scheme).unwrap_or(Scheme::Http);
            let id: i64 = r.try_get("id")?;
            let (lat, chk, ext) = prev.get(&id).copied().unwrap_or((0, 0, 0));
            list.push(Arc::new(Proxy {
                id,
                scheme,
                host: r.try_get("host")?,
                port: r.try_get::<i64, _>("port")? as u16,
                user: r.try_get("user")?,
                pass: r.try_get("pass")?,
                tag: r.try_get("tag")?,
                enabled: AtomicBool::new(r.try_get::<i64, _>("enabled")? != 0),
                alive: AtomicBool::new(r.try_get::<i64, _>("alive")? != 0),
                last_latency_ms: AtomicU64::new(lat),
                last_check_ms: AtomicU64::new(chk),
                disabled_at_ms: AtomicU64::new(r.try_get::<i64, _>("disabled_at")? as u64),
                last_extracted_ms: AtomicU64::new(ext),
                created_at: r.try_get("created_at")?,
            }));
        }
        self.snapshot.store(Arc::new(list));
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.snapshot.load().len()
    }

    pub fn all(&self) -> Arc<Vec<Arc<Proxy>>> {
        self.snapshot.load_full()
    }

    pub fn get(&self, id: i64) -> Option<Arc<Proxy>> {
        self.snapshot
            .load()
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    /// Return all enabled & alive proxies (already cloned Arcs).
    pub fn live(&self) -> Vec<Arc<Proxy>> {
        self.snapshot
            .load()
            .iter()
            .filter(|p| p.enabled.load(Ordering::Relaxed) && p.alive.load(Ordering::Relaxed))
            .cloned()
            .collect()
    }

    /// Insert one proxy. Returns the new id, or `None` on duplicate.
    pub async fn insert(&self, p: &ParsedProxy, tag: Option<&str>) -> Result<Option<i64>> {
        let scheme = match p.scheme {
            Scheme::Http => "http",
            Scheme::Socks5 => "socks5",
        };
        let now = Utc::now().timestamp();
        let res = sqlx::query(
            "INSERT OR IGNORE INTO proxies (scheme, host, port, user, pass, tag, enabled, alive, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, 1, 1, ?)",
        )
        .bind(scheme)
        .bind(&p.host)
        .bind(p.port as i64)
        .bind(&p.user)
        .bind(&p.pass)
        .bind(tag)
        .bind(now)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Ok(None);
        }
        let id = res.last_insert_rowid();
        self.reload().await?;
        Ok(Some(id))
    }

    /// Bulk insert, returning (added, skipped_duplicates, parse_errors).
    pub async fn bulk_import(&self, text: &str, tag: Option<&str>) -> Result<ImportReport> {
        let mut added = 0u64;
        let mut dup = 0u64;
        let mut errs: Vec<String> = Vec::new();

        let now = Utc::now().timestamp();
        let mut tx = self.pool.begin().await?;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            match crate::parser::parse_proxy_line(trimmed) {
                Ok(p) => {
                    let scheme = match p.scheme {
                        Scheme::Http => "http",
                        Scheme::Socks5 => "socks5",
                    };
                    let r = sqlx::query(
                        "INSERT OR IGNORE INTO proxies (scheme, host, port, user, pass, tag, enabled, alive, created_at) \
                         VALUES (?, ?, ?, ?, ?, ?, 1, 1, ?)",
                    )
                    .bind(scheme)
                    .bind(&p.host)
                    .bind(p.port as i64)
                    .bind(&p.user)
                    .bind(&p.pass)
                    .bind(tag)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                    if r.rows_affected() > 0 {
                        added += 1;
                    } else {
                        dup += 1;
                    }
                }
                Err(e) => errs.push(format!("{trimmed}  ({e})")),
            }
        }
        tx.commit().await?;
        self.reload().await?;
        Ok(ImportReport {
            added,
            duplicates: dup,
            errors: errs,
        })
    }

    pub async fn delete(&self, id: i64) -> Result<bool> {
        let r = sqlx::query("DELETE FROM proxies WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        let removed = r.rows_affected() > 0;
        if removed {
            self.reload().await?;
        }
        Ok(removed)
    }

    pub async fn delete_all(&self) -> Result<u64> {
        let r = sqlx::query("DELETE FROM proxies").execute(&self.pool).await?;
        self.reload().await?;
        Ok(r.rows_affected())
    }

    pub async fn set_enabled(&self, id: i64, enabled: bool) -> Result<bool> {
        let now_ms = unix_ms();
        let stamp: i64 = if enabled { 0 } else { now_ms as i64 };
        let r = sqlx::query("UPDATE proxies SET enabled = ?, disabled_at = ? WHERE id = ?")
            .bind(if enabled { 1i64 } else { 0i64 })
            .bind(stamp)
            .bind(id)
            .execute(&self.pool)
            .await?;
        let updated = r.rows_affected() > 0;
        if updated {
            if let Some(p) = self.get(id) {
                p.enabled.store(enabled, Ordering::Relaxed);
                p.disabled_at_ms
                    .store(if enabled { 0 } else { now_ms }, Ordering::Relaxed);
            }
        }
        Ok(updated)
    }

    /// Enable / disable every proxy whose `host` matches `host`. Returns the
    /// number of rows affected.
    pub async fn set_enabled_by_host(&self, host: &str, enabled: bool) -> Result<u64> {
        let now_ms = unix_ms();
        let stamp: i64 = if enabled { 0 } else { now_ms as i64 };
        let r = sqlx::query(
            "UPDATE proxies SET enabled = ?, disabled_at = ? WHERE host = ?",
        )
        .bind(if enabled { 1i64 } else { 0i64 })
        .bind(stamp)
        .bind(host)
        .execute(&self.pool)
        .await?;
        let n = r.rows_affected();
        if n > 0 {
            for p in self.snapshot.load().iter() {
                if p.host == host {
                    p.enabled.store(enabled, Ordering::Relaxed);
                    p.disabled_at_ms
                        .store(if enabled { 0 } else { now_ms }, Ordering::Relaxed);
                }
            }
        }
        Ok(n)
    }

    /// Re-enable any proxy that has been disabled for longer than `grace`
    /// seconds. Returns the number of proxies brought back online.
    pub async fn auto_enable_expired(&self, grace_secs: u64) -> Result<u64> {
        let cutoff_ms = unix_ms().saturating_sub(grace_secs.saturating_mul(1000));
        let r = sqlx::query(
            "UPDATE proxies SET enabled = 1, disabled_at = 0 \
             WHERE enabled = 0 AND disabled_at > 0 AND disabled_at < ?",
        )
        .bind(cutoff_ms as i64)
        .execute(&self.pool)
        .await?;
        let n = r.rows_affected();
        if n > 0 {
            self.reload().await?;
        }
        Ok(n)
    }
}

fn unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub added: u64,
    pub duplicates: u64,
    pub errors: Vec<String>,
}
