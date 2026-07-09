//! Trae 登录的密码学原语:设备密钥对(EC P-256)、PKCE(S256)、refresh 的
//! `DeviceProof` 签名(ECDSA-P256-SHA256 + DER + base64)。
//!
//! ## 与 Trae 主进程对齐的关键点
//!
//! - **密钥对**:`crypto.generateKeyPairSync("ec", {namedCurve:"P-256", ...})`
//!   → private PKCS8 PEM、public SPKI PEM。**持久化复用**(公钥首登上传、服务端
//!   按 device 绑定,refresh 用私钥验签;每次新生成会让 refresh 全挂)。
//! - **PKCE**:`code_verifier = base64url(randomBytes(48))`,
//!   `code_challenge = base64url(sha256(verifier))`,method `S256`。
//! - **签名(`fbe`)**:canonical string =
//!   `[method, path, ClientID, RefreshToken, timestamp_secs, nonce_hex].join("\n")`,
//!   再 `crypto.sign("sha256", buf, ecPrivateKey)` —— Node 对 EC key 默认输出
//!   **ASN.1 DER** 编码的 ECDSA 签名(**不是** raw `r||s`),所以这里用
//!   [`p256::ecdsa::Signature::to_der`] 转 DER 再 base64,否则服务端验签失败。
//!   ⚠️ 只有 **refresh** 流带 DeviceProof;首次 authCode 换 token **不签名**。

use base64::Engine;
use serde::{Deserialize, Serialize};

/// 密码学操作错误。
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("OS RNG 不可用: {0}")]
    Rng(String),
    #[error("EC P-256 密钥对生成失败")]
    KeyGen,
    #[error("私钥 PEM 编码失败: {0}")]
    KeyEncode(String),
    #[error("私钥 PEM 解析失败: {0}")]
    KeyParse(String),
}

/// 一对 PEM 编码的 EC P-256 设备密钥(持久化进凭证包)。
///
/// `private_pkcs8_pem` 是长期 secret(refresh 验签用),手写 [`std::fmt::Debug`] 脱敏。
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceKeyPair {
    /// PKCS8 PEM 私钥(`-----BEGIN PRIVATE KEY-----`)。
    pub private_pkcs8_pem: String,
    /// SPKI PEM 公钥(`-----BEGIN PUBLIC KEY-----`)—— 作 `DeviceInfo.DevicePublicKey` 上传。
    pub public_spki_pem: String,
}

impl std::fmt::Debug for DeviceKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceKeyPair")
            .field("private_pkcs8_pem", &"<redacted>")
            .field("public_spki_pem", &"<spki pem>")
            .finish()
    }
}

impl DeviceKeyPair {
    /// 新生成一对 EC P-256 密钥(PKCS8 私钥 PEM + SPKI 公钥 PEM)。
    pub fn generate() -> Result<Self, CryptoError> {
        use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
        use p256::SecretKey;

        // 随机 32 字节 → SecretKey(>= 曲线阶的概率约 2^-32,极小,失败重试几次)。
        for _ in 0..8 {
            let mut bytes = [0u8; 32];
            getrandom::getrandom(&mut bytes).map_err(|e| CryptoError::Rng(e.to_string()))?;
            let Ok(sk) = SecretKey::from_slice(&bytes) else {
                continue;
            };
            let private_pkcs8_pem = sk
                .to_pkcs8_pem(LineEnding::LF)
                .map_err(|e| CryptoError::KeyEncode(e.to_string()))?
                .to_string();
            let public_spki_pem = sk
                .public_key()
                .to_public_key_pem(LineEnding::LF)
                .map_err(|e| CryptoError::KeyEncode(e.to_string()))?;
            return Ok(Self {
                private_pkcs8_pem,
                public_spki_pem,
            });
        }
        Err(CryptoError::KeyGen)
    }
}

/// PKCE 一对 —— 复用共享 [`crate::pkce::PkcePair`](与 grok_build / qoder 同一实现,去重)。
pub use crate::pkce::PkcePair;

/// 生成 PKCE pair(S256)—— 复用共享 [`crate::pkce`]。
pub fn generate_pkce() -> Result<PkcePair, CryptoError> {
    crate::pkce::generate().map_err(CryptoError::Rng)
}

/// refresh 请求 body 里的 `DeviceProof`(对齐 Trae `{Signature, Timestamp, Nonce}`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceProof {
    #[serde(rename = "Signature")]
    pub signature: String,
    #[serde(rename = "Timestamp")]
    pub timestamp: i64,
    #[serde(rename = "Nonce")]
    pub nonce: String,
}

/// 生成 16 字节 hex nonce(`crypto.randomBytes(16).toString("hex")`,32 字符)。
fn generate_nonce() -> Result<String, CryptoError> {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).map_err(|e| CryptoError::Rng(e.to_string()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// 拼 canonical string(签名覆盖内容),抽出来便于单测。
///
/// 顺序与 Trae `fbe` 完全一致:`[method, path, client_id, refresh_token, ts, nonce]`
/// 用 `\n` join。注意第 4 段是 **RefreshToken 字符串本身**,不是整个 JSON body。
fn canonical_string(
    method: &str,
    path: &str,
    client_id: &str,
    refresh_token: &str,
    timestamp: i64,
    nonce: &str,
) -> String {
    [
        method,
        path,
        client_id,
        refresh_token,
        &timestamp.to_string(),
        nonce,
    ]
    .join("\n")
}

/// 对 refresh 请求签名,产出 [`DeviceProof`]。
///
/// `timestamp` 由调用方传入(UNIX 秒)便于测试可复现;生产传 `unix_now_secs()`。
pub fn build_device_proof(
    private_pkcs8_pem: &str,
    method: &str,
    path: &str,
    client_id: &str,
    refresh_token: &str,
    timestamp: i64,
) -> Result<DeviceProof, CryptoError> {
    use p256::ecdsa::{signature::Signer, Signature, SigningKey};
    use p256::pkcs8::DecodePrivateKey;

    let nonce = generate_nonce()?;
    let canonical = canonical_string(method, path, client_id, refresh_token, timestamp, &nonce);

    let signing_key = SigningKey::from_pkcs8_pem(private_pkcs8_pem)
        .map_err(|e| CryptoError::KeyParse(e.to_string()))?;
    // p256 ECDSA 默认 SHA-256;.to_der() 转 ASN.1 DER(对齐 Node crypto.sign 输出)。
    let sig: Signature = signing_key.sign(canonical.as_bytes());
    let der = sig.to_der();
    let signature = base64::engine::general_purpose::STANDARD.encode(der.as_bytes());

    Ok(DeviceProof {
        signature,
        timestamp,
        nonce,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn keypair_generates_valid_pem() {
        let kp = DeviceKeyPair::generate().unwrap();
        assert!(kp.private_pkcs8_pem.contains("BEGIN PRIVATE KEY"));
        assert!(kp.public_spki_pem.contains("BEGIN PUBLIC KEY"));
        // 私钥 PEM 必须能被 SigningKey 重新解析(持久化往返)
        use p256::ecdsa::SigningKey;
        use p256::pkcs8::DecodePrivateKey;
        SigningKey::from_pkcs8_pem(&kp.private_pkcs8_pem).expect("私钥 PEM 应可重新解析");
    }

    #[test]
    fn debug_redacts_private_key() {
        let kp = DeviceKeyPair::generate().unwrap();
        let dbg = format!("{kp:?}");
        assert!(
            !dbg.contains("BEGIN PRIVATE KEY"),
            "私钥不该出现在 Debug: {dbg}"
        );
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = generate_pkce().unwrap();
        // 手算 base64url(sha256(verifier)) 对比
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expect);
        // verifier 无 padding、url-safe
        assert!(!p.verifier.contains('='));
        assert!(!p.verifier.contains('+'));
        assert!(!p.verifier.contains('/'));
    }

    #[test]
    fn canonical_string_field_order() {
        let s = canonical_string("POST", "/p", "cid", "rt", 1700000000, "abcd");
        assert_eq!(s, "POST\n/p\ncid\nrt\n1700000000\nabcd");
    }

    #[test]
    fn nonce_is_32_hex_chars() {
        let n = generate_nonce().unwrap();
        assert_eq!(n.len(), 32);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// 核心回归锁:签名必须是 **DER 编码**(对齐 Node `crypto.sign`),且能被对应
    /// 公钥 + SHA-256 验过 —— 等价于服务端验签路径。
    #[test]
    fn device_proof_signature_is_der_and_verifies() {
        use p256::ecdsa::{signature::Verifier, DerSignature, SigningKey, VerifyingKey};
        use p256::pkcs8::{DecodePrivateKey, DecodePublicKey};

        let kp = DeviceKeyPair::generate().unwrap();
        let proof = build_device_proof(
            &kp.private_pkcs8_pem,
            "POST",
            "/trae/api/v3/oauth/ExchangeToken",
            "en1oxy7wnw8j9n",
            "refresh-token-xyz",
            1_700_000_000,
        )
        .unwrap();

        // 1. 必须是合法 base64
        let der_bytes = base64::engine::general_purpose::STANDARD
            .decode(&proof.signature)
            .expect("签名应是合法 base64");
        // 2. 必须是合法 DER ECDSA 签名(非 raw r||s 的 64 字节定长)
        let der_sig = DerSignature::try_from(der_bytes.as_slice()).expect("签名应是合法 ASN.1 DER");

        // 3. 用公钥 + SHA-256 验签(重建 canonical string;直接用 DER 签名验,
        //    走 Verifier<DerSignature> 这条 impl)
        let canonical = canonical_string(
            "POST",
            "/trae/api/v3/oauth/ExchangeToken",
            "en1oxy7wnw8j9n",
            "refresh-token-xyz",
            proof.timestamp,
            &proof.nonce,
        );
        let vk = VerifyingKey::from_public_key_pem(&kp.public_spki_pem).unwrap();
        vk.verify(canonical.as_bytes(), &der_sig)
            .expect("公钥应能验过自己私钥的签名");

        // sanity:私钥确实对应该公钥
        let sk = SigningKey::from_pkcs8_pem(&kp.private_pkcs8_pem).unwrap();
        assert_eq!(VerifyingKey::from(&sk).to_sec1_bytes(), vk.to_sec1_bytes());
    }
}
