//! 客户端验签 —— RSA-3072 PKCS#1-v1.5-SHA256 over file bytes.
//!
//! ## 算法
//! 必须跟 `xtask/src/release_bundle.rs::sign_file` (CI 端 sign-side) 完全对称:
//! - hash:   `Sha256::digest(bytes)`
//! - sign:   `private_key.sign(Pkcs1v15Sign::new::<Sha256>(), &hashed)`
//! - encode: `base64::engine::general_purpose::STANDARD.encode(&sig)`
//! - verify (本模块): `public_key.verify(Pkcs1v15Sign::new::<Sha256>(), &hashed, &sig)`
//!
//! 三方等价性证据: `xtask/src/release_bundle.rs:457-495` 已用相同 crate 跑过 sign
//! → verify round-trip + tampered-data reject 单测,本模块 verify-side 直接复用同 crate
//! 同版本即满足 cross-impl 等价。
//!
//! ## Trust model
//! 公钥 build-time `include_str!` 嵌入二进制 —— 客户端**只信官方公钥**,任何
//! 第三方签名(包括用户自签 self-host) verify 必然 fail。设计意图:
//! - 防 MITM `latest.json` (改 sha256+url 推任意 installer)
//! - 防恶意 update URL 注入(GitHub release 镜像 / 仓库 transfer 等场景)
//!
//! Self-host 用户需要 fork repo + 改 `release/Codex-App-Transfer-release-public.pem`
//! + 自签 + 自 build,不支持运行时切换公钥(避免引入 trust-on-first-use 攻击面)。

use std::fmt;

use base64::Engine as _;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Sign, RsaPublicKey};
use sha2::{Digest, Sha256};

/// 官方发布公钥 (RSA-3072 PKCS#8 PEM)。
///
/// build-time 编译期嵌入 — 修改公钥必须重 build 客户端,运行时不可替换(故意为之,
/// 避免 attacker 通过修改本地 `~/.codex-app-transfer/*.pem` 等路径绕开)。
///
/// 路径相对 `src-tauri/src/admin/signature.rs`: `../../../release/...`。
const RELEASE_PUBLIC_KEY_PEM: &str =
    include_str!("../../../release/Codex-App-Transfer-release-public.pem");

/// 验签错误。所有路径都被视为"signature invalid",升级流程必须硬 fail
/// (不能 fallback 到 sha256-only 校验)。
#[derive(Debug)]
pub enum VerifyError {
    /// 客户端公钥 PEM 解析失败 — 等同 build 出 bug,理论上不该发生。
    PublicKeyParse(String),
    /// `.sig` 文件 base64 解码失败(空白 / 非法字符 / 长度异常)。
    SignatureDecode(String),
    /// RSA verify 拒绝: 数据被篡改 / 签名错配公钥 / 算法不匹配。
    SignatureRejected(String),
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::PublicKeyParse(e) => write!(f, "embedded public key parse failed: {e}"),
            VerifyError::SignatureDecode(e) => write!(f, "signature decode failed: {e}"),
            VerifyError::SignatureRejected(e) => {
                write!(
                    f,
                    "signature verify failed (data tampered or wrong key): {e}"
                )
            }
        }
    }
}

impl std::error::Error for VerifyError {}

/// 校验 `data` 是否被官方公钥签名,签名以 base64 文本形式提供。
///
/// 算法: `RSASSA-PKCS1-v1_5(Sha256(data))` against `RELEASE_PUBLIC_KEY_PEM`。
///
/// `sig_b64_text` 接受 release 体系产出的 `.sig` 文件直接读出的字符串
/// (单行 base64,无换行 / 头注释)。允许首尾空白(`trim` 一次)。
pub fn verify_signed_bytes(data: &[u8], sig_b64_text: &str) -> Result<(), VerifyError> {
    let public_key = RsaPublicKey::from_public_key_pem(RELEASE_PUBLIC_KEY_PEM)
        .map_err(|e| VerifyError::PublicKeyParse(e.to_string()))?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig_b64_text.trim().as_bytes())
        .map_err(|e| VerifyError::SignatureDecode(e.to_string()))?;
    let hashed = Sha256::digest(data);
    public_key
        .verify(Pkcs1v15Sign::new::<Sha256>(), &hashed, &sig_bytes)
        .map_err(|e| VerifyError::SignatureRejected(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::{EncodePublicKey, LineEnding};
    use rsa::rand_core::OsRng;
    use rsa::traits::PublicKeyParts;
    use rsa::RsaPrivateKey;

    /// build-time 嵌入的官方 PEM 不该是空 / 不该 parse 失败。
    /// 同时 verify embedded pubkey 的 RSA size 是 3072 bits (跟 xtask 对齐)。
    #[test]
    fn embedded_public_key_parses_and_is_3072_bit() {
        let pubkey = RsaPublicKey::from_public_key_pem(RELEASE_PUBLIC_KEY_PEM)
            .expect("embedded release public key must parse");
        // RSA-3072 modulus = 384 bytes = 3072 bits
        assert_eq!(
            pubkey.size(),
            384,
            "embedded release public key must be RSA-3072"
        );
    }

    /// release/latest.json 现网真实 + 配套 .sig 必须 verify pass。
    /// 任何修改 release/latest.json 都需要 xtask 重签 (CI 自动) 否则本测试 break,
    /// 这是有意的回归 guard。
    #[test]
    fn real_release_latest_json_signature_verifies() {
        let json_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("release")
            .join("latest.json");
        let sig_path = json_path.with_file_name("latest.json.sig");
        if !json_path.exists() || !sig_path.exists() {
            eprintln!(
                "skipping: {} or {} missing — run xtask release-bundle first",
                json_path.display(),
                sig_path.display()
            );
            return;
        }
        let data = std::fs::read(&json_path).expect("read latest.json");
        let sig = std::fs::read_to_string(&sig_path).expect("read latest.json.sig");
        verify_signed_bytes(&data, &sig)
            .expect("real release latest.json must verify against embedded pubkey");
    }

    /// 篡改 1 byte 后 verify 必须 Err(SignatureRejected),不能 panic 不能 OK。
    /// 这是核心防 MITM 测试。
    #[test]
    fn tampered_data_rejected() {
        let json_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("release")
            .join("latest.json");
        let sig_path = json_path.with_file_name("latest.json.sig");
        if !json_path.exists() || !sig_path.exists() {
            eprintln!("skipping: release samples missing");
            return;
        }
        let mut data = std::fs::read(&json_path).expect("read latest.json");
        let sig = std::fs::read_to_string(&sig_path).expect("read latest.json.sig");
        if !data.is_empty() {
            data[0] ^= 0x01;
        }
        let err = verify_signed_bytes(&data, &sig).unwrap_err();
        assert!(
            matches!(err, VerifyError::SignatureRejected(_)),
            "expected SignatureRejected, got {err:?}"
        );
    }

    /// 用 dmg 的真签名样本验 (installer 路径同样 PKCS1-v15-SHA256 over raw bytes)。
    #[test]
    fn real_release_installer_signature_verifies() {
        let dmg_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("release")
            .join("Codex-App-Transfer-v1.0.3-macOS-arm64.dmg");
        let sig_path = dmg_path.with_file_name("Codex-App-Transfer-v1.0.3-macOS-arm64.dmg.sig");
        if !dmg_path.exists() || !sig_path.exists() {
            eprintln!("skipping: installer sample {} missing", dmg_path.display());
            return;
        }
        let data = std::fs::read(&dmg_path).expect("read dmg");
        let sig = std::fs::read_to_string(&sig_path).expect("read dmg.sig");
        verify_signed_bytes(&data, &sig).expect("real installer must verify");
    }

    /// 异签名公钥的 .sig 必须 Err — 生成临时 keypair 签名后用本模块嵌入公钥 verify,
    /// 拒绝。证明 verify 真的 anchor 到嵌入公钥,不是 accept-any。
    #[test]
    fn signature_from_unknown_key_rejected() {
        let mut rng = OsRng;
        let priv_unknown = RsaPrivateKey::new(&mut rng, 3072).unwrap();
        // 跨 PEM round-trip 模拟"别人用自己私钥签"
        let _pem = priv_unknown
            .to_public_key()
            .to_public_key_pem(LineEnding::LF)
            .unwrap();
        let data = b"any data";
        let hashed = Sha256::digest(data);
        let sig = priv_unknown
            .sign(Pkcs1v15Sign::new::<Sha256>(), &hashed)
            .unwrap();
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);
        let err = verify_signed_bytes(data, &sig_b64).unwrap_err();
        assert!(
            matches!(err, VerifyError::SignatureRejected(_)),
            "foreign-key signature must be rejected"
        );
    }

    /// 损坏的 base64 输入应返回 SignatureDecode 而非 panic。
    #[test]
    fn malformed_base64_returns_decode_error() {
        let err = verify_signed_bytes(b"abc", "not_valid_base64!!!").unwrap_err();
        assert!(
            matches!(err, VerifyError::SignatureDecode(_)),
            "malformed sig must yield SignatureDecode, got {err:?}"
        );
    }

    /// 首尾空白 / 换行不算损坏 — 写 .sig 工具有时会加换行。
    #[test]
    fn trims_whitespace_around_signature() {
        let json_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("release")
            .join("latest.json");
        let sig_path = json_path.with_file_name("latest.json.sig");
        if !json_path.exists() || !sig_path.exists() {
            eprintln!("skipping");
            return;
        }
        let data = std::fs::read(&json_path).unwrap();
        let sig = std::fs::read_to_string(&sig_path).unwrap();
        let padded = format!("\n  {}  \n", sig.trim());
        verify_signed_bytes(&data, &padded).expect("trim whitespace");
    }
}
