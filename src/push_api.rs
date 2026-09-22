//! 公网端口（默认 3100）路由：
//! - 推送 API：GET/POST 任意路径（对齐官方单入口设计），push_key 鉴权
//! - 第三方 webhook 兼容：飞书 msg_type / 企微、钉钉 msgtype 自动转译
//! - 证书下载：/certs/cert.crt、/certs/download
//! - WebSocket：任意路径升级 → MQTT over WS
//! - 安全：不暴露任何管理路由（admin/login/install 一律 404，除非 public_panel=true）

use crate::auth::Sessions;
use crate::config::ConfigStore;
use crate::mqtt::{MqttHub, WsIo};
use crate::third::{self, Params};
use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use serde_json::json;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{info, warn};

/// 全局应用状态
pub struct AppState {
    pub store: Arc<ConfigStore>,
    pub hub: Arc<MqttHub>,
    pub sessions: Sessions,
    /// 登录失败限速（全局，对齐官方）
    pub guard: Mutex<crate::auth::LoginGuard>,
    /// 内存推送日志（环形，供面板展示）
    pub logger: PushLog,
    pub base_dir: PathBuf,
    pub started_at: Instant,
}

pub type SharedState = Arc<AppState>;

/// 内存推送日志（环形 + broadcast 订阅，供面板历史/SSE 实时展示）
pub struct PushLog {
    inner: Mutex<VecDeque<LogEntry>>,
    capacity: usize,
    tx: tokio::sync::broadcast::Sender<LogEntry>,
}

#[derive(Clone)]
pub struct LogEntry {
    pub time: String,
    pub key: String,
    pub title: String,
    pub result: String,
}

impl PushLog {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(512);
        Self { inner: Mutex::new(VecDeque::with_capacity(capacity)), capacity, tx }
    }

    pub fn push(&self, key: &str, title: &str, result: &str) {
        let entry = LogEntry {
            time: crate::now_str(),
            key: key.to_string(),
            title: title.chars().take(60).collect(),
            result: result.to_string(),
        };
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.capacity {
            q.pop_front();
        }
        q.push_back(entry.clone());
        // SSE 订阅者推送（无订阅者时静默丢弃）
        let _ = self.tx.send(entry);
    }

    pub fn recent(&self) -> Vec<LogEntry> {
        self.inner.lock().unwrap().iter().rev().cloned().collect()
    }

    /// 最近 N 条（正序，供 history 接口）
    pub fn recent_n(&self, n: usize) -> Vec<LogEntry> {
        let q = self.inner.lock().unwrap();
        q.iter().rev().take(n).cloned().collect()
    }

    /// 清空日志缓冲
    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }

    /// 订阅实时日志流
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LogEntry> {
        self.tx.subscribe()
    }
}

/// 公网 router（面板路由在 main.rs 按需 merge）
///
/// 安全设计（修复 mumuopen 版公网暴露 admin）：
/// - `/` 推送 API（含 WS 升级到 MQTT）
/// - `/certs/cert.crt`、`/certs/download` 证书下载
/// - 其他路径仅接受 WebSocket 升级（兼容官方任意 path WS），否则 404
/// - admin/login/install 等管理路由在公网端口一律 404（除非 public_panel=true 由 panel router 覆盖）
pub fn public_router(state: SharedState) -> Router {
    Router::new()
        .route("/", any(push_entry))
        .route("/certs/cert.crt", get(cert_raw))
        .route("/certs/download", get(cert_download))
        .fallback(ws_or_404)
        .with_state(state)
}

/// 非 `/` 非 certs 的路径：仅允许 WebSocket 升级，其余 404
async fn ws_or_404(State(state): State<SharedState>, req: Request) -> Response {
    if !is_ws_request(&req) {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }
    let (mut parts, _body) = req.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws) => ws,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid websocket upgrade").into_response(),
    };
    let hub = state.hub.clone();
    let store = state.store.clone();
    ws.protocols(["mqtt"])
        .on_upgrade(move |socket| handle_ws(socket, hub, store))
}

/// 是否为 WebSocket 升级请求
fn is_ws_request(req: &Request) -> bool {
    let conn = req
        .headers()
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let upgrade = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    conn.contains("upgrade") && upgrade == "websocket"
}

/// 任意路径入口：WS 升级 → MQTT over WS；否则推送 API
async fn push_entry(State(state): State<SharedState>, req: Request) -> Response {
    // CORS（对齐官方）
    let mut cors = HeaderMap::new();
    cors.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    cors.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS"),
    );
    cors.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization, X-Requested-With, Accept, Origin"),
    );
    cors.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));

    if req.method() == axum::http::Method::OPTIONS {
        return (StatusCode::OK, cors, "").into_response();
    }

    if is_ws_request(&req) {
        let (mut parts, _body) = req.into_parts();
        let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(ws) => ws,
            Err(_) => return (StatusCode::BAD_REQUEST, "invalid websocket upgrade").into_response(),
        };
        let hub = state.hub.clone();
        let store = state.store.clone();
        return ws
            .protocols(["mqtt"])
            .on_upgrade(move |socket| handle_ws(socket, hub, store));
    }

    let (parts, body) = req.into_parts();
    let resp = push_api(&state, parts.uri.query(), &parts.headers, body).await;
    (cors, resp).into_response()
}

/// WebSocket → MQTT 会话桥接
async fn handle_ws(
    mut socket: axum::extract::ws::WebSocket,
    hub: Arc<MqttHub>,
    store: Arc<ConfigStore>,
) {
    use axum::extract::ws::Message;
    use tokio::sync::mpsc;

    let (in_tx, in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let io = WsIo::new(in_rx, out_tx);

    // MQTT 会话跑在独立任务
    let session = tokio::spawn(async move {
        hub.handle_client(io, store).await;
    });

    // WS 帧桥接循环
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        if in_tx.send(b.to_vec()).is_err() {
                            break; // MQTT 会话已结束
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // text/ping/pong 忽略
                    Some(Err(e)) => {
                        warn!("[WS] recv error: {e}");
                        break;
                    }
                }
            }
            out = out_rx.recv() => {
                match out {
                    Some(chunk) => {
                        if socket.send(Message::Binary(chunk.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    session.abort();
}

/// 解析请求参数（query 优先，其次 JSON / urlencoded body）
async fn parse_params(query: Option<&str>, headers: &HeaderMap, body: Body) -> Params {
    let mut map = std::collections::HashMap::new();

    if let Some(q) = query {
        third::parse_urlencoded(q, &mut map);
    }

    let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap_or_default();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if content_type.contains("application/json") {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        if let Some(obj) = v.as_object() {
            for (k, v) in obj {
                map.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    } else if content_type.contains("application/x-www-form-urlencoded") {
        if let Ok(s) = std::str::from_utf8(&bytes) {
            third::parse_urlencoded(s, &mut map);
        }
    }

    Params { map }
}

/// 推送 API 主逻辑（对齐官方 app/controller/index.js）
async fn push_api(
    state: &SharedState,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let params = parse_params(query, headers, body).await;

    let mut push_key = params.get_str("push_key");
    let temp_key = params.get_str("temp_key");
    let mut title = params.get_str("title");
    let mut content = params.get_str("content");
    let mut kind = params.get_str("type");

    // temp_key 反查
    if push_key.is_none() && let Some(tk) = &temp_key {
        if let Some(k) = state.store.get().find_by_temp_key(tk) {
            push_key = Some(k);
        }
    }

    // 第三方平台转译（飞书/企微/钉钉）
    let third_data = third::detect(&params);
    if third_data.detected {
        if !third_data.title.is_empty() {
            title = Some(third_data.title.clone());
        }
        if !third_data.content.is_empty() {
            content = Some(third_data.content.clone());
        }
        if third_data.kind == "markdown" {
            kind = Some("markdown".to_string());
        }
    }

    // 无任何推送参数 → 服务标识文本（公网不放 HTML 面板）
    if push_key.is_none() && title.is_none() && content.is_none() {
        return text_response("PushMe Server");
    }

    // push_key 校验（对齐官方 _check_keys）
    if params.get_val("push_key").is_some_and(|v| v.is_object() || v.is_array()) {
        return text_response("Push failed, push_key type must be string!");
    }
    let Some(push_key) = push_key.filter(|s| !s.is_empty()) else {
        return text_response("Push failed, empty push_key!");
    };
    // title/content 参数为对象且未被第三方转译覆盖 → 格式错误
    let title_bad = title.is_none()
        && params
            .get_val("title")
            .is_some_and(|v| v.is_object() || v.is_array());
    let content_bad = content.is_none()
        && params
            .get_val("content")
            .is_some_and(|v| v.is_object() || v.is_array());
    if title_bad || content_bad {
        return text_response("Push failed, the parameter format is incorrect!");
    }

    let keys: Vec<&str> = if push_key.contains(',') {
        push_key.split(',').collect()
    } else {
        vec![push_key.as_str()]
    };
    if keys.len() > 100 {
        return text_response("Push failed, push_key numbers must be less than 100!");
    }

    // 白名单校验
    let cfg = state.store.get();
    let key_set = cfg.key_set();
    for k in &keys {
        if !k.is_empty() && !key_set.contains(*k) {
            warn!("[push] 非法push_key {k}");
            return text_response("非法push_key!");
        }
    }
    // 离线消息设置（发布时无在线订阅者则缓存，重连补发）
    let offline_enabled = cfg.offline_messages;
    let offline_limit = cfg.offline_limit;
    drop(cfg);

    // title、content 至少一项（对齐官方：两者均为显式空串时报错）
    if title.as_deref() == Some("") && content.as_deref() == Some("") {
        return text_response("Push failed, empty title and content!");
    }

    // date：未传或为空时注入当前时间（对齐官方 publish）
    let mut date = params.get_str("date").unwrap_or_default();
    if date.is_empty() {
        date = crate::now_str();
    }

    // 组装消息 payload（键序对齐官方：title, content, date, type）
    #[derive(serde::Serialize)]
    struct PushMsg<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<&'a str>,
        date: &'a str,
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        kind: Option<&'a str>,
    }
    let title_ref = title.as_deref();
    let content_ref = content.as_deref();
    let kind_ref = kind.as_deref();
    let msg = PushMsg { title: title_ref, content: content_ref, date: &date, kind: kind_ref };
    let payload = serde_json::to_vec(&msg).unwrap_or_else(|_| b"{}".to_vec());

    // 发布
    let mut result = "success".to_string();
    let single = keys.len() == 1;
    for k in &keys {
        if k.is_empty() {
            continue;
        }
        match state.hub.publish(k, bytes::Bytes::from(payload.clone()), offline_enabled, offline_limit) {
            Ok(()) => {
                state.store.incr_message_count(1);
                state.logger.push(k, title.as_deref().unwrap_or(""), "success");
                info!("[push] publish success: {k}");
            }
            Err(e) => {
                state.logger.push(k, title.as_deref().unwrap_or(""), &format!("failed: {e}"));
                warn!("[push] publish {k} failed: {e}");
                // 对齐官方：单 key 时结果反映失败；多 key 固定 success
                if single {
                    result = format!("failed: {e}");
                }
            }
        }
    }

    // 第三方请求返回第三方格式
    if third_data.detected {
        let ok = result == "success";
        let state_code = if ok { 0 } else { 1 };
        return json_response(json!({
            "errcode": state_code,
            "errmsg": result,
            "code": state_code,
            "msg": result,
        }));
    }

    text_response(&result)
}

fn text_response(s: &str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"))],
        s.to_string(),
    )
        .into_response()
}

fn json_response(v: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        v.to_string(),
    )
        .into_response()
}

/// /certs/cert.crt：直接返回证书内容
async fn cert_raw(State(state): State<SharedState>) -> Response {
    match crate::certs::read_cert_pem(&state.base_dir) {
        Some(b) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, HeaderValue::from_static("application/x-x509-ca-cert"))],
            b,
        )
            .into_response(),
        None => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"))],
            "请先在服务端生成自签名证书",
        )
            .into_response(),
    }
}

/// /certs/download：附件下载
async fn cert_download(State(state): State<SharedState>) -> Response {
    match crate::certs::read_cert_pem(&state.base_dir) {
        Some(b) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("application/x-x509-ca-cert")),
                (header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"cert.crt\"")),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            ],
            b,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
