//! 共享 PKCE(RFC 7636,method **S256**)原语,供各 provider 的 OAuth 登录复用。
//!
//! `code_verifier = base64url(48 随机字节)`,`code_challenge = base64url(sha256(verifier))`。
//! grok_build / qoder / trae 三处此前各自复制过同一段逻辑([[MOC-302]] 的重复消除),
//! 统一到此。RNG 失败仅在 OS 熵源不可用(极罕见),返回错误串,由各 provider 包成自己的错误类型。

use base64::Engine;
use sha2::{Digest, Sha256};

/// PKCE 一对:`verifier` 客户端本地留(换 token 时回传),`challenge` 进 authorize URL(`code_challenge_method=S256`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    /// `code_verifier`(base64url 无 padding 的 48 随机字节 → 64 字符,落在 RFC 7636 的 43–128 区间)。
    pub verifier: String,
    /// `code_challenge` = `base64url(sha256(verifier))`。
    pub challenge: String,
}

/// 生成一对 PKCE(S256)。`Err` = OS RNG 不可用(极罕见)。
pub fn generate() -> Result<PkcePair, String> {
    let mut vbytes = [0u8; 48];
    getrandom::getrandom(&mut vbytes).map_err(|e| e.to_string())?;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(vbytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    Ok(PkcePair {
        verifier,
        challenge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_s256_of_verifier() {
        let p = generate().unwrap();
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expect);
        // verifier 长度落在 RFC 7636 §4.1 的 43–128 区间(48 字节 base64url = 64 字符)。
        assert!(
            (43..=128).contains(&p.verifier.len()),
            "len={}",
            p.verifier.len()
        );
    }

    #[test]
    fn two_pairs_differ() {
        assert_ne!(generate().unwrap().verifier, generate().unwrap().verifier);
    }
}
