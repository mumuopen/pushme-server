//! 鉴权与安全：
//! - 官方 md5 加盐算法兼容（迁移官方 config/data.json 可直接登录）
//! - bcrypt 密码哈希（官方 md5 密码首次登录自动升级）
//! - HMAC-SHA256 签名 session cookie（替代官方明文用户名 cookie，更安全）
//! - 登录失败限速：全局 5 次失败锁 3 分钟（对齐官方 login.js）
//! - 面板 IP 白名单（支持 IPv4 CIDR）

use crate::config::ConfigStore;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SESSION_COOKIE: &str = "pushme_session";
/// session 有效期：7 天
const SESSION_TTL_SECS: u64 = 7 * 24 * 3600;
/// 登录失败上限（对齐官方）
const MAX_LOGIN_FAILS: u32 = 5;
/// 锁定时长（对齐官方 3 分钟）
const LOGIN_LOCK_SECS: u64 = 3 * 60;

fn md5_hex(input: &[u8]) -> String {
    let digest = md5::compute(input);
    format!("{:x}", digest)
}

/// 官方多层加盐 md5：md5(salt + md5(salt + md5(str + salt) + salt))，salt='pushme'
/// 对齐官方 app/controller/base.js `_md5()`
pub fn md5_pushme(input: &str) -> String {
    const SALT: &str = "pushme";
    let a = md5_hex(format!("{input}{SALT}").as_bytes());
    let b = md5_hex(format!("{SALT}{a}{SALT}").as_bytes());
    md5_hex(format!("{SALT}{b}").as_bytes())
}

/// bcrypt 哈希密码
pub fn bcrypt_hash(password: &str) -> Result<String, String> {
    bcrypt::hash(password, bcrypt::DEFAULT_COST).map_err(|e| e.to_string())
}

pub fn bcrypt_verify(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

/// 安装：写入管理员账号（官方双 md5 字段 + bcrypt 扩展字段）
pub fn install(store: &ConfigStore, username: &str, password: &str) -> Result<(), String> {
    let hash = bcrypt_hash(password)?;
    store
        .update(|c| {
            c.admin_user = username.to_string();
            c.pass_hash = hash;
            c.user = md5_pushme(username);
            c.password = md5_pushme(password);
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 校验登录。官方 md5 密码通过后自动升级为 bcrypt。
/// 返回 Err(原因)；Ok(()) 表示成功
pub fn verify_login(store: &ConfigStore, username: &str, password: &str) -> Result<(), &'static str> {
    let cfg = store.get();

    if !cfg.installed() {
        return Err("系统未安装");
    }

    // 用户名校验（admin_user 为空时走官方 md5 兼容路径）
    let user_ok = if !cfg.admin_user.is_empty() {
        username == cfg.admin_user
    } else {
        md5_pushme(username) == cfg.user
    };
    if !user_ok {
        return Err("账号或密码错误");
    }

    // 密码校验
    if !cfg.pass_hash.is_empty() {
        if !bcrypt_verify(password, &cfg.pass_hash) {
            return Err("账号或密码错误");
        }
        return Ok(());
    }

    // 官方 md5 密码（迁移场景）
    if md5_pushme(password) != cfg.password {
        return Err("账号或密码错误");
    }

    // 升级为 bcrypt（下次登录走 bcrypt）
    if let Ok(hash) = bcrypt_hash(password) {
        let _ = store.update(|c| c.pass_hash = hash);
    }
    Ok(())
}

// ------------------------- session -------------------------

/// HMAC 签名 session：token = base64url(username).exp.hmac
pub struct Sessions {
    key: Vec<u8>,
}

impl Sessions {
    pub fn new(session_key_b64: &str) -> Self {
        use base64::Engine;
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(session_key_b64)
            .unwrap_or_else(|_| session_key_b64.as_bytes().to_vec());
        Self {
            key: if key.is_empty() { b"pushme-default".to_vec() } else { key },
        }
    }

    fn sign(&self, payload: &str) -> Result<String, String> {
        let mut mac = <Hmac<Sha256>>::new_from_slice(&self.key).map_err(|e| e.to_string())?;
        mac.update(payload.as_bytes());
        Ok(hex(&mac.finalize().into_bytes()))
    }

    /// 签发 session token
    pub fn issue(&self, username: &str) -> Result<String, String> {
        let user_b64 = crate::config::use_base64(username.as_bytes());
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + SESSION_TTL_SECS;
        let payload = format!("{user_b64}.{exp}");
        let sig = self.sign(&payload)?;
        Ok(format!("{payload}.{sig}"))
    }

    /// 校验 token → 用户名
    pub fn verify(&self, token: &str) -> Option<String> {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut parts = token.splitn(3, '.');
        let (user_b64, exp, sig) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let expected = self.sign(&format!("{user_b64}.{exp}")).ok()?;
        if !constant_time_eq(sig.as_bytes(), expected.as_bytes()) {
            return None;
        }
        let exp: u64 = exp.parse().ok()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now >= exp {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(user_b64).ok()?;
        let username = String::from_utf8(bytes).ok()?;
        if username.is_empty() {
            return None;
        }
        Some(username)
    }
}

/// 常数时间比较（防时序侧信道）
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------- 登录限速 -------------------------

/// 全局登录失败限速（对齐官方：5 次失败锁 3 分钟）
pub struct LoginGuard {
    state: Mutex<GuardState>,
}

struct GuardState {
    fails: u32,
    locked_until: Option<Instant>,
}

impl LoginGuard {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(GuardState { fails: 0, locked_until: None }),
        }
    }

    /// 是否处于锁定中；返回剩余秒数
    pub fn lock_remaining(&self) -> u64 {
        let st = self.state.lock().unwrap();
        match st.locked_until {
            Some(t) if t > Instant::now() => {
                (t - Instant::now()).as_secs().max(1)
            }
            _ => 0,
        }
    }

    /// 是否处于锁定中
    #[allow(dead_code)]
    pub fn is_locked(&self) -> bool {
        self.lock_remaining() > 0
    }

    pub fn record_fail(&mut self) -> u32 {
        let mut st = self.state.lock().unwrap();
        // 锁定过期则重新计数
        if matches!(st.locked_until, Some(t) if t <= Instant::now()) {
            st.fails = 0;
            st.locked_until = None;
        }
        st.fails += 1;
        if st.fails >= MAX_LOGIN_FAILS {
            st.locked_until = Some(Instant::now() + Duration::from_secs(LOGIN_LOCK_SECS));
            st.fails = 0;
            return 0; // 触发锁定
        }
        MAX_LOGIN_FAILS - st.fails // 剩余机会
    }

    pub fn reset(&self) {
        let mut st = self.state.lock().unwrap();
        st.fails = 0;
        st.locked_until = None;
    }
}

// ------------------------- IP 白名单 -------------------------

/// 检查 IP 是否在白名单内（空列表 = 不限制）
/// 支持：精确 IP / IPv4 CIDR（如 192.168.1.0/24）
pub fn ip_allowed(allowed: &[String], ip: IpAddr) -> bool {
    if allowed.is_empty() {
        return true;
    }
    for entry in allowed {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some((net, prefix)) = parse_cidr_v4(entry) {
            if let IpAddr::V4(v4) = ip {
                if ipv4_match(v4, net, prefix) {
                    return true;
                }
            }
            continue;
        }
        if let Ok(target) = entry.parse::<IpAddr>() {
            if target == ip {
                return true;
            }
        }
    }
    false
}

fn parse_cidr_v4(s: &str) -> Option<(std::net::Ipv4Addr, u32)> {
    let (net, prefix) = s.split_once('/')?;
    let net: std::net::Ipv4Addr = net.parse().ok()?;
    let prefix: u32 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    Some((net, prefix))
}

fn ipv4_match(ip: std::net::Ipv4Addr, net: std::net::Ipv4Addr, prefix: u32) -> bool {
    if prefix == 0 {
        return true;
    }
    let mask: u32 = if prefix == 32 { u32::MAX } else { !((1u32 << (32 - prefix)) - 1) };
    let ip_u = u32::from(ip);
    let net_u = u32::from(net);
    (ip_u & mask) == (net_u & mask)
}
