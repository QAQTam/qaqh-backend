//! daemon 传输安全（移动端 M0；plan §4.1 / spec-daemon-auth-devices §10）。
//!
//! 自签证书生成并持久化于 `<data_dir>/tls/{cert,key}.pem`，rustls acceptor 包裹
//! `axum::serve`。**证书轮换 = 删除文件重生成 = 全部设备重扫码**（写进配对文档）。
//!
//! 指纹经 `/ringing/v2/pairing/tokens` 进二维码，供原生端 pinning 防 LAN 内 MITM。
//! 实现取叶证书 **DER** 的 sha256（`sha256:<hex>`）作为 pin 值；与规范建议的 SPKI
//! 指纹等价可用（不引 x509 解析器），原生端按此字节序列 pin 即可。
//!
//! 回环 bind 不启用 TLS：桌面壳 / daemon-CLI / TUI / 探针走 `http://localhost`
//! 行为零变化。非回环 bind 自动启用 TLS。

use std::path::Path;
use std::sync::Arc;

use tokio_rustls::TlsAcceptor;

/// TLS 就绪后的服务端材料：acceptor + 供配对的证书指纹。
pub struct TlsMaterial {
    pub acceptor: TlsAcceptor,
    /// `sha256:<hex>`（叶证书 DER 的 SHA-256）。
    pub fingerprint: String,
}

fn build_material(cert_pem: &str, key_pem: &str) -> Result<TlsMaterial, String> {
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("parse cert pem: {error}"))?;
    let leaf = certs
        .first()
        .cloned()
        .ok_or_else(|| "empty cert chain".to_string())?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .map_err(|error| format!("parse key pem: {error}"))?
        .ok_or_else(|| "no private key in pem".to_string())?;

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("rustls protocol versions: {error}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| format!("build rustls config: {error}"))?;

    let fingerprint = format!("sha256:{}", qaqh_types::sha256_hex(leaf.as_ref()));
    Ok(TlsMaterial {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        fingerprint,
    })
}

/// 载入持久化自签证书；不存在则生成并落盘。
pub fn load_or_generate(data_dir: &Path) -> Result<TlsMaterial, String> {
    let dir = data_dir.join("tls");
    std::fs::create_dir_all(&dir).map_err(|error| format!("create tls dir: {error}"))?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    let (cert_pem, key_pem) = if cert_path.exists() && key_path.exists() {
        let cert = std::fs::read_to_string(&cert_path).map_err(|e| format!("read cert: {e}"))?;
        let key = std::fs::read_to_string(&key_path).map_err(|e| format!("read key: {e}"))?;
        (cert, key)
    } else {
        let certified = rcgen::generate_simple_self_signed(vec![
            "qaqh-daemon".to_string(),
            "localhost".to_string(),
        ])
        .map_err(|error| format!("generate self-signed cert: {error}"))?;
        let cert = certified.cert.pem();
        let key = certified.signing_key.serialize_pem();
        std::fs::write(&cert_path, &cert).map_err(|e| format!("write cert: {e}"))?;
        std::fs::write(&key_path, &key).map_err(|e| format!("write key: {e}"))?;
        (cert, key)
    };

    build_material(&cert_pem, &key_pem)
}

/// `axum::serve` 的 TLS 监听器：每接受一个 TCP 连接即完成 TLS 握手，
/// 握手失败（非客户端发起的 TLS）直接跳过该连接。
pub struct TlsListener {
    inner: tokio::net::TcpListener,
    acceptor: TlsAcceptor,
}

impl TlsListener {
    pub fn new(inner: tokio::net::TcpListener, acceptor: TlsAcceptor) -> Self {
        Self { inner, acceptor }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // axum 0.8 的 `Listener::accept` 不可失败：TCP accept 错误与 TLS 握手失败
        // 都在此吞掉重试（accept 错误加短暂退避，避免忙等）。
        loop {
            match self.inner.accept().await {
                Ok((stream, addr)) => match self.acceptor.accept(stream).await {
                    Ok(tls) => return (tls, addr),
                    Err(error) => {
                        log::debug!("[tls] handshake failed from {addr}: {error}");
                        continue;
                    }
                },
                Err(error) => {
                    log::warn!("[tls] accept error: {error}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}
