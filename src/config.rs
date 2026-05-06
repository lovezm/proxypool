use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Admin / API bind address (default 127.0.0.1).
    #[serde(default = "default_admin_bind")]
    pub admin_bind: String,
    /// Admin / API port (default 8080).
    #[serde(default = "default_admin_port")]
    pub admin_port: u16,
    /// Proxy listener bind address (default 0.0.0.0).
    #[serde(default = "default_proxy_bind")]
    pub proxy_bind: String,
    /// Proxy listener port (default 11077).
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
    /// SQLite database path.
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// Master proxy auth credentials.
    pub auth: AuthConfig,
    /// Admin panel auth (password only).
    #[serde(default = "default_admin_auth")]
    pub admin_auth: AdminAuthConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminAuthConfig {
    pub password: String,
}

fn default_admin_auth() -> AdminAuthConfig {
    AdminAuthConfig {
        password: "ergou123".into(),
    }
}

fn default_admin_bind() -> String {
    "127.0.0.1".to_string()
}
fn default_admin_port() -> u16 {
    11078
}
fn default_proxy_bind() -> String {
    "0.0.0.0".to_string()
}
fn default_proxy_port() -> u16 {
    11077
}
fn default_db_path() -> String {
    "data/proxies.db".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            admin_bind: default_admin_bind(),
            admin_port: default_admin_port(),
            proxy_bind: default_proxy_bind(),
            proxy_port: default_proxy_port(),
            db_path: default_db_path(),
            auth: AuthConfig {
                username: "user".into(),
                password: "pass".into(),
            },
            admin_auth: default_admin_auth(),
        }
    }
}

impl Config {
    pub fn load_or_init<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            let txt = std::fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            let cfg: Config = toml_lite::from_str(&txt)
                .with_context(|| format!("parse config {}", path.display()))?;
            return Ok(cfg);
        }
        let cfg = Config::default();
        let txt = toml_lite::to_string(&cfg);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        std::fs::write(path, txt)
            .with_context(|| format!("write config {}", path.display()))?;
        if let Some(parent) = std::path::Path::new(&cfg.db_path).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        Ok(cfg)
    }
}

/// Tiny TOML reader/writer for our small flat config — we only have primitives
/// and one nested `[auth]` table, so we avoid pulling in a full TOML crate.
mod toml_lite {
    use super::Config;
    use anyhow::{anyhow, Result};

    pub fn from_str(s: &str) -> Result<Config> {
        let mut section = String::new();
        let mut admin_bind = None::<String>;
        let mut admin_port = None::<u16>;
        let mut proxy_bind = None::<String>;
        let mut proxy_port = None::<u16>;
        let mut db = None::<String>;
        let mut user = None::<String>;
        let mut pass = None::<String>;
        let mut admin_password = None::<String>;

        for raw in s.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_string();
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| anyhow!("invalid line: {raw}"))?;
            let k = k.trim();
            let v = v.trim().trim_matches('"');
            match (section.as_str(), k) {
                ("", "admin_bind") => admin_bind = Some(v.into()),
                ("", "admin_port") => admin_port = Some(v.parse()?),
                ("", "proxy_bind") => proxy_bind = Some(v.into()),
                ("", "proxy_port") => proxy_port = Some(v.parse()?),
                ("", "db_path") => db = Some(v.into()),
                ("auth", "username") => user = Some(v.into()),
                ("auth", "password") => pass = Some(v.into()),
                ("admin_auth", "password") => admin_password = Some(v.into()),
                _ => {}
            }
        }
        let def = Config::default();
        Ok(Config {
            admin_bind: admin_bind.unwrap_or(def.admin_bind),
            admin_port: admin_port.unwrap_or(def.admin_port),
            proxy_bind: proxy_bind.unwrap_or(def.proxy_bind),
            proxy_port: proxy_port.unwrap_or(def.proxy_port),
            db_path: db.unwrap_or(def.db_path),
            auth: super::AuthConfig {
                username: user.unwrap_or(def.auth.username),
                password: pass.unwrap_or(def.auth.password),
            },
            admin_auth: super::AdminAuthConfig {
                password: admin_password.unwrap_or(def.admin_auth.password),
            },
        })
    }

    pub fn to_string(c: &Config) -> String {
        format!(
            "# proxy-gateway config\n\
             admin_bind = \"{}\"\n\
             admin_port = {}\n\
             proxy_bind = \"{}\"\n\
             proxy_port = {}\n\
             db_path = \"{}\"\n\
             \n[auth]\n\
             username = \"{}\"\n\
             password = \"{}\"\n\
             \n[admin_auth]\n\
             password = \"{}\"\n",
            c.admin_bind,
            c.admin_port,
            c.proxy_bind,
            c.proxy_port,
            c.db_path,
            c.auth.username,
            c.auth.password,
            c.admin_auth.password,
        )
    }
}
