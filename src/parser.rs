//! Parsing of proxy text formats and the `username` strategy DSL.

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    Http,
    Socks5,
}

impl Scheme {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "http" | "https" => Some(Scheme::Http),
            "socks" | "socks5" | "socks5h" => Some(Scheme::Socks5),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedProxy {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub pass: Option<String>,
}

/// Parse a single proxy line. Accepts:
///   - `[scheme://]ip:port`
///   - `[scheme://]ip:port:user:pass`
///   - `[scheme://]user:pass@ip:port`
///   - `[scheme://]ip:port@user:pass`
pub fn parse_proxy_line(input: &str) -> Result<ParsedProxy> {
    let line = input.trim();
    if line.is_empty() {
        return Err(anyhow!("empty line"));
    }

    let (scheme, rest) = match line.split_once("://") {
        Some((s, r)) => (Scheme::parse(s).unwrap_or(Scheme::Http), r),
        None => (Scheme::Http, line),
    };
    let rest = rest.trim().trim_end_matches('/');

    // Try `user:pass@host:port`
    if let Some((auth, hp)) = split_last_at(rest) {
        if let Some((u, p)) = auth.split_once(':') {
            let (host, port) = parse_host_port(hp)?;
            return Ok(ParsedProxy {
                scheme,
                host,
                port,
                user: Some(u.to_string()),
                pass: Some(p.to_string()),
            });
        }
        // `host:port@user:pass` (legacy / our admin export)
        if let Some((u, p)) = hp.split_once(':') {
            let (host, port) = parse_host_port(auth)?;
            return Ok(ParsedProxy {
                scheme,
                host,
                port,
                user: Some(u.to_string()),
                pass: Some(p.to_string()),
            });
        }
    }

    // No '@'. Try `ip:port:user:pass` or `ip:port`.
    let parts: Vec<&str> = rest.split(':').collect();
    match parts.len() {
        2 => {
            let port: u16 = parts[1]
                .parse()
                .map_err(|_| anyhow!("bad port: {}", parts[1]))?;
            Ok(ParsedProxy {
                scheme,
                host: parts[0].to_string(),
                port,
                user: None,
                pass: None,
            })
        }
        4 => {
            let port: u16 = parts[1]
                .parse()
                .map_err(|_| anyhow!("bad port: {}", parts[1]))?;
            Ok(ParsedProxy {
                scheme,
                host: parts[0].to_string(),
                port,
                user: Some(parts[2].to_string()),
                pass: Some(parts[3].to_string()),
            })
        }
        _ => Err(anyhow!("unrecognized proxy format: {input}")),
    }
}

fn split_last_at(s: &str) -> Option<(&str, &str)> {
    let i = s.rfind('@')?;
    Some((&s[..i], &s[i + 1..]))
}

fn parse_host_port(s: &str) -> Result<(String, u16)> {
    let (h, p) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("missing :port in {s}"))?;
    let port: u16 = p.parse().map_err(|_| anyhow!("bad port: {p}"))?;
    Ok((h.to_string(), port))
}

// ────────────────────────────────────────────────────────────────────────────
// Auth / username DSL
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Strategy {
    /// Random per connection.
    Random,
    /// Stick a session_key to an upstream proxy.
    /// `ttl = None` means no auto-expiry — the binding only changes when the
    /// upstream errors out (auto-rotate) or the proxy is removed/disabled.
    Sticky {
        session_key: String,
        ttl: Option<Duration>,
    },
}

#[derive(Debug, Clone)]
pub struct ParsedAuth {
    pub strategy: Strategy,
    /// Effective base username (after stripping prefixes).
    pub base_user: String,
    /// Optional region tag (e.g. region-us-…). Future use.
    pub region: Option<String>,
}

/// Parse the username sent by the client. Only the password is verified
/// upstream — the username is purely a routing/session hint.
///
/// Rules (in order):
///   1. Username starts with `time-<N>[unit]-…` → sticky for N (auto-expire),
///      keyed by the full username.
///   2. Username is exactly the master user → Random per connection.
///   3. Anything else → long-term sticky session (no expiry, only rotates on
///      upstream error), keyed by the full username.
///
/// Examples (assume master user = `user`):
///   * `user`                  → Random
///   * `time-5-user`           → Sticky 5min, key="time-5-user"
///   * `time-30m-anything`     → Sticky 30min, key="time-30m-anything"
///   * `myapp1`                → long-term session "myapp1"
///   * `session-abc-user`      → long-term session "session-abc-user"
///   * `8a92f1e0`              → long-term session "8a92f1e0" (any string)
pub fn parse_auth(username: &str, master_user: &str) -> ParsedAuth {
    // Try to peel `time-<N>[unit]` prefix.
    let parts: Vec<&str> = username.splitn(3, '-').collect();
    let sticky_ttl: Option<Duration> = if parts.len() >= 2 && parts[0] == "time" {
        parse_duration(parts[1])
    } else {
        None
    };

    let strategy = if let Some(ttl) = sticky_ttl {
        Strategy::Sticky {
            session_key: username.to_string(),
            ttl: Some(ttl),
        }
    } else if username == master_user {
        Strategy::Random
    } else {
        // Any other arbitrary username — treat as a self-assigned session id.
        Strategy::Sticky {
            session_key: username.to_string(),
            ttl: None,
        }
    };

    ParsedAuth {
        strategy,
        base_user: username.to_string(),
        region: None,
    }
}

fn parse_duration(token: &str) -> Option<Duration> {
    if token.is_empty() {
        return None;
    }
    let (num_str, unit_secs) = if let Some(stripped) = token.strip_suffix('s') {
        (stripped, 1u64)
    } else if let Some(stripped) = token.strip_suffix('m') {
        (stripped, 60u64)
    } else if let Some(stripped) = token.strip_suffix('h') {
        (stripped, 3600u64)
    } else {
        (token, 60u64) // bare number = minutes
    };
    let n: u64 = num_str.parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(Duration::from_secs(n * unit_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_formats() {
        let p = parse_proxy_line("1.2.3.4:8080").unwrap();
        assert_eq!(p.host, "1.2.3.4");
        assert_eq!(p.port, 8080);
        assert!(p.user.is_none());

        let p = parse_proxy_line("1.2.3.4:8080:foo:bar").unwrap();
        assert_eq!(p.user.as_deref(), Some("foo"));
        assert_eq!(p.pass.as_deref(), Some("bar"));

        let p = parse_proxy_line("foo:bar@1.2.3.4:8080").unwrap();
        assert_eq!(p.host, "1.2.3.4");
        assert_eq!(p.port, 8080);

        let p = parse_proxy_line("socks5://1.2.3.4:1080").unwrap();
        assert_eq!(p.scheme, Scheme::Socks5);
    }

    #[test]
    fn parse_auth_random() {
        let a = parse_auth("user", "user");
        assert!(matches!(a.strategy, Strategy::Random));
    }

    #[test]
    fn parse_auth_time() {
        let a = parse_auth("time-5-user", "user");
        match a.strategy {
            Strategy::Sticky { ttl: Some(d), .. } => assert_eq!(d.as_secs(), 300),
            _ => panic!(),
        }
        let a = parse_auth("time-30s-anything", "user");
        match a.strategy {
            Strategy::Sticky { ttl: Some(d), .. } => assert_eq!(d.as_secs(), 30),
            _ => panic!(),
        }
    }

    #[test]
    fn parse_auth_arbitrary_session() {
        for name in ["myapp1", "session-abc-user", "8a92f1e0", "abc-def-ghi"] {
            let a = parse_auth(name, "user");
            match a.strategy {
                Strategy::Sticky { ttl: None, session_key } => {
                    assert_eq!(session_key, name);
                }
                _ => panic!("expected long-term sticky for {name}"),
            }
        }
    }
}
