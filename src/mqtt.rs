//! MQTT 连接处理：自写协议前端（mqttbytes 编解码）+ rumqttd 路由内核
//!
//! 设计要点（对齐官方 pushme-server 的 aedes 行为）：
//! - connect：keepalive 60/300/600 → 3600（官方 preConnect 同款调整）
//! - 订阅/发布 ACL：仅允许 push_keys 白名单内的 topic（官方 authorizeSubscribe/authorizePublish）
//! - 非法 publish → 断开连接（官方 aedes callback error 行为）
//! - 消息以 qos=1 投递给订阅客户端（官方 pushme.publish qos=1）
//! - 内存路由，无持久化；clean_session 恒为 true（第一版限制，README 注明）

use crate::config::ConfigStore;
use bytes::{Bytes, BytesMut};
use rumqttc::mqttbytes::v4::{
    ConnAck, ConnectReturnCode, Packet, PubAck, PubComp, Publish, SubAck, SubscribeReasonCode,
    UnsubAck,
};
use rumqttc::mqttbytes::{self, Protocol, QoS};
use rumqttd::local::{LinkBuilder, LinkError, LinkRx, LinkTx};
use rumqttd::{Config, Notification, Router, RouterConfig};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info, warn};

/// 单个 MQTT 包上限（推送消息载荷保护）
pub const MAX_PACKET_SIZE: usize = 1024 * 1024;

/// WebSocket ↔ 字节流适配器（axum ws Binary 帧 ↔ MQTT 字节流）
pub struct WsIo {
    rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pending: Vec<u8>,
    pending_pos: usize,
}

impl WsIo {
    pub fn new(
        rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) -> Self {
        Self { rx, tx, pending: Vec::new(), pending_pos: 0 }
    }
}

impl AsyncRead for WsIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // 先吐 pending
        if self.pending_pos < self.pending.len() {
            let avail = self.pending.len() - self.pending_pos;
            let n = avail.min(buf.remaining());
            buf.put_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
            self.pending_pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        // 再等新 chunk
        match self.rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(chunk)) => {
                let n = chunk.len().min(buf.remaining());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.pending = chunk;
                    self.pending_pos = n;
                }
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(Ok(())), // 对端关闭 = EOF
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl AsyncWrite for WsIo {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        // 无界通道：永不阻塞；WebSocket 单帧上限 16KB
        let n = buf.len().min(16 * 1024);
        let chunk = buf[..n].to_vec();
        match self.tx.send(chunk) {
            Ok(()) => std::task::Poll::Ready(Ok(n)),
            Err(_) => std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "ws closed",
            ))),
        }
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), std::io::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), std::io::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// MQTT 服务核心：rumqttd Router 内核 + 自建链接（dynamic_filters 开启，支持动态主题）
///
/// 注意：不用 `Broker::link`（它写死 dynamic_filters=false，动态订阅/发布会被路由内核丢弃），
/// 而是通过 `Router::new + spawn` 拿到 router_tx 后，用 `LinkBuilder` 闭包工厂自建链接。
pub struct MqttHub {
    /// 建链工厂（内部持有 router_tx）
    make_link: Arc<dyn Fn(&str) -> Result<(LinkTx, LinkRx), LinkError> + Send + Sync>,
    publish_link: Mutex<LinkTx>,
    /// 在线订阅计数：topic → 订阅连接数（离线队列判据）
    subscriptions: std::sync::RwLock<std::collections::HashMap<String, usize>>,
    /// 离线消息队列：topic → 消息（重连补发）
    offline: std::sync::RwLock<std::collections::HashMap<String, std::collections::VecDeque<OfflineMsg>>>,
    pub connections: AtomicI64,
}

/// 离线消息条目
#[derive(Clone)]
pub struct OfflineMsg {
    pub payload: Bytes,
}

impl MqttHub {
    pub fn new() -> Arc<Self> {
        let config = Config {
            id: 0,
            router: RouterConfig {
                max_connections: 10010,
                max_outgoing_packet_count: 200,
                max_segment_size: 100 * 1024 * 1024,
                max_segment_count: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        let router = Router::new(config.id, config.router.clone());
        let router_tx = router.spawn();

        let make_link: Arc<dyn Fn(&str) -> Result<(LinkTx, LinkRx), LinkError> + Send + Sync> = {
            let router_tx = router_tx.clone();
            Arc::new(move |client_id: &str| {
                LinkBuilder::new(client_id, router_tx.clone())
                    .dynamic_filters(true)
                    .build()
                    .map(|(tx, rx, _ack)| (tx, rx))
            })
        };
        let (link_tx, _link_rx) = (make_link)("pushme-api-publisher")
            .expect("failed to create publisher link");
        Arc::new(Self {
            make_link,
            publish_link: Mutex::new(link_tx),
            subscriptions: std::sync::RwLock::new(std::collections::HashMap::new()),
            offline: std::sync::RwLock::new(std::collections::HashMap::new()),
            connections: AtomicI64::new(0),
        })
    }

    /// 推送 API 发布消息（入路由内核；投递给订阅端时以 qos1 下发）
    /// 离线消息开关开启且该主题无在线订阅者时，消息缓存进离线队列供重连补发
    pub fn publish(
        &self,
        topic: &str,
        payload: Bytes,
        offline_enabled: bool,
        offline_limit: usize,
    ) -> Result<(), String> {
        if offline_enabled && offline_limit > 0 {
            let online = self
                .subscriptions
                .read()
                .map(|m| m.get(topic).copied().unwrap_or(0))
                .unwrap_or(0);
            if online == 0 {
                let mut guard = self.offline.write().map_err(|e| e.to_string())?;
                let q = guard.entry(topic.to_string()).or_default();
                if q.len() >= offline_limit {
                    q.pop_front();
                }
                q.push_back(OfflineMsg {
                    payload: payload.clone(),
                });
            }
        }

        let mut link = self.publish_link.lock().map_err(|e| e.to_string())?;
        let topic_bytes = Bytes::copy_from_slice(topic.as_bytes());
        link.publish(topic_bytes, payload).map(|_| ()).map_err(|e| e.to_string())
    }

    /// 订阅登记（在线数 +1）
    pub fn subscribe_topic(&self, topic: &str) {
        if let Ok(mut m) = self.subscriptions.write() {
            *m.entry(topic.to_string()).or_insert(0) += 1;
        }
    }

    /// 取消订阅（在线数 -1）
    pub fn unsubscribe_topic(&self, topic: &str) {
        if let Ok(mut m) = self.subscriptions.write() {
            let n = m.entry(topic.to_string()).or_insert(0);
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(topic);
            }
        }
    }

    /// 取走该主题全部离线消息并清空队列
    pub fn take_offline(&self, topic: &str) -> Vec<OfflineMsg> {
        if let Ok(mut m) = self.offline.write() {
            m.remove(topic).map(|q| q.into_iter().collect()).unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// 处理一条 MQTT 客户端连接（TCP 直连或 WebSocket）
    pub async fn handle_client<S>(&self, stream: S, store: Arc<ConfigStore>)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.connections.fetch_add(1, Ordering::Relaxed);
        let result = self.client_session(stream, store).await;
        self.connections.fetch_sub(1, Ordering::Relaxed);
        if let Err(e) = result {
            debug!("[MQTT] connection closed: {e}");
        }
    }

    async fn client_session<S>(&self, mut stream: S, store: Arc<ConfigStore>) -> Result<(), String>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // 记录本连接成功订阅的主题；连接结束（含异常/超时/对端断开）统一清理在线计数，
        // 否则断开的客户端会残留“在线”标记，导致离线队列永不触发
        let mut subscribed: std::collections::HashSet<String> = std::collections::HashSet::new();
        let result = self.client_session_loop(&mut stream, &store, &mut subscribed).await;
        for t in &subscribed {
            self.unsubscribe_topic(t);
        }
        if !subscribed.is_empty() {
            debug!("[MQTT] session end, cleaned {} subscriptions", subscribed.len());
        }
        result
    }

    async fn client_session_loop<S>(
        &self,
        mut stream: &mut S,
        store: &Arc<ConfigStore>,
        subscribed: &mut std::collections::HashSet<String>,
    ) -> Result<(), String>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut read_buf = BytesMut::with_capacity(4096);

        // ---- 1. 等待 Connect 包 ----
        let connect = match read_packet(&mut stream, &mut read_buf).await? {
            Packet::Connect(c) => c,
            _ => return Err("first packet must be CONNECT".into()),
        };

        // 仅支持 MQTT v3.1.1（官方 aedes 同款范围）
        if connect.protocol != Protocol::V4 {
            let _ = write_packet(
                &mut stream,
                &Packet::ConnAck(ConnAck::new(
                    ConnectReturnCode::RefusedProtocolVersion,
                    false,
                )),
            )
            .await;
            return Err("unsupported protocol version".into());
        }

        // 官方 preConnect：keepalive 60/300/600 → 3600
        let mut keep_alive = connect.keep_alive;
        if matches!(keep_alive, 60 | 300 | 600) {
            keep_alive = 3600;
        }

        let client_id = if connect.client_id.is_empty() {
            format!("anon-{:?}", std::time::SystemTime::now())
        } else {
            connect.client_id.clone()
        };

        // ---- 2. 注册到路由内核（dynamic_filters=true，动态主题可订阅）----
        let (mut link_tx, mut link_rx) = (self.make_link)(&client_id)
            .map_err(|e| format!("broker link: {e}"))?;

        // ---- 3. ConnAck ----
        write_packet(&mut stream, &Packet::ConnAck(ConnAck::new(ConnectReturnCode::Success, false)))
            .await?;
        info!("[MQTT] client connected: {client_id} (keepalive={keep_alive})");

        // ---- 4. 双向转发循环 ----
        // 读超时：keepalive*2 + 10s（防半开连接泄漏；官方 3600 意图下即约 2 小时）
        let idle_timeout = Duration::from_secs(keep_alive as u64 * 2 + 10);
        let mut pkid: u16 = 0;

        loop {
            tokio::select! {
                // socket → 协议包
                r = tokio::time::timeout(idle_timeout, read_packet(&mut stream, &mut read_buf)) => {
                    match r {
                        Err(_) => return Err("read timeout (keepalive)".into()),
                        Ok(Err(e)) => return Err(e),
                        Ok(Ok(pkt)) => {
                            let next = handle_inbound(
                                self,
                                pkt,
                                &client_id,
                                &mut stream,
                                &store,
                                &mut link_tx,
                                &mut pkid,
                                subscribed,
                            )
                            .await?;
                            if !next {
                                return Ok(());
                            }
                        }
                    }
                }
                // 路由内核 → 订阅客户端
                n = link_rx.next() => {
                    match n {
                        Ok(Some(Notification::Forward(fwd))) => {
                            pkid = pkid.wrapping_add(1);
                            if pkid == 0 { pkid = 1; }
                            let topic = String::from_utf8_lossy(&fwd.publish.topic).to_string();
                            let out = Packet::Publish(Publish {
                                dup: false,
                                qos: QoS::AtLeastOnce,
                                retain: false,
                                topic,
                                pkid,
                                payload: fwd.publish.payload.clone(),
                            });
                            write_packet(&mut stream, &out).await?;
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => return Err("router closed link".into()),
                        Err(e) => return Err(format!("router: {e}")),
                    }
                }
            }
        }
    }
}

/// 处理客户端 → 服务端的 MQTT 包。返回 false 表示结束连接。
///
/// 离线消息语义：订阅成功（ACL 通过）后，把该主题缓存的离线消息
/// 以 qos1 补发给该客户端，并清空队列；取消订阅时在线计数 -1。
async fn handle_inbound<S>(
    hub: &MqttHub,
    pkt: Packet,
    client_id: &str,
    stream: &mut S,
    store: &Arc<ConfigStore>,
    link_tx: &mut LinkTx,
    pkid: &mut u16,
    subscribed: &mut std::collections::HashSet<String>,
) -> Result<bool, String>
where
    S: AsyncWrite + Unpin,
{
    match pkt {
        Packet::PingReq => {
            write_packet(stream, &Packet::PingResp).await?;
        }

        Packet::Subscribe(sub) => {
            let keys = store.get().key_set();
            let mut codes = Vec::new();
            for f in &sub.filters {
                if !f.path.is_empty() && keys.contains(&f.path) {
                    link_tx
                        .subscribe(f.path.clone())
                        .map_err(|e| format!("subscribe: {e}"))?;
                    info!("[MQTT] {client_id} subscribed: {}", f.path);
                    // 在线订阅计数 +1（离线队列判据）
                    hub.subscribe_topic(&f.path);
                    subscribed.insert(f.path.clone());
                    // 订阅成功后补发离线消息（重连场景）
                    let pending = hub.take_offline(&f.path);
                    if !pending.is_empty() {
                        info!("[MQTT] {client_id} offline replay: {} ({} msgs)", f.path, pending.len());
                    }
                    for m in pending {
                        *pkid = pkid.wrapping_add(1);
                        if *pkid == 0 {
                            *pkid = 1;
                        }
                        let out = Packet::Publish(Publish {
                            dup: false,
                            qos: QoS::AtLeastOnce,
                            retain: false,
                            topic: f.path.clone(),
                            pkid: *pkid,
                            payload: m.payload.clone(),
                        });
                        write_packet(stream, &out).await?;
                    }
                    codes.push(SubscribeReasonCode::Success(f.qos));
                } else {
                    warn!("[MQTT] {client_id} unauthorized subscribe: {:?}", f.path);
                    codes.push(SubscribeReasonCode::Failure);
                }
            }
            write_packet(stream, &Packet::SubAck(SubAck::new(sub.pkid, codes))).await?;
        }

        Packet::Unsubscribe(unsub) => {
            for f in &unsub.topics {
                let _ = link_tx.unsubscribe(f.clone());
                // 在线订阅计数 -1
                hub.unsubscribe_topic(f);
                subscribed.remove(f);
                info!("[MQTT] {client_id} unsubscribed: {f}");
            }
            write_packet(stream, &Packet::UnsubAck(UnsubAck::new(unsub.pkid))).await?;
        }

        Packet::Publish(p) => {
            let keys = store.get().key_set();
            if p.topic.is_empty() || !keys.contains(&p.topic) {
                // 对齐官方 aedes：非法 publish 直接断开该客户端
                warn!("[MQTT] {client_id} unauthorized publish: {} (disconnect)", p.topic);
                return Ok(false);
            }
            link_tx
                .publish(p.topic.clone(), p.payload.clone())
                .map_err(|e| format!("publish: {e}"))?;
            debug!("[MQTT] {client_id} published: {} ({} bytes)", p.topic, p.payload.len());
            if p.qos != QoS::AtMostOnce {
                write_packet(stream, &Packet::PubAck(PubAck::new(p.pkid))).await?;
            }
        }

        // 客户端对我们 qos1 投递的确认 → 忽略
        Packet::PubAck(_) | Packet::PubRec(_) | Packet::PubComp(_) => {}
        // qos2 确认闭环（PushMe App 实际不用 qos2）
        Packet::PubRel(rel) => {
            write_packet(stream, &Packet::PubComp(PubComp::new(rel.pkid))).await?;
        }

        Packet::Disconnect => {
            info!("[MQTT] {client_id} disconnected");
            return Ok(false);
        }
        Packet::Connect(_) => return Err("duplicate CONNECT".into()),
        _ => {}
    }
    Ok(true)
}

/// 从流中读出一个完整 MQTT 包（不足时继续读）
async fn read_packet<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut BytesMut) -> Result<Packet, String> {
    loop {
        match Packet::read(buf, MAX_PACKET_SIZE) {
            Ok(pkt) => return Ok(pkt),
            Err(mqttbytes::Error::InsufficientBytes(needed)) => {
                buf.reserve(needed.max(1024));
                let n = stream
                    .read_buf(buf)
                    .await
                    .map_err(|e| format!("socket read: {e}"))?;
                if n == 0 {
                    return Err("connection closed by peer".into());
                }
            }
            Err(e) => return Err(format!("malformed packet: {e}")),
        }
    }
}

/// 序列化并写出一个 MQTT 包
async fn write_packet<S: AsyncWrite + Unpin>(stream: &mut S, pkt: &Packet) -> Result<(), String> {
    let mut out = BytesMut::new();
    pkt.write(&mut out, MAX_PACKET_SIZE)
        .map_err(|e| format!("encode: {e}"))?;
    stream
        .write_all(&out)
        .await
        .map_err(|e| format!("socket write: {e}"))?;
    stream.flush().await.map_err(|e| format!("flush: {e}"))
}
