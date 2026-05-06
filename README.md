# Proxy Gateway · 静态代理池网关

> [English](README_EN.md) · 中文（当前）

把一批静态代理（HTTP / HTTPS / SOCKS5）汇聚成一个固定入口，对外提供：

- **统一隧道**：客户端只配 `host:11077`，由网关按用户名语义自动选 / 换 IP
- **API 提取**：`GET /api/extract`，按需返回纯文本代理清单（直连，不过网关）
- **Web 管理面板**：`http://host:11078`，导入 / 启停 / 测速 / 一键生成调用示例

基于 Rust + Tokio 异步实现，单端口同时识别 HTTP CONNECT、HTTP 转发、SOCKS5 三种协议，单机轻松支撑数千并发。

---

## 功能

- ✅ 一键启动，零外部依赖（SQLite 单文件持久化）
- ✅ 文本批量导入：`IP:PORT` / `IP:PORT:U:P` / `U:P@IP:PORT`，可加 `http://` / `socks5://` 前缀
- ✅ 隧道入口同时支持 **HTTP / HTTPS / SOCKS5**（`:11077` 自动协议探测）
- ✅ 三种取 IP 策略，由 username 语法切换：
  - 每连接随机
  - `time-N-…` → N 分钟同 IP，到期自动换
  - 任意自定义字符串 → **长效会话**，IP 不变，失败时自动轮换
- ✅ API 提取支持四种文本格式 + 协议过滤
- ✅ 内置测速：通过每个代理 CONNECT 到 `apple.com:443` 测延迟，支持单条 / 全量并发
- ✅ Web 管理面板，含登录、登出、密码加密保护
- ✅ Basic Auth 同时支持（方便 curl 脚本）
- ✅ 上游连接出错对 sticky 会话自动重试 + 轮换最多 3 次

---

## 架构

```
                    ┌──────────────────────────────────┐
                    │  管理 UI + REST API  :11078      │ ← 登录、导入、测速、提取
                    └──────────────┬───────────────────┘
                                   │
                    ┌──────────────▼───────────────────┐
                    │   共享代理池 (内存 + SQLite)     │
                    │   - ArcSwap<Vec<Arc<Proxy>>>     │
                    │   - DashMap<sessionKey, ⇒proxy>  │
                    └──────────────┬───────────────────┘
                                   │
   客户端 ──► [:11077 多协议监听] ──► HTTP CONNECT | HTTP 转发 | SOCKS5
                                   │
                                   ▼
                          [认证 + 用户名解析]
                                   │
                                   ▼
                       [Selector：sticky / random]
                                   │
                                   ▼
                     [上游隧道 → 静态代理 → 目标站点]
```

---

## 快速开始

### 1. 构建

```bash
cargo build --release
```

可执行文件：`target/release/proxy-gateway` (Linux/macOS) 或 `proxy-gateway.exe` (Windows)。

### 2. 启动

```bash
./target/release/proxy-gateway
```

首次启动自动写入 `config.toml`（默认值见下文），并在 `data/proxies.db` 建表。

启动后：

| 入口 | 默认地址 | 默认凭证 |
|---|---|---|
| 隧道代理 | `0.0.0.0:11077` | `user` / `pass` |
| 管理面板 | `http://127.0.0.1:11078` | 密码 `ergou123` |

### 3. 添加代理

打开管理面板登录，"批量导入"框粘贴：

```
1.1.1.1:8080
2.2.2.2:8080:foo:bar
foo:bar@3.3.3.3:8080
socks5://4.4.4.4:1080:foo:bar
```

回车导入即可。

### 4. 使用

**作为 HTTP 代理：**
```bash
curl -x http://user:pass@127.0.0.1:11077 https://api.ipify.org
```

**5 分钟同 IP：**
```bash
curl -x http://time-5-user:pass@127.0.0.1:11077 https://api.ipify.org
```

**长效会话（自己定一个 id，绑定一个 IP，错了自动换）：**
```bash
curl -x http://my-session-1:pass@127.0.0.1:11077 https://api.ipify.org
```

**SOCKS5：**
```bash
curl -x socks5h://user:pass@127.0.0.1:11077 https://api.ipify.org
```

**API 提取：**
```bash
curl -u admin:ergou123 'http://127.0.0.1:11078/api/extract?count=10&format=user_pass_at_ip_port'
```

---

## 用户名语义（隧道模式）

> **只校验密码**。username 是路由 / 会话提示，可以是任意字符串。

| username 形式 | 行为 |
|---|---|
| `<主用户名>` （配置里那个） | 每连接随机选一条 |
| `time-5-anything` | 5 分钟内同一 IP，到期自动换 |
| `time-30s-anything` | 30 秒，单位支持 `s/m/h` |
| 其它任意字符串（如 `myapp1`、`8a92f1e0`） | 长效会话，绑定一条 IP，**失败自动轮换** |

会话 key 是整段 username 字符串。同名 username 共享同一会话。

---

## 配置

`config.toml`：

```toml
admin_bind   = "127.0.0.1"   # 管理面板监听地址
admin_port   = 11078         # 管理面板端口
proxy_bind   = "0.0.0.0"     # 隧道监听地址
proxy_port   = 11077         # 隧道端口
db_path      = "data/proxies.db"

[auth]
username = "user"            # 隧道代理主用户名
password = "pass"            # 隧道代理密码

[admin_auth]
password = "ergou123"        # 管理面板登录密码
```

也可在管理面板 → 认证/配置 中在线修改（admin 密码改后下次刷新需要重新登录）。

---

## REST API

> 除 `/api/health`、`/api/login` 外均需鉴权。鉴权方式三选一：
> - Cookie `pg_token=...` （登录后自动设置）
> - `Authorization: Bearer <token>`
> - `Authorization: Basic <base64(任意:面板密码)>`（兼容 curl）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET  | `/` | 管理面板 HTML |
| GET  | `/api/health` | 健康探活（公开）|
| POST | `/api/login` | `{ "password": "..." }` → 设 cookie + 返回 token |
| POST | `/api/logout` | 清 cookie + 失效 token |
| GET  | `/api/stats` | 总数 / 启用 / 存活 / 会话数 |
| GET  | `/api/proxies` | 全部代理列表 |
| POST | `/api/proxies` | 单条新增 `{line, tag?}` |
| POST | `/api/proxies/import` | 批量导入 `{text, tag?}` |
| DELETE | `/api/proxies` | 清空所有 |
| DELETE | `/api/proxies/:id` | 删除一条 |
| POST | `/api/proxies/:id/enable` \| `/disable` | 启停 |
| POST | `/api/proxies/:id/test` | 测速一条 (默认 apple.com:443) |
| POST | `/api/proxies/test_all` | 并发测速全部 (上限 32) |
| GET  | `/api/extract?count=N&format=...&protocol=...` | 提取 N 条，纯文本一行一条 |
| GET  | `/api/config` / PUT `/api/config` | 读 / 改配置 |

`format` 支持：

| 值 | 输出 |
|---|---|
| `user_pass_at_ip_port` *(默认)* | `user:pass@ip:port` |
| `ip_port_user_pass`              | `ip:port:user:pass` |
| `ip_port`                        | `ip:port` |
| `url`                            | `scheme://user:pass@ip:port` |

`protocol` 可选：`http` / `socks5`，留空表示全部。

---

## 隧道 vs API 提取

| | 隧道（:11077） | API 提取（:11078/api/extract） |
|---|---|---|
| 流量是否过网关 | **是**（双向转发） | 否（客户端直连）|
| 客户端复杂度 | 配一个固定地址即可 | 自己处理代理切换 |
| 适合场景 | 浏览器 / 不可编程的工具 / 长效会话 | 高并发爬虫 / 自管代理池 |
| 切换 IP 方式 | 改 username 即可 | 重新拉取 |

---

## 项目结构

```
src/
  main.rs          入口，并行起 admin + proxy 两个监听
  config.rs        TOML 配置加载 / 持久化
  store.rs         代理池（ArcSwap + SQLite）
  parser.rs        文本格式 + username DSL 解析
  selector.rs      sticky 会话 + 随机 + 错误轮换
  health.rs        测速（apple.com:443）
  proxy/
    listener.rs    :11077，按首字节分发协议
    http.rs        HTTP CONNECT + HTTP 转发
    socks5.rs      SOCKS5 服务端 (RFC1929)
    upstream.rs    通过静态代理建立 TCP 隧道 + 失败重试
  admin.rs         axum 路由 + Basic/Token 鉴权 + 嵌入 UI
assets/
  index.html       管理面板（include_str! 编译期嵌入）
```

---

## 许可

MIT
