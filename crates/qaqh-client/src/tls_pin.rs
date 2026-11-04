//! 自签证书的 TLS 指纹 pinning（移动端 LAN 直连）。
//!
//! daemon 的 LAN 模式用 rcgen 自签证书（`qaqh-daemon/src/tls.rs`），QR 配对
//! 载荷携带 `tls_fp`。已核实的事实（qaqh-android HANDOFF.md「tls_fp 语义」）：
//! daemon 实现里 `tls_fp` 是**整张 DER 证书**的 sha256（wire 类型注释写 SPKI，
//! 与实现不符）——本 verifier 按实现语义 pin 整张 DER；指纹不符一律拒绝
//! （fail-closed，不做「SPKI 兼容」猜测）。
//!
//! 信任锚 = 指纹本身，证书链/主机名校验被刻意跳过；但握手签名仍交给
//! provider 算法校验，服务端必须真的持有对应私钥。

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};

/// 解析 `sha256:<hex>` 或裸 hex 指纹为 32 字节。大小写不敏感；长度/字符
/// 不符返回 `None`（调用方转成显式错误，不静默放行）。
pub(crate) fn parse_fingerprint(raw: &str) -> Option<[u8; 32]> {
    let hex = raw.strip_prefix("sha256:").unwrap_or(raw).trim();
    if hex.len() != 64 {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_val(bytes[i * 2])?;
        let lo = hex_val(bytes[i * 2 + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 整张 DER 证书的 sha256（与 daemon `tls_fp` 的实现语义一致）。
pub(crate) fn der_fingerprint(cert: &CertificateDer<'_>) -> [u8; 32] {
    Sha256::digest(cert.as_ref()).into()
}

/// 唯一信任锚是指纹的证书校验器。
#[derive(Debug)]
pub(crate) struct PinnedServerCertVerifier {
    expected: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl PinnedServerCertVerifier {
    pub(crate) fn new(expected: [u8; 32], provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        Self { expected, provider }
    }
}

impl ServerCertVerifier for PinnedServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if der_fingerprint(end_entity) == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "qaqh tls pin: certificate fingerprint mismatch".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_prefixed_and_bare_hex() {
        let hex = "ab".repeat(32);
        let bare = parse_fingerprint(&hex).unwrap();
        let prefixed = parse_fingerprint(&format!("sha256:{}", hex.to_ascii_uppercase())).unwrap();
        assert_eq!(bare, prefixed);
        assert_ne!(bare, [0u8; 32]);
    }

    #[test]
    fn parse_rejects_bad_shapes() {
        assert_eq!(parse_fingerprint(""), None);
        assert_eq!(parse_fingerprint("sha256:abcd"), None);
        assert_eq!(parse_fingerprint(&"g".repeat(64)), None);
        assert_eq!(parse_fingerprint(&"a".repeat(63)), None);
    }

    #[test]
    fn der_fingerprint_is_sha256_of_der() {
        let der = CertificateDer::from_slice(&[0x30, 0x82, 0x01, 0x00]);
        let expected: [u8; 32] = Sha256::digest([0x30u8, 0x82, 0x01, 0x00]).into();
        assert_eq!(der_fingerprint(&der), expected);
    }

    fn verifier_for(cert: &CertificateDer<'static>) -> PinnedServerCertVerifier {
        let expected = der_fingerprint(cert);
        PinnedServerCertVerifier::new(
            expected,
            Arc::new(rustls::crypto::ring::default_provider()),
        )
    }

    fn localhost_cert() -> CertificateDer<'static> {
        rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .expect("generate self-signed cert")
            .cert
            .der()
            .clone()
    }

    /// 信任锚 = 指纹本身：匹配即接受，不校验链/主机名。
    #[test]
    fn verifier_accepts_certificate_matching_pin() {
        let cert = localhost_cert();
        let verifier = verifier_for(&cert);
        let name = ServerName::try_from("localhost".to_string()).expect("server name");
        // Ok(_) 即接受——任何拒绝都会以 Err 返回,expect 已是完整断言。
        let _verdict = verifier
            .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
            .expect("pinned cert must be accepted");
    }

    #[test]
    fn verifier_rejects_certificate_with_wrong_fingerprint() {
        let cert = localhost_cert();
        let wrong = {
            let mut fp = der_fingerprint(&cert);
            fp[0] ^= 0xff;
            fp
        };
        let verifier = PinnedServerCertVerifier::new(
            wrong,
            Arc::new(rustls::crypto::ring::default_provider()),
        );
        let name = ServerName::try_from("localhost".to_string()).expect("server name");
        let error = verifier
            .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
            .expect_err("mismatched pin must be rejected");
        assert!(error.to_string().contains("fingerprint mismatch"));
    }
}
