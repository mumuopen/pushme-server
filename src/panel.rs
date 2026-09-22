//! Web 管理面板（默认仅绑定 127.0.0.1:3010）：
//! - 安装（首次设置管理员账号）/ 登录 / 退出
//! - 推送 key 管理（增/删/备注），自动生成 key 与 temp_key
//! - 消息测试发送
//! - 修改密码、TLS 模式设置、自签名证书生成
//! - 运行状态（消息计数/连接数/运行时长）与最近推送日志
//!
//! 安全：HMAC session cookie；全局登录限速；IP 白名单；admin 路由不暴露公网端口

use crate::push_api::{AppState, SharedState};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use tracing::warn;

pub fn panel_router(state: SharedState, include_root: bool) -> Router {
    let mut r = Router::new()
        .route("/install", get(install_page).post(install_submit))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", get(logout).post(logout))
        .route("/api/keys", get(api_keys_list))
        .route("/api/keys/add", post(api_keys_add))
        .route("/api/keys/delete", post(api_keys_delete))
        .route("/api/keys/update", post(api_keys_update))
        .route("/api/test", post(api_test))
        .route("/api/setting/password", post(api_setting_password))
        .route("/api/setting/tls", post(api_setting_tls))
        .route("/api/setting/network", post(api_setting_network))
        .route("/api/setting/ports", post(api_setting_ports))
        .route("/api/setting/status", post(api_setting_status))
        .route("/api/setting/offline", post(api_setting_offline))
        .route("/api/cert/generate", post(api_cert_generate))
        .route("/api/log/history", get(api_log_history))
        .route("/api/log/clear", post(api_log_clear))
        .route("/api/log/stream", get(api_log_stream))
        .route("/api/status", get(api_status));
    if include_root {
        r = r.route("/", get(index));
    }
    r.with_state(state)
}

// ------------------------- 公共工具 -------------------------

fn html_response(s: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"))],
        s,
    )
        .into_response()
}

fn json_ok(v: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        v.to_string(),
    )
        .into_response()
}

fn json_err(msg: &str, code: StatusCode) -> Response {
    (
        code,
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        json!({ "ok": false, "message": msg }).to_string(),
    )
        .into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 从请求解析当前登录用户名
fn session_user(headers: &HeaderMap, state: &AppState) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    for kv in cookies.split(';') {
        let kv = kv.trim();
        if let Some(token) = kv.strip_prefix(&format!("{}=", crate::auth::SESSION_COOKIE)) {
            return state.sessions.verify(token);
        }
    }
    None
}

/// 从 body 解析表单/JSON 字符串参数
async fn form_params(req: Request) -> std::collections::HashMap<String, String> {
    let headers = req.headers().clone();
    let (_parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap_or_default();
    let mut map = std::collections::HashMap::new();
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ct.contains("json") {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(obj) = v.as_object() {
                for (k, v) in obj {
                    let s = match v {
                        serde_json::Value::String(s) => Some(s.clone()),
                        serde_json::Value::Number(n) => Some(n.to_string()),
                        serde_json::Value::Bool(b) => Some(b.to_string()),
                        _ => None,
                    };
                    if let Some(s) = s {
                        map.insert(k.clone(), s);
                    }
                }
            }
        }
    } else {
        // urlencoded form
        if let Ok(s) = std::str::from_utf8(&bytes) {
            for pair in s.split('&') {
                if pair.is_empty() {
                    continue;
                }
                let (k, v) = match pair.find('=') {
                    Some(pos) => (&pair[..pos], &pair[pos + 1..]),
                    None => (pair, ""),
                };
                let k = crate::third::url_decode(k);
                if !k.is_empty() {
                    map.entry(k).or_insert_with(|| crate::third::url_decode(v));
                }
            }
        }
    }
    map
}

fn random_hex(n_bytes: usize) -> String {
    (0..n_bytes).map(|_| format!("{:02x}", rand::random::<u8>())).collect()
}

// ------------------------- 页面 -------------------------

/// 面板首页：未安装 → 安装页；未登录 → 登录页；已登录 → 管理台
async fn index(State(state): State<SharedState>, req: Request) -> Response {
    render_index(state, req.headers()).await
}

async fn render_index(state: SharedState, headers: &HeaderMap) -> Response {
    let cfg = state.store.get();
    if !cfg.installed() {
        return html_response(page_shell("系统安装", INSTALL_HTML));
    }
    if session_user(headers, &state).is_none() {
        return html_response(page_shell("登录", LOGIN_HTML));
    }
    // 管理台
    let keys = cfg.push_keys();
    let mut rows = String::new();
    for k in &keys {
        rows.push_str(&format!(
            r#"<tr><td class="mono">{}</td><td class="mono">{}</td><td>{}</td><td><button class="btn danger" onclick="delKey('{}')">删除</button></td></tr>"#,
            html_escape(&k.key),
            html_escape(&k.temp_key),
            html_escape(&k.note),
            html_escape(&k.key),
        ));
    }
    let username = session_user(headers, &state).unwrap_or_default();
    let body = DASHBOARD_HTML
        .replace("{{USERNAME}}", &html_escape(&username))
        .replace("{{ROWS}}", &rows)
        .replace(
            "{{TLS}}",
            &html_escape(&cfg.tls),
        )
        .replace("{{PANEL_BIND}}", &html_escape(&cfg.panel_bind))
        .replace("{{PUBLIC_PANEL}}", if cfg.public_panel { "checked" } else { "" })
        .replace("{{OFFLINE_MESSAGES}}", if cfg.offline_messages { "checked" } else { "" })
        .replace("{{OFFLINE_LIMIT}}", &cfg.offline_limit.to_string());
    html_response(page_shell("管理台", &body))
}

async fn install_page(State(state): State<SharedState>) -> Response {
    if state.store.get().installed() {
        return Redirect::to("/").into_response();
    }
    html_response(page_shell("系统安装", INSTALL_HTML))
}

async fn install_submit(State(state): State<SharedState>, req: Request) -> Response {
    if state.store.get().installed() {
        return json_err("系统已安装", StatusCode::FORBIDDEN);
    }
    let params = form_params(req).await;
    let user = params.get("user").map(|s| s.trim()).unwrap_or("");
    let password = params.get("password").map(|s| s.as_str()).unwrap_or("");
    if user.is_empty() || password.is_empty() {
        return html_response(page_shell(
            "系统安装",
            &install_error("账号或密码不能为空！"),
        ));
    }
    match crate::auth::install(&state.store, user, password) {
        Ok(()) => {
            // 安装成功 → 直接签发 session 跳转管理台
            let token = state.sessions.issue(user).unwrap_or_default();
            let mut resp = Redirect::to("/").into_response();
            resp.headers_mut()
                .insert(header::SET_COOKIE, session_cookie(&token, &state).parse().unwrap());
            resp
        }
        Err(e) => html_response(page_shell("系统安装", &install_error(&e))),
    }
}

fn install_error(msg: &str) -> String {
    format!(
        r#"<div class="alert">{}</div>{}"#,
        html_escape(msg),
        INSTALL_HTML
    )
}

async fn login_page(State(state): State<SharedState>) -> Response {
    if !state.store.get().installed() {
        return Redirect::to("/install").into_response();
    }
    html_response(page_shell("登录", LOGIN_HTML))
}

async fn login_submit(State(state): State<SharedState>, req: Request) -> Response {
    let params = form_params(req).await;
    let user = params.get("user").map(|s| s.trim()).unwrap_or("");
    let password = params.get("password").map(|s| s.as_str()).unwrap_or("");

    // 登录限速（对齐官方：5 次失败锁 3 分钟）
    let mut guard = state.guard.lock().unwrap();
    let locked = guard.lock_remaining();
    if locked > 0 {
        let mins = locked / 60;
        let secs = locked % 60;
        let tip = if mins > 0 { format!("{mins}分{secs}秒") } else { format!("{secs}秒") };
        return html_response(page_shell("登录", &login_error(&format!("请{tip}后再试！"))));
    }

    match crate::auth::verify_login(&state.store, user, password) {
        Ok(()) => {
            guard.reset();
            drop(guard);
            let token = state.sessions.issue(user).unwrap_or_default();
            let mut resp = Redirect::to("/").into_response();
            resp.headers_mut()
                .insert(header::SET_COOKIE, session_cookie(&token, &state).parse().unwrap());
            resp
        }
        Err(_) => {
            let left = guard.record_fail();
            drop(guard);
            let tip = if left == 0 {
                "账号或密码错误！请3分钟后再试".to_string()
            } else if left <= 3 {
                format!("账号或密码错误！还剩{left}次机会")
            } else {
                "账号或密码错误".to_string()
            };
            html_response(page_shell("登录", &login_error(&tip)))
        }
    }
}

fn login_error(msg: &str) -> String {
    format!(
        r#"<div class="alert">{}</div>{}"#,
        html_escape(msg),
        LOGIN_HTML
    )
}

fn session_cookie(token: &str, state: &AppState) -> String {
    let secure = state.store.get().panel_tls == "tls";
    format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800{}",
        crate::auth::SESSION_COOKIE,
        if secure { "; Secure" } else { "" }
    )
}

async fn logout(State(state): State<SharedState>) -> Response {
    let mut resp = Redirect::to("/").into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        format!("{}=; Path=/; HttpOnly; Max-Age=0", crate::auth::SESSION_COOKIE)
            .parse()
            .unwrap(),
    );
    let _ = state;
    resp
}

// ------------------------- 管理 API -------------------------

fn require_login(state: &SharedState, headers: &HeaderMap) -> Option<Response> {
    if session_user(headers, state).is_some() {
        None
    } else {
        Some(json_err("未登录", StatusCode::UNAUTHORIZED))
    }
}

#[axum::debug_handler]
async fn api_keys_list(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let keys = state.store.get().push_keys();
    let arr: Vec<serde_json::Value> = keys
        .iter()
        .map(|k| json!({ "key": k.key, "temp_key": k.temp_key, "note": k.note }))
        .collect();
    json_ok(json!({ "ok": true, "keys": arr }))
}

async fn api_keys_add(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let params = form_params(req).await;
    let note = params.get("note").cloned().unwrap_or_default();
    let key = format!("PUSHME-{}", random_hex(16));
    let temp_key = random_hex(8);
    let _ = state.store.update(|c| {
        c.push_keys.push(json!({ "key": key, "temp_key": temp_key, "note": note }));
    });
    json_ok(json!({ "ok": true, "key": key, "temp_key": temp_key }))
}

async fn api_keys_delete(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let params = form_params(req).await;
    let key = params.get("key").cloned().unwrap_or_default();
    if key.is_empty() {
        return json_err("缺少 key", StatusCode::BAD_REQUEST);
    }
    let _ = state.store.update(|c| {
        c.push_keys.retain(|k| {
            k.get("key").and_then(|v| v.as_str()) != Some(key.as_str())
                && k.as_str() != Some(key.as_str())
        });
    });
    json_ok(json!({ "ok": true }))
}

async fn api_keys_update(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let params = form_params(req).await;
    let key = params.get("key").cloned().unwrap_or_default();
    let note = params.get("note").cloned().unwrap_or_default();
    let _ = state.store.update(|c| {
        for k in c.push_keys.iter_mut() {
            let matches = k.get("key").and_then(|v| v.as_str()) == Some(key.as_str())
                || k.as_str() == Some(key.as_str());
            if matches {
                if let Some(obj) = k.as_object_mut() {
                    obj.insert("note".into(), json!(note));
                }
            }
        }
    });
    json_ok(json!({ "ok": true }))
}

async fn api_test(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let params = form_params(req).await;
    let key = params.get("key").cloned().unwrap_or_default();
    let title = params.get("title").cloned().unwrap_or_default();
    let content = params.get("content").cloned().unwrap_or_default();
    if key.is_empty() {
        return json_err("缺少 key", StatusCode::BAD_REQUEST);
    }
    if !state.store.get().key_set().contains(&key) {
        return json_err("key 不存在", StatusCode::BAD_REQUEST);
    }
    let date = crate::now_str();
    let payload = json!({
        "title": title,
        "content": content,
        "date": date,
    })
    .to_string();
    let cfg = state.store.get();
    match state.hub.publish(&key, bytes::Bytes::from(payload), cfg.offline_messages, cfg.offline_limit) {
        Ok(()) => {
            state.store.incr_message_count(1);
            state.logger.push(&key, &title, "test-success");
            json_ok(json!({ "ok": true, "message": "已发送" }))
        }
        Err(e) => {
            state.logger.push(&key, &title, "test-failed");
            warn!("[panel] test push {key} failed: {e}");
            json_ok(json!({ "ok": false, "message": e }))
        }
    }
}

async fn api_setting_password(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let headers = req.headers().clone();
    let params = form_params(req).await;
    let old = params.get("old_password").map(|s| s.as_str()).unwrap_or("");
    let new = params.get("new_password").map(|s| s.as_str()).unwrap_or("");
    if new.len() < 6 {
        return json_err("新密码至少 6 位", StatusCode::BAD_REQUEST);
    }
    let current_user = session_user(&headers, &state).unwrap_or_default();
    if crate::auth::verify_login(&state.store, &current_user, old).is_err() {
        return json_err("原密码错误", StatusCode::FORBIDDEN);
    }
    let hash = match crate::auth::bcrypt_hash(new) {
        Ok(h) => h,
        Err(e) => return json_err(&e, StatusCode::INTERNAL_SERVER_ERROR),
    };
    let _ = state.store.update(|c| {
        c.pass_hash = hash;
        c.password = crate::auth::md5_pushme(new);
    });
    json_ok(json!({ "ok": true, "message": "密码已更新" }))
}

async fn api_setting_tls(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let params = form_params(req).await;
    let tls = params.get("tls").cloned().unwrap_or_default();
    if !matches!(tls.as_str(), "none" | "public" | "self") {
        return json_err("tls 取值仅支持 none/public/self", StatusCode::BAD_REQUEST);
    }
    let _ = state.store.update(|c| c.tls = tls.clone());
    json_ok(json!({
        "ok": true,
        "message": "已保存，重启服务后生效",
        "restart": true
    }))
}

/// 面板网络设置：panel_bind / public_panel / 面板端口
async fn api_setting_network(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let params = form_params(req).await;
    let panel_bind = params.get("panel_bind").cloned();
    let public_panel = params.get("public_panel").cloned();
    let _ = state.store.update(|c| {
        if let Some(b) = panel_bind {
            if !b.trim().is_empty() {
                c.panel_bind = b.trim().to_string();
            }
        }
        if let Some(p) = public_panel {
            c.public_panel = p == "true" || p == "1" || p == "on";
        }
    });
    json_ok(json!({
        "ok": true,
        "message": "已保存，重启服务后生效",
        "restart": true
    }))
}

/// 离线消息设置：开关 + 缓存条数上限
async fn api_setting_offline(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let params = form_params(req).await;
    let enabled = params
        .get("enabled")
        .map(|s| s == "true" || s == "1" || s == "on")
        .unwrap_or(false);
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(50)
        .clamp(1, 1000);
    let _ = state.store.update(|c| {
        c.offline_messages = enabled;
        c.offline_limit = limit;
    });
    json_ok(json!({
        "ok": true,
        "message": "已保存（立即生效）",
        "offline_messages": enabled,
        "offline_limit": limit,
    }))
}

/// 端口设置：server_port / panel_port（校验 1-65535，重启后生效）
async fn api_setting_ports(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let params = form_params(req).await;
    let server_port = params.get("server_port").and_then(|s| s.parse::<u16>().ok());
    let panel_port = params.get("panel_port").and_then(|s| s.parse::<u16>().ok());
    let mut changed = false;
    let _ = state.store.update(|c| {
        if let Some(p) = server_port {
            if p != c.server_port {
                c.server_port = p;
                changed = true;
            }
        }
        if let Some(p) = panel_port {
            if p != c.panel_port {
                c.panel_port = p;
                changed = true;
            }
        }
    });
    json_ok(json!({
        "ok": true,
        "message": if changed { "已保存，重启服务后生效" } else { "端口未变化" },
        "restart": changed,
    }))
}

/// 服务状态设置：start / stop（重启后生效）
async fn api_setting_status(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let params = form_params(req).await;
    let status = params.get("status").cloned().unwrap_or_default();
    if !matches!(status.as_str(), "start" | "stop") {
        return json_err("status 取值仅支持 start/stop", StatusCode::BAD_REQUEST);
    }
    let _ = state.store.update(|c| c.status = status.clone());
    json_ok(json!({
        "ok": true,
        "message": format!("消息服务已设为{}，重启后生效", if status == "start" { "启动" } else { "停止" }),
        "restart": true,
    }))
}

async fn api_cert_generate(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let params = form_params(req).await;
    // 多域名：逗号/换行/分号分隔（官方 domains 行为），自动补 127.0.0.1 与 ::1
    let raw = params.get("domains").cloned().unwrap_or_else(|| {
        params.get("cn").cloned().unwrap_or_default() // 兼容旧单域名参数
    });
    let names: Vec<String> = raw
        .split([',', '\n', '\r', ';'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return json_err("至少填写一个域名或 IP", StatusCode::BAD_REQUEST);
    }
    match crate::certs::generate_self_signed(&state.base_dir, &names) {
        Ok(()) => json_ok(json!({ "ok": true, "message": "证书已生成" })),
        Err(e) => json_err(&e, StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// 日志历史（最近 N 条，默认 100）
async fn api_log_history(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let count = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("count=")))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100)
        .min(1000);
    let logs: Vec<serde_json::Value> = state
        .logger
        .recent_n(count)
        .iter()
        .map(|l| json!({ "time": l.time, "key": l.key, "title": l.title, "result": l.result }))
        .collect();
    json_ok(json!({ "ok": true, "logs": logs }))
}

/// 清空日志缓冲
async fn api_log_clear(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    state.logger.clear();
    json_ok(json!({ "ok": true, "message": "日志已清空" }))
}

/// 日志实时流（SSE，对齐官方 log/stream）
async fn api_log_stream(State(state): State<SharedState>, req: Request) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let mut rx = state.logger.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(entry) => {
                    let data = serde_json::to_string(&json!({
                        "time": entry.time,
                        "key": entry.key,
                        "title": entry.title,
                        "result": entry.result,
                    }))
                    .unwrap_or_else(|_| "{}".to_string());
                    yield Ok::<_, std::convert::Infallible>(Event::default().data(data));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}

async fn api_status(State(state): State<SharedState>, req: Request) -> Response {
    if let Some(r) = require_login(&state, req.headers()) {
        return r;
    }
    let cfg = state.store.get();
    let conns = state.hub.connections.load(std::sync::atomic::Ordering::Relaxed);
    let uptime = state.started_at.elapsed().as_secs();
    let logs: Vec<serde_json::Value> = state
        .logger
        .recent()
        .iter()
        .take(30)
        .map(|l| json!({ "time": l.time, "key": l.key, "title": l.title, "result": l.result }))
        .collect();
    json_ok(json!({
        "ok": true,
        "status": {
            "message_count": state.store.message_count(),
            "connections": conns,
            "uptime": uptime,
            "tls": cfg.tls,
            "server_port": cfg.server_port,
            "panel_bind": cfg.panel_bind,
            "panel_port": cfg.panel_port,
            "offline_messages": cfg.offline_messages,
            "offline_limit": cfg.offline_limit,
            "status": cfg.status,
        },
        "logs": logs,
    }))
}

// ------------------------- HTML -------------------------

/// 页面外壳（统一深色风格）
fn page_shell(title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - PushMe Server</title>
<style>
:root {{ --bg:#0f1420; --card:#171e2e; --border:#2a3348; --text:#dce3f0; --dim:#8b95a8; --accent:#00e5ff; --danger:#ff5470; }}
* {{ box-sizing:border-box; margin:0; padding:0; }}
body {{ font-family:-apple-system,"Segoe UI","Microsoft YaHei",sans-serif; background:var(--bg); color:var(--text); line-height:1.6; }}
.wrap {{ max-width:960px; margin:0 auto; padding:32px 20px; }}
h1 {{ font-size:22px; margin-bottom:8px; }}
h1 .logo {{ color:var(--accent); }}
h2 {{ font-size:16px; margin:24px 0 12px; color:var(--accent); }}
.sub {{ color:var(--dim); font-size:13px; margin-bottom:24px; }}
.card {{ background:var(--card); border:1px solid var(--border); border-radius:10px; padding:18px; margin-bottom:16px; }}
input, select {{ background:#0d1220; border:1px solid var(--border); color:var(--text); padding:10px 12px; border-radius:6px; font-size:14px; width:100%; }}
textarea {{ background:#0d1220; border:1px solid var(--border); color:var(--text); padding:10px 12px; border-radius:6px; font-size:14px; width:100%; resize:vertical; min-height:72px; }}
input:focus, select:focus {{ outline:none; border-color:var(--accent); }}
label {{ display:block; font-size:13px; color:var(--dim); margin:10px 0 4px; }}
.btn {{ background:var(--accent); color:#04222a; border:none; padding:9px 18px; border-radius:6px; font-size:14px; font-weight:600; cursor:pointer; margin-top:12px; }}
.btn:hover {{ filter:brightness(1.15); }}
.btn.danger {{ background:var(--danger); color:#fff; padding:4px 10px; font-size:12px; margin:0; }}
table {{ width:100%; border-collapse:collapse; font-size:13px; }}
th, td {{ padding:8px 10px; border-bottom:1px solid var(--border); text-align:left; }}
th {{ color:var(--dim); font-weight:500; }}
.mono {{ font-family:Consolas,monospace; word-break:break-all; }}
.alert {{ background:#3a1620; border:1px solid var(--danger); color:#ffb3c0; padding:10px 14px; border-radius:6px; margin-bottom:16px; font-size:14px; }}
.grid {{ display:grid; grid-template-columns:1fr 1fr 1fr; gap:12px; }}
.stat {{ background:var(--card); border:1px solid var(--border); border-radius:10px; padding:14px; text-align:center; }}
.stat .num {{ font-size:26px; color:var(--accent); font-weight:700; }}
.stat .label {{ font-size:12px; color:var(--dim); }}
.row {{ display:flex; gap:8px; align-items:center; }}
.row input {{ flex:1; }}
.tip {{ font-size:12px; color:var(--dim); margin-top:6px; }}
</style>
</head>
<body><div class="wrap">{body}</div></body>
</html>"#
    )
}

const INSTALL_HTML: &str = r#"
<h1><span class="logo">PushMe</span> Server</h1>
<div class="sub">首次安装：设置管理员账号</div>
<div class="card" style="max-width:420px">
<form method="post" action="/install">
<label>管理员账号</label>
<input name="user" autocomplete="username" required>
<label>密码</label>
<input name="password" type="password" autocomplete="new-password" required>
<button class="btn" type="submit">完成安装</button>
</form>
</div>
"#;

const LOGIN_HTML: &str = r#"
<h1><span class="logo">PushMe</span> Server</h1>
<div class="sub">管理面板登录</div>
<div class="card" style="max-width:420px">
<form method="post" action="/login">
<label>账号</label>
<input name="user" autocomplete="username" required>
<label>密码</label>
<input name="password" type="password" autocomplete="current-password" required>
<button class="btn" type="submit">登录</button>
</form>
</div>
"#;

const DASHBOARD_HTML: &str = r#"
<h1><span class="logo">PushMe</span> Server <span style="font-size:13px;color:var(--dim)">({{USERNAME}})</span></h1>
<div class="sub"><a href="/logout" style="color:var(--dim)">退出登录</a></div>

<div class="grid">
<div class="stat"><div class="num" id="msg-count">0</div><div class="label">推送消息总数</div></div>
<div class="stat"><div class="num" id="conn-count">0</div><div class="label">当前 MQTT 连接</div></div>
<div class="stat"><div class="num" id="uptime">0s</div><div class="label">运行时长</div></div>
</div>

<h2>推送 Key</h2>
<div class="card">
<table id="keys-table">
<thead><tr><th>push_key</th><th>temp_key</th><th>备注</th><th>操作</th></tr></thead>
<tbody id="keys-body">{{ROWS}}</tbody>
</table>
<div class="row" style="margin-top:14px">
<input id="new-note" placeholder="备注（可选）">
<button class="btn" style="margin:0" onclick="addKey()">添加新 Key</button>
</div>
<span id="key-result" class="tip"></span>
<div class="tip">推送接口：GET/POST http://服务器:端口/?push_key=KEY&amp;title=标题&amp;content=内容 （也支持 temp_key）</div>
</div>

<h2>消息测试</h2>
<div class="card">
<div class="row"><input id="test-key" placeholder="push_key"></div>
<label>标题</label><input id="test-title" placeholder="测试标题">
<label>内容</label><input id="test-content" placeholder="测试内容">
<button class="btn" onclick="testPush()">发送测试消息</button>
<span id="test-result" class="tip"></span>
</div>



<h2>设置</h2>
<div class="card">
<label>修改密码</label>
<div class="row"><input id="old-pw" type="password" placeholder="原密码"></div>
<div class="row" style="margin-top:8px"><input id="new-pw" type="password" placeholder="新密码（至少6位）"></div>
<button class="btn" onclick="changePassword()">更新密码</button><span id="pw-result" class="tip"></span>

<label style="margin-top:20px">消息服务 TLS 模式</label>
<select id="tls-mode">
<option value="none">none（明文）</option>
<option value="self">self（自签名证书）</option>
<option value="public">public（正式证书，放入 config/certs/cert.crt 与 private.key）</option>
</select>
<button class="btn" onclick="saveTls()">保存 TLS 设置</button><span id="tls-result" class="tip"></span>

<label style="margin-top:20px">生成自签名证书（每行一个域名或 IP）</label>
<div><textarea id="cert-domains" rows="3" placeholder="push.example.com&#10;1.2.3.4&#10;（自动包含 127.0.0.1 与 ::1）"></textarea></div>
<button class="btn" onclick="genCert()">生成证书</button><span id="cert-result" class="tip"></span>

<label style="margin-top:20px">端口设置（重启后生效）</label>
<div class="row" style="margin-top:8px"><input id="server-port" type="number" min="1" max="65535" placeholder="消息服务端口（默认 3100）"></div>
<div class="row" style="margin-top:8px"><input id="panel-port" type="number" min="1" max="65535" placeholder="面板端口（默认 3010）"></div>
<button class="btn" onclick="savePorts()">保存端口</button><span id="ports-result" class="tip"></span>

<label style="margin-top:20px">消息服务状态（重启后生效）</label>
<div class="row"><span id="status-label" class="tip" style="flex:1"></span></div>
<div class="row" style="margin-top:8px"><button class="btn" style="margin:0;flex:1" onclick="setStatus('start')">启动消息服务</button><button class="btn danger" style="margin:0;flex:1" onclick="setStatus('stop')">停止消息服务</button></div>

<label style="margin-top:20px">面板网络（重启后生效）</label>
<div class="row"><input id="panel-bind" placeholder="面板绑定地址（默认 127.0.0.1）"></div>
<label style="margin-top:8px"><input type="checkbox" id="public-panel" style="width:auto;margin-right:6px">允许公网端口访问管理路由（不推荐）</label>
<button class="btn" onclick="saveNetwork()">保存网络设置</button><span id="net-result" class="tip"></span>

<label style="margin-top:20px">离线消息补发</label>
<label style="margin-top:8px"><input type="checkbox" id="offline-enabled" {{OFFLINE_MESSAGES}} style="width:auto;margin-right:6px">开启离线消息（发布时无在线订阅者则缓存，客户端重连后自动补发）</label>
<div class="row" style="margin-top:8px"><input id="offline-limit" type="number" min="1" max="1000" placeholder="缓存条数上限（默认 50）"></div>
<div class="tip">适用场景：交易消息等离线期间的消息，重连后不丢失。上限按推送 Key 分别缓存，超限淘汰最旧消息。</div>
<button class="btn" onclick="saveOffline()">保存离线设置</button><span id="offline-result" class="tip"></span>
</div>

<script>
async function post(url, data) {
  const body = new URLSearchParams(data);
  const r = await fetch(url, { method:'POST', body });
  return r.json();
}
function tip(id, msg, ok) {
  const el = document.getElementById(id);
  el.textContent = msg;
  el.style.color = ok ? '#7ee787' : '#ff9fb0';
}
async function refreshKeys() {
  const r = await fetch('/api/keys');
  if (!r.ok) return;
  const d = await r.json();
  const body = document.getElementById('keys-body');
  body.innerHTML = d.keys.map(k => '<tr><td class="mono">' + k.key + '</td><td class="mono">' + k.temp_key + '</td><td>' + (k.note || '') + '</td><td><button class="btn danger" onclick="delKey(\'' + k.key + '\')">删除</button></td></tr>').join('');
}
async function addKey() {
  const note = document.getElementById('new-note').value;
  const r = await post('/api/keys/add', { note });
  if (r.ok) { tip('key-result', '新 Key：' + r.key, true); document.getElementById('new-note').value = ''; }
  else tip('key-result', r.message || '失败', false);
  refreshKeys();
}
async function delKey(key) {
  if (!confirm('确认删除该 Key？')) return;
  await post('/api/keys/delete', { key });
  refreshKeys();
}
async function testPush() {
  const key = document.getElementById('test-key').value;
  const title = document.getElementById('test-title').value;
  const content = document.getElementById('test-content').value;
  const r = await post('/api/test', { key, title, content });
  tip('test-result', r.message, r.ok);
}
async function changePassword() {
  const old_password = document.getElementById('old-pw').value;
  const new_password = document.getElementById('new-pw').value;
  const r = await post('/api/setting/password', { old_password, new_password });
  tip('pw-result', r.message, r.ok);
}
async function saveTls() {
  const tls = document.getElementById('tls-mode').value;
  const r = await post('/api/setting/tls', { tls });
  tip('tls-result', r.message, r.ok);
}
async function genCert() {
  const domains = document.getElementById('cert-domains').value || 'localhost';
  const r = await post('/api/cert/generate', { domains });
  tip('cert-result', r.message, r.ok);
}
async function saveNetwork() {
  const panel_bind = document.getElementById('panel-bind').value;
  const public_panel = document.getElementById('public-panel').checked ? 'true' : 'false';
  const r = await post('/api/setting/network', { panel_bind, public_panel });
  tip('net-result', r.message, r.ok);
}
async function saveOffline() {
  const enabled = document.getElementById('offline-enabled').checked ? 'true' : 'false';
  const limit = document.getElementById('offline-limit').value || '50';
  const r = await post('/api/setting/offline', { enabled, limit });
  tip('offline-result', r.message, r.ok);
}
async function savePorts() {
  const server_port = document.getElementById('server-port').value;
  const panel_port = document.getElementById('panel-port').value;
  const r = await post('/api/setting/ports', { server_port, panel_port });
  tip('ports-result', r.message, r.ok);
}
async function setStatus(status) {
  const r = await post('/api/setting/status', { status });
  tip('ports-result', r.message, r.ok);
}
async function clearLogs() {
  const r = await post('/api/log/clear', {});
  tip('logs-body', r.message, r.ok);
  document.getElementById('logs-body').innerHTML = '';
}
function appendLog(l) {
  const tbody = document.getElementById('logs-body');
  const tr = document.createElement('tr');
  tr.innerHTML = '<td class="mono">' + (l.time || '') + '</td><td class="mono">' + (l.key || '') + '</td><td>' + (l.title || '') + '</td><td>' + (l.result || '') + '</td>';
  tbody.insertBefore(tr, tbody.firstChild);
  while (tbody.children.length > 100) tbody.removeChild(tbody.lastChild);
}
async function loadLogs() {
  const r = await fetch('/api/log/history?count=100');
  if (!r.ok) return;
  const d = await r.json();
  (d.logs || []).forEach(appendLog);
}
function openLogStream() {
  const es = new EventSource('/api/log/stream');
  es.onmessage = e => { try { appendLog(JSON.parse(e.data)); } catch (err) {} };
  es.onerror = () => { es.close(); setTimeout(openLogStream, 5000); };
}
function fmtUptime(s) {
  if (s < 60) return s + 's';
  if (s < 3600) return Math.floor(s/60) + 'm' + (s%60) + 's';
  return Math.floor(s/3600) + 'h' + Math.floor((s%3600)/60) + 'm';
}
async function refreshStatus() {
  const r = await fetch('/api/status');
  if (!r.ok) return;
  const d = await r.json();
  document.getElementById('msg-count').textContent = d.status.message_count;
  document.getElementById('conn-count').textContent = d.status.connections;
  document.getElementById('uptime').textContent = fmtUptime(d.status.uptime);
  if (!document.getElementById('panel-bind').value) document.getElementById('panel-bind').value = d.status.panel_bind;
  document.getElementById('tls-mode').value = d.status.tls;
  if (!document.getElementById('server-port').value) document.getElementById('server-port').value = d.status.server_port;
  if (!document.getElementById('panel-port').value) document.getElementById('panel-port').value = d.status.panel_port;
  if (!document.getElementById('offline-limit').value) document.getElementById('offline-limit').value = d.status.offline_limit || 50;
  const st = d.status.status === 'stop' ? '已停止' : '运行中';
  document.getElementById('status-label').textContent = '消息服务：' + st + '（端口 ' + d.status.server_port + '）';
}
refreshKeys();
refreshStatus();
loadLogs();
openLogStream();
setInterval(refreshStatus, 5000);
</script>
"#;
