//! PushMe Server（Rust 版）
//!
//! 架构（修复 mumuopen 版公网暴露 admin 的安全问题）：
//! - 公网端口（默认 0.0.0.0:3100）单端口多协议：
//!   MQTT（TCP/TLS）+ WebSocket + HTTP（推送 API / 证书下载）
//!   管理路由（admin/login/install）一律 404，除非配置 public_panel=true
//! - 面板端口（默认 127.0.0.1:3010）：完整管理面板，仅本机/白名单可访问
//!
//! 用法：pushme-server-rs [--data <数据目录>]

mod auth;
mod certs;
mod config;
mod mqtt;
mod panel;
mod push_api;
mod third;

use crate::config::ConfigStore;
use crate::push_api::{AppState, SharedState};
use hyper_util::service::TowerToHyperService;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};

/// 组合 trait（trait object 不能直接多 trait；Unpin 作为超 trait 保证 Box 化后可轮询）
trait AnyStreamIo: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> AnyStreamIo for T {}

/// 任意字节流（TCP / TLS，Box 自动 Unpin）
type AnyIo = Box<dyn AnyStreamIo + Send>;

/// 包装后的 axum 服务（Router 直接实现 tower Service，适配 hyper-util）
pub type HyperService = TowerToHyperService<axum::routing::RouterIntoService<hyper::body::Incoming>>;

/// 当前本地时间字符串（官方 date 注入格式：YYYY-mm-dd HH:ii:ss）
pub fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn main_banner() {
    println!("\n  ┌─────────────────────────────────────┐");
    println!("  │        PushMe Server (Rust)         │");
    println!("  └─────────────────────────────────────┘\n");
}

#[tokio::main]
async fn main() {
    // rustls CryptoProvider：依赖图中存在多个 rustls 版本时无法自动确定，
    // 显式安装 ring provider（对应 Cargo.toml 中 rustls 的 ring feature）
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    main_banner();

    // --data 参数（默认当前目录）
    let args: Vec<String> = std::env::args().collect();
    let base_dir = match args.iter().position(|a| a == "--data") {
        Some(i) => args.get(i + 1).map(PathBuf::from).unwrap_or_default(),
        None => std::env::current_dir().unwrap_or_default(),
    };

    let store = match ConfigStore::load(&base_dir) {
        Ok(s) => s,
        Err(e) => {
            error!("配置加载失败（{}）：{e}", base_dir.display());
            return;
        }
    };
    let hub = mqtt::MqttHub::new();

    let state: SharedState = Arc::new(AppState {
        store: store.clone(),
        hub,
        sessions: auth::Sessions::new(&store.get().session_key),
        guard: Mutex::new(auth::LoginGuard::new()),
        logger: push_api::PushLog::new(200),
        base_dir: base_dir.clone(),
        started_at: Instant::now(),
    });

    let cfg = store.get();
    let server_port = cfg.server_port;
    let panel_port = cfg.panel_port;
    let panel_bind = cfg.panel_bind.clone();
    let status = cfg.status.clone();

    // ---- TLS ----
    let tls_acceptor: Option<TlsAcceptor> = match cfg.tls.as_str() {
        "self" => {
            if let Err(e) = certs::ensure_self_signed(&base_dir) {
                warn!("[certs] 自签名证书生成失败：{e}");
            }
            match certs::load_tls_acceptor(&base_dir) {
                Ok(a) => Some(a),
                Err(e) => {
                    warn!("[certs] TLS 配置失败，公网端口将以明文运行：{e}");
                    None
                }
            }
        }
        "public" => match certs::load_tls_acceptor(&base_dir) {
            Ok(a) => Some(a),
            Err(e) => {
                warn!("[certs] TLS 证书缺失（config/certs/cert.crt、private.key），公网端口将以明文运行：{e}");
                None
            }
        },
        _ => None,
    };

    // ---- 面板独立 TLS（panel_tls 独立于消息服务 tls；"tls" 时面板走 HTTPS）----
    let panel_acceptor: Option<TlsAcceptor> = match cfg.panel_tls.as_str() {
        "tls" => {
            if let Err(e) = certs::ensure_self_signed(&base_dir) {
                warn!("[certs] 面板自签名证书生成失败：{e}");
            }
            match certs::load_tls_acceptor(&base_dir) {
                Ok(a) => Some(a),
                Err(e) => {
                    warn!("[certs] 面板 TLS 配置失败，面板将以明文运行：{e}");
                    None
                }
            }
        }
        _ => None,
    };

    // ---- 公网消息服务（0.0.0.0:server_port）----
    let public_router = if cfg.public_panel {
        warn!("[public] public_panel=true，管理路由将暴露公网端口（不建议）");
        push_api::public_router(state.clone()).merge(panel::panel_router(state.clone(), false))
    } else {
        push_api::public_router(state.clone())
    };
    let public_svc: HyperService =
        TowerToHyperService::new(public_router.into_service::<hyper::body::Incoming>());
    drop(cfg);

    if status == "stop" {
        warn!("[public] status=stop，消息服务未启动（可在 data.json 中改为 start 后重启）");
    } else {
        let state2 = state.clone();
        let tls = tls_acceptor.clone();
        tokio::spawn(async move {
            let listener = match TcpListener::bind(("0.0.0.0", server_port)).await {
                Ok(l) => l,
                Err(e) => {
                    error!("[public] 端口 {server_port} 监听失败：{e}");
                    return;
                }
            };
            info!(
                "[public] 消息服务已启动 0.0.0.0:{server_port}（MQTT/WS/推送API{}）",
                if tls.is_some() { "/TLS" } else { "" }
            );
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let st = state2.clone();
                        let tls = tls.clone();
                        let svc = public_svc.clone();
                        tokio::spawn(async move {
                            handle_public_conn(stream, peer, st, tls, svc).await;
                        });
                    }
                    Err(e) => {
                        warn!("[public] accept 失败：{e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
    }

    // ---- 管理面板（panel_bind:panel_port）----
    let panel_addr = format!("{panel_bind}:{panel_port}");
    let listener = match TcpListener::bind(&panel_addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("[panel] 面板监听失败（{panel_addr}）：{e}");
            return;
        }
    };
    info!("[panel] 管理面板已启动 http://{panel_addr}");

    let panel_tls = panel_acceptor;
    let panel_state = state.clone();

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                // IP 白名单
                let allowed = panel_state.store.get().panel_allowed_ips.clone();
                if !auth::ip_allowed(&allowed, peer.ip()) {
                    warn!("[panel] 拒绝白名单外 IP 访问：{}", peer.ip());
                    continue;
                }
                let tls = panel_tls.clone();
                let panel_svc: HyperService = TowerToHyperService::new(
                    panel::panel_router(panel_state.clone(), true)
                        .into_service::<hyper::body::Incoming>(),
                );
                tokio::spawn(async move {
                    let io: AnyIo = match tls {
                        Some(acceptor) => match acceptor.accept(stream).await {
                            Ok(s) => Box::new(s),
                            Err(_) => return,
                        },
                        None => Box::new(stream),
                    };
                    let io = hyper_util::rt::TokioIo::new(io);
                    let _ = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(io, panel_svc)
                    .await;
                });
            }
            Err(e) => {
                warn!("[panel] accept 失败：{e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// 公网连接处理：TLS 解密 → 协议嗅探 → 分流
async fn handle_public_conn(
    stream: tokio::net::TcpStream,
    peer: std::net::SocketAddr,
    state: SharedState,
    tls: Option<TlsAcceptor>,
    svc: HyperService,
) {
    // TLS 握手
    let mut tls_stream: AnyIo = match tls {
        Some(acceptor) => match acceptor.accept(stream).await {
            Ok(s) => Box::new(s),
            Err(_) => return,
        },
        None => Box::new(stream),
    };

    // 协议嗅探：读至少 8 字节（5 秒超时，对齐官方快速检测）
    let mut prefix: Vec<u8> = Vec::with_capacity(512);
    let mut buf = [0u8; 512];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(5), tls_stream.read(&mut buf)).await;
        match read {
            Err(_) => return, // 超时销毁
            Ok(Err(_)) => return,
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => {
                prefix.extend_from_slice(&buf[..n]);
                if prefix.len() >= 8 {
                    break;
                }
            }
        }
    }

    if is_mqtt(&prefix) {
        info!("[public] MQTT connection: {peer}");
        let io = SniffedStream::new(tls_stream, prefix);
        state.hub.handle_client(io, state.store.clone()).await;
    } else if is_http(&prefix) {
        let io = SniffedStream::new(tls_stream, prefix);
        let io = hyper_util::rt::TokioIo::new(io);
        if let Err(e) =
            hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection_with_upgrades(io, svc)
                .await
        {
            let msg = e.to_string();
            if !msg.contains("closed") && !msg.contains("connection closed") {
                tracing::debug!("[public] http connection error: {msg}");
            }
        }
    } else {
        // 未知协议，销毁连接（对齐官方）
        warn!("[public] unknown protocol from {peer}, dropped");
    }
}

/// MQTT 检测：首字节 0x10 且 [4..8] == "MQTT"（对齐官方）
fn is_mqtt(b: &[u8]) -> bool {
    b.len() >= 8 && b[0] == 0x10 && &b[4..8] == b"MQTT"
}

/// HTTP 检测：常见请求方法前缀（对齐官方）
fn is_http(b: &[u8]) -> bool {
    const METHODS: [&[u8]; 7] = [b"GET ", b"POST", b"PUT ", b"DELETE", b"HEAD", b"OPTIONS", b"PATCH"];
    METHODS.iter().any(|m| b.starts_with(m))
}

/// 带前缀回放的流（嗅探已读字节不丢失）
pub struct SniffedStream<S> {
    inner: S,
    prefix: Vec<u8>,
    pos: usize,
}

impl<S> SniffedStream<S> {
    pub fn new(inner: S, prefix: Vec<u8>) -> Self {
        Self { inner, prefix, pos: 0 }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for SniffedStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.remaining());
            buf.put_slice(&self.prefix[self.pos..self.pos + n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for SniffedStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
