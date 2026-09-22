//! 配置管理：兼容官方 pushme-server 的 config/data.json 格式
//! 扩展字段：panel_bind / public_panel / pass_hash / session_key（带默认值，旧配置可直接迁移）

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};

pub const CONFIG_VERSION: i64 = 2;

/// 单个推送 key（官方格式：key + temp_key + note）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PushKey {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub temp_key: String,
    #[serde(default)]
    pub note: String,
}

/// 官方 data.json 兼容结构（字段名与官方完全一致 + 扩展字段）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_config_version")]
    pub config_version: i64,
    /// PushMe 消息服务端口（MQTT/WS/推送API 单端口）
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    /// Web 面板端口
    #[serde(default = "default_panel_port")]
    pub panel_port: u16,
    /// 扩展：面板绑定地址（默认仅本机，安全默认）
    #[serde(default = "default_panel_bind")]
    pub panel_bind: String,
    /// 扩展：公网端口是否暴露面板路由（默认 false，修复 mumuopen 版公网暴露 admin 的问题）
    #[serde(default)]
    pub public_panel: bool,
    /// 推送 key 列表
    #[serde(default)]
    pub push_keys: Vec<serde_json::Value>,
    /// 官方格式：用户名（多层 md5 加盐后）
    #[serde(default)]
    pub user: String,
    /// 官方格式：密码（多层 md5 加盐后）
    #[serde(default)]
    pub password: String,
    /// 扩展：bcrypt 密码哈希（优先于官方 md5）
    #[serde(default)]
    pub pass_hash: String,
    /// 明文用户名（官方只存 md5，我们额外存明文用于显示；为空则未安装）
    #[serde(default)]
    pub admin_user: String,
    /// 扩展：面板 IP 白名单（CIDR 支持），空=不限制
    #[serde(default)]
    pub panel_allowed_ips: Vec<String>,
    /// 消息服务 TLS：none | public | self
    #[serde(default = "default_tls")]
    pub tls: String,
    /// 面板 TLS：none | tls
    #[serde(default = "default_panel_tls")]
    pub panel_tls: String,
    /// 服务状态：start | stop
    #[serde(default = "default_status")]
    pub status: String,
    /// 消息计数
    #[serde(default)]
    pub message_count: i64,
    /// 扩展：离线消息推送开关（发布时无在线订阅者则缓存，重连后补发）
    #[serde(default)]
    pub offline_messages: bool,
    /// 扩展：离线消息缓存条数上限
    #[serde(default = "default_offline_limit")]
    pub offline_limit: usize,
    /// 扩展：session cookie HMAC 密钥（base64）
    #[serde(default)]
    pub session_key: String,
}

fn default_config_version() -> i64 {
    CONFIG_VERSION
}
fn default_server_port() -> u16 {
    3100
}
fn default_panel_port() -> u16 {
    3010
}
fn default_panel_bind() -> String {
    "127.0.0.1".to_string()
}
fn default_tls() -> String {
    "none".to_string()
}
fn default_panel_tls() -> String {
    "none".to_string()
}
fn default_status() -> String {
    "start".to_string()
}
fn default_offline_limit() -> usize {
    50
}

impl Default for AppConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl AppConfig {
    /// 是否已完成安装（有管理员账号）
    /// 兼容官方 data.json：官方无 admin_user 字段，user+password 均有值即视为已安装
    pub fn installed(&self) -> bool {
        if !self.admin_user.is_empty() {
            return !self.pass_hash.is_empty()
                || (!self.user.is_empty() && !self.password.is_empty());
        }
        // 官方/历史配置直接迁移
        !self.user.is_empty() && !self.password.is_empty()
    }

    /// 归一化后的 push_keys（字符串或对象两种形态都兼容官方）
    pub fn push_keys(&self) -> Vec<PushKey> {
        self.push_keys
            .iter()
            .filter_map(|v| {
                if v.is_string() {
                    Some(PushKey {
                        key: v.as_str()?.to_string(),
                        ..Default::default()
                    })
                } else {
                    serde_json::from_value(v.clone()).ok()
                }
            })
            .filter(|k| !k.key.is_empty())
            .collect()
    }

    /// push_key 白名单集合（ACL 用）
    pub fn key_set(&self) -> std::collections::HashSet<String> {
        self.push_keys().into_iter().map(|k| k.key).collect()
    }

    /// temp_key 反查真实 push_key
    pub fn find_by_temp_key(&self, temp_key: &str) -> Option<String> {
        if temp_key.is_empty() {
            return None;
        }
        self.push_keys()
            .into_iter()
            .find(|k| k.temp_key == temp_key)
            .map(|k| k.key)
    }
}

/// 线程安全的配置存储（内存缓存 + 原子写盘，对齐官方 ConfigManager 行为）
pub struct ConfigStore {
    path: PathBuf,
    cache: RwLock<Arc<AppConfig>>,
    message_count: AtomicI64,
}

impl ConfigStore {
    pub fn load(base_dir: &Path) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(base_dir.join("config").join("certs"))?;
        let path = base_dir.join("config").join("data.json");
        let mut cfg: AppConfig = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => AppConfig::default(),
        };

        // 首次运行：生成 session_key（不存在时）
        let mut dirty = false;
        if cfg.session_key.is_empty() {
            let bytes: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
            cfg.session_key = use_base64(&bytes);
            dirty = true;
        }
        if cfg.config_version < CONFIG_VERSION {
            cfg.config_version = CONFIG_VERSION;
            dirty = true;
        }

        let store = Self {
            path,
            cache: RwLock::new(Arc::new(cfg.clone())),
            message_count: AtomicI64::new(cfg.message_count),
        };
        if dirty {
            store.persist(&cfg)?;
        } else {
            store.persist(&cfg)?; // 确保首次也生成 data.json
        }
        Ok(Arc::new(store))
    }

    pub fn get(&self) -> Arc<AppConfig> {
        self.cache.read().unwrap().clone()
    }

    /// 更新配置（内存 + 写盘，原子替换）
    pub fn update<F: FnOnce(&mut AppConfig)>(&self, f: F) -> std::io::Result<Arc<AppConfig>> {
        let mut cfg = (*self.get()).clone();
        cfg.message_count = self.message_count.load(Ordering::Relaxed);
        f(&mut cfg);
        self.persist(&cfg)?;
        let arc = Arc::new(cfg);
        *self.cache.write().unwrap() = arc.clone();
        Ok(arc)
    }

    fn persist(&self, cfg: &AppConfig) -> std::io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(cfg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn incr_message_count(&self, n: i64) {
        self.message_count.fetch_add(n, Ordering::Relaxed);
    }

    pub fn message_count(&self) -> i64 {
        self.message_count.load(Ordering::Relaxed)
    }
}

pub fn use_base64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}
