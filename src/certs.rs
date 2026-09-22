//! TLS 证书：rcgen 自签名生成 + rustls 加载
//! 文件对齐官方：config/certs/cert.crt + config/certs/private.key

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;
use tracing::info;

/// 证书目录 / 文件路径（对齐官方布局）
pub fn certs_dir(base_dir: &Path) -> PathBuf {
    base_dir.join("config").join("certs")
}
pub fn cert_path(base_dir: &Path) -> PathBuf {
    certs_dir(base_dir).join("cert.crt")
}
pub fn key_path(base_dir: &Path) -> PathBuf {
    certs_dir(base_dir).join("private.key")
}

/// 证书文件是否就绪
pub fn files_exist(base_dir: &Path) -> bool {
    cert_path(base_dir).is_file() && key_path(base_dir).is_file()
}

/// 生成自签名证书（SAN = 多个域名/IP，CN = 首个），写入 config/certs/
/// 自动补 127.0.0.1 与 ::1（对齐官方 domains 行为）
pub fn generate_self_signed(base_dir: &Path, names: &[String]) -> Result<(), String> {
    let dir = certs_dir(base_dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let mut list: Vec<String> = names
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .collect();
    if list.is_empty() {
        list.push("localhost".to_string());
    }
    for extra in ["127.0.0.1", "::1"] {
        if !list.iter().any(|n| n == extra) {
            list.push(extra.to_string());
        }
    }
    let cn = list[0].clone();

    // rcgen：IP 字符串自动识别为 IP SAN，其余为 DNS SAN
    let key_pair = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let mut params = rcgen::CertificateParams::new(list.clone())
        .map_err(|e| e.to_string())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, &cn);

    let cert = params.self_signed(&key_pair).map_err(|e| e.to_string())?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    std::fs::write(cert_path(base_dir), &cert_pem).map_err(|e| e.to_string())?;
    std::fs::write(key_path(base_dir), &key_pem).map_err(|e| e.to_string())?;
    info!("[certs] self-signed certificate generated (cn={cn}, san={})", list.join(","));
    Ok(())
}

/// 启动时若 tls=self 且无证书，自动生成一份 localhost 证书（可在面板重新生成）
pub fn ensure_self_signed(base_dir: &Path) -> Result<(), String> {
    if !files_exist(base_dir) {
        generate_self_signed(base_dir, &["localhost".to_string()])?;
    }
    Ok(())
}

/// 加载证书 → TlsAcceptor
pub fn load_tls_acceptor(base_dir: &Path) -> Result<TlsAcceptor, String> {
    let cert_file = std::fs::File::open(cert_path(base_dir)).map_err(|e| format!("打开证书失败: {e}"))?;
    let key_file = std::fs::File::open(key_path(base_dir)).map_err(|e| format!("打开私钥失败: {e}"))?;

    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_file))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("解析证书失败: {e}"))?;

    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .map_err(|e| format!("解析私钥失败: {e}"))?
        .ok_or_else(|| "私钥文件为空".to_string())?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("装配 TLS 配置失败: {e}"))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// 读取证书内容（下载端点用）
pub fn read_cert_pem(base_dir: &Path) -> Option<Vec<u8>> {
    std::fs::read(cert_path(base_dir)).ok()
}
