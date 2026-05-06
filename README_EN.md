# Proxy Gateway · Static-Proxy Pool Frontend

> [中文](README.md) · English (current)

A single fixed entry-point in front of a pool of static proxies (HTTP / HTTPS / SOCKS5). Exposes:

- **Unified tunnel** — clients only need to dial `host:11077`; the gateway picks / rotates the upstream IP based on the username they supply.
- **API extract** — `GET /api/extract` returns a plain-text list of proxies for clients that want to connect upstream directly.
- **Web admin panel** — `http://host:11078` for import, enable/disable, latency testing, and one-click snippet generation.

Built in Rust + Tokio. A single port speaks HTTP CONNECT, plain-HTTP forwarding, and SOCKS5 simultaneously (auto-detected from the first byte). Single-machine throughput easily reaches several thousand concurrent tunnels.

---

## Features

- ✅ One binary, no external dependencies (SQLite single-file persistence).
- ✅ Bulk text import — `IP:PORT`, `IP:PORT:U:P`, `U:P@IP:PORT`, optionally prefixed with `http://` or `socks5://`.
- ✅ The proxy port (`:11077`) accepts **HTTP / HTTPS / SOCKS5** on the same socket.
- ✅ Three IP-selection strategies, switched purely via the client username:
  - per-connection random
  - `time-N-…` → same IP for N minutes, auto-rotates on expiry
  - any custom string → **long-lived session**: bound to one IP, auto-rotates on upstream failure.
- ✅ API extract supports four output formats and protocol filtering.
- ✅ Built-in latency test: dials `apple.com:443` through each proxy and reports ms (single + bulk parallel up to 32).
- ✅ Web admin panel with custom login form (no native browser pop-up).
- ✅ HTTP Basic Auth also accepted (handy for curl / scripts).
- ✅ Sticky-session upstream errors trigger automatic re-pick + retry (up to 3 attempts).

---

## Architecture

```
                    ┌──────────────────────────────────┐
                    │  Admin UI + REST API  :11078     │ ← login, import, test, extract
                    └──────────────┬───────────────────┘
                                   │
                    ┌──────────────▼───────────────────┐
                    │   Shared pool (mem + SQLite)     │
                    │   - ArcSwap<Vec<Arc<Proxy>>>     │
                    │   - DashMap<sessionKey, ⇒proxy>  │
                    └──────────────┬───────────────────┘
                                   │
   client ──► [:11077 multi-proto listener] ──► HTTP CONNECT | HTTP forward | SOCKS5
                                   │
                                   ▼
                          [auth + username parse]
                                   │
                                   ▼
                       [Selector: sticky / random]
                                   │
                                   ▼
                  [upstream tunnel → static proxy → target site]
```

---

## Quick Start

### 1. Build

```bash
cargo build --release
```

Binary: `target/release/proxy-gateway` (Linux/macOS) or `proxy-gateway.exe` (Windows).

### 2. Run

```bash
./target/release/proxy-gateway
```

On first launch, it writes a default `config.toml` and creates `data/proxies.db`.

| Endpoint | Default | Default credentials |
|---|---|---|
| Tunnel proxy | `0.0.0.0:11077` | `user` / `pass` |
| Admin panel  | `http://127.0.0.1:11078` | password `ergou123` |

### 3. Add proxies

Open the admin panel, log in, paste lines into "批量导入" (bulk import):

```
1.1.1.1:8080
2.2.2.2:8080:foo:bar
foo:bar@3.3.3.3:8080
socks5://4.4.4.4:1080:foo:bar
```

### 4. Use it

**Plain HTTP proxy:**
```bash
curl -x http://user:pass@127.0.0.1:11077 https://api.ipify.org
```

**Same IP for 5 minutes:**
```bash
curl -x http://time-5-user:pass@127.0.0.1:11077 https://api.ipify.org
```

**Long-lived session (any string you make up; auto-rotates on error):**
```bash
curl -x http://my-session-1:pass@127.0.0.1:11077 https://api.ipify.org
```

**SOCKS5:**
```bash
curl -x socks5h://user:pass@127.0.0.1:11077 https://api.ipify.org
```

**Extract via API:**
```bash
curl -u admin:ergou123 \
  'http://127.0.0.1:11078/api/extract?count=10&format=user_pass_at_ip_port'
```

---

## Username semantics (tunnel mode)

> **Only the password is verified.** The username is purely a routing / session hint — any string is accepted.

| Username | Behaviour |
|---|---|
| `<master_user>` (the one in the config) | Random pick per connection |
| `time-5-anything`                       | Same IP for 5 minutes, then auto-rotates |
| `time-30s-anything`                     | 30 seconds; supports `s` / `m` / `h` |
| anything else (e.g. `myapp1`, `8a92f1e0`) | Long-lived session, bound to one IP, **auto-rotates only on upstream failure** |

Session key is the entire username string. Same username = same session.

---

## Configuration

`config.toml`:

```toml
admin_bind   = "127.0.0.1"      # admin panel bind addr
admin_port   = 11078            # admin panel port
proxy_bind   = "0.0.0.0"        # tunnel bind addr
proxy_port   = 11077            # tunnel port
db_path      = "data/proxies.db"

[auth]
username = "user"               # tunnel master username
password = "pass"               # tunnel password

[admin_auth]
password = "ergou123"           # admin panel login password
```

Editable live via the admin panel → Auth/Config (changing the admin password forces re-login).

---

## REST API

> All endpoints require auth except `/api/health` and `/api/login`. Auth accepted via:
> - Cookie `pg_token=...` (set by `/api/login`)
> - `Authorization: Bearer <token>`
> - `Authorization: Basic <base64(any:adminPassword)>` (curl-friendly)

| Method | Path | Notes |
|---|---|---|
| GET  | `/` | Admin panel HTML |
| GET  | `/api/health` | Liveness probe (public) |
| POST | `/api/login` | `{ "password": "..." }` → sets cookie + returns token |
| POST | `/api/logout` | Clears cookie + invalidates token |
| GET  | `/api/stats` | total / enabled / alive / sessions |
| GET  | `/api/proxies` | list all |
| POST | `/api/proxies` | add one `{line, tag?}` |
| POST | `/api/proxies/import` | bulk `{text, tag?}` |
| DELETE | `/api/proxies` | wipe all |
| DELETE | `/api/proxies/:id` | delete one |
| POST | `/api/proxies/:id/enable` \| `/disable` | toggle |
| POST | `/api/proxies/:id/test` | latency test (default `apple.com:443`) |
| POST | `/api/proxies/test_all` | parallel latency test (concurrency 32) |
| GET  | `/api/extract?count=N&format=…&protocol=…` | plain-text list, one per line |
| GET  / PUT | `/api/config` | read / update config |

`format` values:

| value | output |
|---|---|
| `user_pass_at_ip_port` *(default)* | `user:pass@ip:port` |
| `ip_port_user_pass`                | `ip:port:user:pass` |
| `ip_port`                          | `ip:port` |
| `url`                              | `scheme://user:pass@ip:port` |

`protocol`: `http` or `socks5`, omit for any.

---

## Tunnel vs API extract

|  | Tunnel (`:11077`) | API extract (`:11078/api/extract`) |
|---|---|---|
| Traffic flows through gateway? | **Yes** (bidirectional copy) | No (client connects directly) |
| Client complexity | Just one fixed endpoint | Has to manage rotation itself |
| Fits | browsers / non-programmable tools / sticky workloads | high-concurrency scrapers / DIY pool consumers |
| How to switch IP | Change the username | Re-fetch |

---

## Project layout

```
src/
  main.rs          entry — spawns admin + proxy listeners in parallel
  config.rs        TOML load / persist
  store.rs         pool (ArcSwap + SQLite)
  parser.rs        text-format + username DSL parser
  selector.rs      sticky + random + error rotation
  health.rs        latency test (apple.com:443)
  proxy/
    listener.rs    :11077 — first-byte protocol detection
    http.rs        HTTP CONNECT + plain-HTTP forwarding
    socks5.rs      SOCKS5 server (RFC 1929)
    upstream.rs    dial a static proxy + retry loop
  admin.rs         axum routes + Basic/Token auth + embedded UI
assets/
  index.html       admin panel (include_str! at build time)
```

---

## License

MIT
