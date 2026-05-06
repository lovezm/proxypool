//! Admin/API HTTP server (axum). Also serves the embedded management UI.

use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Path, Query, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;

use crate::store::ProxyView;
use crate::AppState;

const INDEX_HTML: &str = include_str!("../assets/index.html");

pub async fn serve(state: Arc<AppState>, addr: String) -> Result<()> {
    let protected = Router::new()
        .route("/api/stats", get(stats))
        .route("/api/proxies", get(list_proxies).post(add_proxy).delete(clear_proxies))
        .route("/api/proxies/import", post(import_proxies))
        .route("/api/proxies/:id", delete(delete_proxy))
        .route("/api/proxies/:id/enable", post(enable_proxy))
        .route("/api/proxies/:id/disable", post(disable_proxy))
        .route("/api/proxies/:id/test", post(test_proxy))
        .route("/api/proxies/test_all", post(test_all_proxies))
        .route("/api/config", get(get_config).put(update_config))
        .route("/api/extract", get(extract))
        .route("/api/sessions", get(list_sessions))
        .route("/api/logout", post(logout))
        .layer(middleware::from_fn_with_state(state.clone(), require_admin_auth));

    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/login", post(login))
        .merge(protected)
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("admin listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct StatsResponse {
    total: usize,
    enabled: usize,
    alive: usize,
    sessions: usize,
}

async fn stats(State(s): State<Arc<AppState>>) -> Json<StatsResponse> {
    let all = s.store.all();
    let mut enabled = 0;
    let mut alive = 0;
    for p in all.iter() {
        if p.enabled.load(std::sync::atomic::Ordering::Relaxed) {
            enabled += 1;
        }
        if p.alive.load(std::sync::atomic::Ordering::Relaxed) {
            alive += 1;
        }
    }
    Json(StatsResponse {
        total: all.len(),
        enabled,
        alive,
        sessions: s.selector.sessions_count(),
    })
}

async fn list_proxies(State(s): State<Arc<AppState>>) -> Json<Vec<ProxyView>> {
    let all = s.store.all();
    Json(all.iter().map(|p| p.snapshot()).collect())
}

#[derive(Deserialize)]
struct AddProxyReq {
    line: String,
    tag: Option<String>,
}

async fn add_proxy(
    State(s): State<Arc<AppState>>,
    Json(req): Json<AddProxyReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let parsed = crate::parser::parse_proxy_line(&req.line)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let id = s
        .store
        .insert(&parsed, req.tag.as_deref())
        .await
        .map_err(ApiError::server)?;
    Ok(Json(serde_json::json!({
        "added": id.is_some(),
        "id": id,
    })))
}

#[derive(Deserialize)]
struct ImportReq {
    text: String,
    tag: Option<String>,
}

async fn import_proxies(
    State(s): State<Arc<AppState>>,
    Json(req): Json<ImportReq>,
) -> Result<Json<crate::store::ImportReport>, ApiError> {
    let report = s
        .store
        .bulk_import(&req.text, req.tag.as_deref())
        .await
        .map_err(ApiError::server)?;
    Ok(Json(report))
}

async fn delete_proxy(
    State(s): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ok = s.store.delete(id).await.map_err(ApiError::server)?;
    Ok(Json(serde_json::json!({ "deleted": ok })))
}

async fn clear_proxies(
    State(s): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let n = s.store.delete_all().await.map_err(ApiError::server)?;
    Ok(Json(serde_json::json!({ "deleted": n })))
}

async fn enable_proxy(
    State(s): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ok = s.store.set_enabled(id, true).await.map_err(ApiError::server)?;
    Ok(Json(serde_json::json!({ "ok": ok })))
}

async fn disable_proxy(
    State(s): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ok = s.store.set_enabled(id, false).await.map_err(ApiError::server)?;
    Ok(Json(serde_json::json!({ "ok": ok })))
}

#[derive(Serialize, Deserialize)]
struct ConfigView {
    admin_bind: String,
    admin_port: u16,
    proxy_bind: String,
    proxy_port: u16,
    auth_username: String,
    auth_password: String,
    admin_password: String,
}

async fn get_config(State(s): State<Arc<AppState>>) -> Json<ConfigView> {
    let c = s.config.read();
    Json(ConfigView {
        admin_bind: c.admin_bind.clone(),
        admin_port: c.admin_port,
        proxy_bind: c.proxy_bind.clone(),
        proxy_port: c.proxy_port,
        auth_username: c.auth.username.clone(),
        auth_password: c.auth.password.clone(),
        admin_password: c.admin_auth.password.clone(),
    })
}

#[derive(Deserialize)]
struct ConfigUpdate {
    auth_username: Option<String>,
    auth_password: Option<String>,
    admin_password: Option<String>,
}

async fn update_config(
    State(s): State<Arc<AppState>>,
    Json(req): Json<ConfigUpdate>,
) -> Json<serde_json::Value> {
    let mut c = s.config.write();
    if let Some(u) = req.auth_username {
        c.auth.username = u;
    }
    if let Some(p) = req.auth_password {
        c.auth.password = p;
    }
    if let Some(p) = req.admin_password {
        if !p.is_empty() {
            c.admin_auth.password = p;
        }
    }
    Json(serde_json::json!({ "ok": true }))
}

#[derive(Deserialize)]
struct ExtractParams {
    #[serde(default = "one")]
    count: usize,
    /// Output format: ip_port | ip_port_user_pass | user_pass_at_ip_port | url
    #[serde(default = "default_format")]
    format: String,
    /// Filter to a specific scheme: http | socks5
    protocol: Option<String>,
}

fn one() -> usize {
    1
}
fn default_format() -> String {
    "user_pass_at_ip_port".to_string()
}

async fn extract(
    State(s): State<Arc<AppState>>,
    Query(p): Query<ExtractParams>,
) -> Response {
    let count = p.count.clamp(1, 1000);
    let mut picks = s.selector.extract(count);

    if let Some(proto) = &p.protocol {
        let want = crate::parser::Scheme::parse(proto);
        picks.retain(|x| match want {
            Some(w) => x.scheme == w,
            None => true,
        });
    }
    if picks.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "no proxy available\n").into_response();
    }

    let mut body = String::new();
    for p_ in &picks {
        let line = format_proxy(p_, &p.format);
        body.push_str(&line);
        body.push('\n');
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(body.into())
        .unwrap()
}

#[derive(Deserialize, Default)]
struct TestParams {
    /// e.g. "apple.com" — defaults to apple.com
    host: Option<String>,
    /// defaults to 443
    port: Option<u16>,
}

async fn test_proxy(
    State(s): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<TestParams>,
) -> Result<Json<crate::health::TestResult>, ApiError> {
    let p = s
        .store
        .get(id)
        .ok_or_else(|| ApiError::bad_request(format!("no such proxy: {id}")))?;
    let (default_host, default_port) = crate::health::default_target();
    let host = q.host.unwrap_or_else(|| default_host.to_string());
    let port = q.port.unwrap_or(default_port);
    let r = crate::health::test_one(&p, &host, port).await;
    Ok(Json(r))
}

async fn test_all_proxies(
    State(s): State<Arc<AppState>>,
    Query(q): Query<TestParams>,
) -> Json<Vec<crate::health::TestResult>> {
    let (default_host, default_port) = crate::health::default_target();
    let host = q.host.unwrap_or_else(|| default_host.to_string());
    let port = q.port.unwrap_or(default_port);
    let proxies: Vec<_> = s
        .store
        .all()
        .iter()
        .filter(|p| p.enabled.load(std::sync::atomic::Ordering::Relaxed))
        .cloned()
        .collect();
    let results = crate::health::test_all(proxies, &host, port, 32).await;
    Json(results)
}

fn format_proxy(p: &crate::store::Proxy, fmt: &str) -> String {
    let scheme = match p.scheme {
        crate::parser::Scheme::Http => "http",
        crate::parser::Scheme::Socks5 => "socks5",
    };
    let auth = match (&p.user, &p.pass) {
        (Some(u), Some(pw)) => Some((u.clone(), pw.clone())),
        _ => None,
    };
    match fmt {
        "ip_port" => format!("{}:{}", p.host, p.port),
        "user_pass_at_ip_port" => match auth {
            Some((u, pw)) => format!("{u}:{pw}@{}:{}", p.host, p.port),
            None => format!("{}:{}", p.host, p.port),
        },
        "url" => match auth {
            Some((u, pw)) => format!("{scheme}://{u}:{pw}@{}:{}", p.host, p.port),
            None => format!("{scheme}://{}:{}", p.host, p.port),
        },
        _ => match auth {
            Some((u, pw)) => format!("{}:{}:{u}:{pw}", p.host, p.port),
            None => format!("{}:{}", p.host, p.port),
        },
    }
}

#[derive(Serialize)]
struct SessionsResp {
    count: usize,
}

async fn list_sessions(State(s): State<Arc<AppState>>) -> Json<SessionsResp> {
    Json(SessionsResp {
        count: s.selector.sessions_count(),
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Errors
// ────────────────────────────────────────────────────────────────────────────

struct ApiError {
    status: StatusCode,
    msg: String,
}

impl ApiError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            msg: msg.into(),
        }
    }
    fn server(e: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: e.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.msg }))).into_response()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Auth middleware + login/logout
// ────────────────────────────────────────────────────────────────────────────

/// Auth middleware. Accepts (in order):
///   1. Cookie `pg_token=<X>` where X is a valid issued token.
///   2. `Authorization: Bearer <X>` for token auth (e.g. curl + login token).
///   3. `Authorization: Basic <base64(_:password)>` matching admin password —
///      allows scripted access without going through /api/login.
///
/// On failure: 401 *without* `WWW-Authenticate` so browsers don't pop up the
/// native auth dialog. The SPA handles the redirect to its login form.
async fn require_admin_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let query = req.uri().query().map(|s| s.to_string());
    if check_auth(&state, req.headers(), query.as_deref()) {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "auth required" })),
    )
        .into_response()
}

fn check_auth(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    query: Option<&str>,
) -> bool {
    // 0. ?key=<admin_password> query param — convenient for scripts /
    //    third-party tools that can't set custom headers.
    if let Some(q) = query {
        let configured = state.config.read().admin_auth.password.clone();
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == "key" && percent_decode(v) == configured {
                    return true;
                }
            }
        }
    }
    // 1. Cookie.
    if let Some(c) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        for piece in c.split(';') {
            if let Some(rest) = piece.trim().strip_prefix("pg_token=") {
                if state.admin_tokens.contains_key(rest) {
                    return true;
                }
            }
        }
    }
    // 2. Authorization header.
    if let Some(h) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = h.strip_prefix("Bearer ").map(str::trim) {
            if state.admin_tokens.contains_key(t) {
                return true;
            }
        }
        if let Some(b64) = h
            .strip_prefix("Basic ")
            .or_else(|| h.strip_prefix("basic "))
        {
            if let Ok(decoded) = B64.decode(b64.trim()) {
                if let Ok(s) = std::str::from_utf8(&decoded) {
                    if let Some((_, pass)) = s.split_once(':') {
                        let configured = state.config.read().admin_auth.password.clone();
                        if pass == configured {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Minimal percent-decoder for query-param values.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginReq>,
) -> Response {
    let configured = state.config.read().admin_auth.password.clone();
    if req.password != configured {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "ok": false, "error": "wrong password" })),
        )
            .into_response();
    }
    let token = generate_token();
    state.admin_tokens.insert(token.clone(), ());
    let cookie = format!(
        "pg_token={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000"
    );
    let mut resp = Json(serde_json::json!({ "ok": true, "token": token })).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).unwrap(),
    );
    resp
}

async fn logout(State(state): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(c) = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        for piece in c.split(';') {
            if let Some(rest) = piece.trim().strip_prefix("pg_token=") {
                state.admin_tokens.remove(rest);
            }
        }
    }
    if let Some(h) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = h.strip_prefix("Bearer ").map(str::trim) {
            state.admin_tokens.remove(t);
        }
    }
    let mut resp = Json(serde_json::json!({ "ok": true })).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("pg_token=; Path=/; Max-Age=0"),
    );
    resp
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
