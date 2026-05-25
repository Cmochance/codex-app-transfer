//! 用户偏好语言全局状态 — 控制注入到模型 system messages 的 prompt 语言。
//!
//! **设计动机**(#262):本项目向 chat-path / autocompact 流程注入大段英文
//! system prompt(apply_patch 规则 + compact 总结指引),中文用户跑非 OpenAI
//! provider 时模型输出中英混杂。改成跟随 `Settings.language` 选 prompt 语言
//! 后,中文用户看到中文 prompt → 模型保持单一语言思考输出。
//!
//! **为什么用全局 atomic 而不是 Adapter trait 参数**:
//! - `Adapter::prepare_request` 签名 `(client_path, body, provider)` 已固定,
//!   加 language 参数会 ripple 改 ~5 个 trait method + 所有 impl + 所有 caller
//! - language 是 **per-user 全局偏好**,不是 per-provider / per-request — 跟
//!   provider 配置正交,塞进 `Provider` struct 语义错位
//! - settings 改动后 hot reload(下次注入即生效),OnceLock + RwLock 模式即可
//!
//! **默认 fallback**:`"en"` — 保持升级用户的生产行为不变(只在用户显式选
//! `"zh"` 后才切中文)。其它 language 字符串(`ja` / `ko` / 等)统统当 `"en"`
//! 处理(目前只有 zh/en 两版翻译)。

use std::sync::RwLock;

/// 当前 user 偏好语言。初始 `None` 时各 caller fallback 到 `"en"`。
/// `RwLock<Option<String>>` 选择理由:
/// - settings 读写少(user 切语言才更新),并发 read 多(每次注入 prompt 都读)
/// - `Option` 区分 "未初始化"(走 fallback 默认行为)vs "显式设为某语言"
/// - `String` 不锁住具体枚举,前向兼容未来语种扩展
static USER_LANGUAGE: RwLock<Option<String>> = RwLock::new(None);

/// 设置当前 user 语言偏好。backend settings 加载 / 更新时调。
///
/// `lang` 接受任意字符串,内部 normalize 成小写;实际只 `zh` / `zh-*`
/// 走中文路径,其它当英文。caller 不需要校验。
pub fn set_user_language(lang: impl Into<String>) {
    let lang = lang.into();
    if let Ok(mut guard) = USER_LANGUAGE.write() {
        *guard = Some(lang);
    }
}

/// 拿当前 user 语言偏好。未设置 / 不识别 → `Language::English`。
pub fn current_language() -> Language {
    let raw = USER_LANGUAGE.read().ok().and_then(|g| g.clone());
    raw.as_deref()
        .map(Language::from_code)
        .unwrap_or(Language::English)
}

/// Adapters 支持的注入 prompt 语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    English,
    Chinese,
}

impl Language {
    /// 从语言码字符串映射(小写不敏感,接受 `zh` / `zh-CN` / `zh-Hans` /
    /// `zh-TW` 等所有 zh-* 走中文);其它一律 English。
    pub fn from_code(code: &str) -> Self {
        let lower = code.trim().to_ascii_lowercase();
        if lower == "zh" || lower.starts_with("zh-") || lower.starts_with("zh_") {
            Language::Chinese
        } else {
            Language::English
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 测试串行化:全局 atomic state 在并发测试间互相干扰,加 mutex 让 set/get 配对独立。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_lang<F: FnOnce()>(lang: Option<&str>, f: F) {
        let _guard = TEST_LOCK.lock().unwrap();
        let prev = USER_LANGUAGE.read().ok().and_then(|g| g.clone());
        match lang {
            Some(l) => set_user_language(l),
            None => {
                if let Ok(mut g) = USER_LANGUAGE.write() {
                    *g = None;
                }
            }
        }
        f();
        if let Ok(mut g) = USER_LANGUAGE.write() {
            *g = prev;
        }
    }

    #[test]
    fn language_from_code_recognizes_zh_variants() {
        assert_eq!(Language::from_code("zh"), Language::Chinese);
        assert_eq!(Language::from_code("zh-CN"), Language::Chinese);
        assert_eq!(Language::from_code("zh-Hans"), Language::Chinese);
        assert_eq!(Language::from_code("zh-TW"), Language::Chinese);
        assert_eq!(Language::from_code("zh_HK"), Language::Chinese);
        assert_eq!(Language::from_code("ZH-CN"), Language::Chinese);
        assert_eq!(Language::from_code(" zh "), Language::Chinese);
    }

    #[test]
    fn language_from_code_defaults_other_to_english() {
        assert_eq!(Language::from_code(""), Language::English);
        assert_eq!(Language::from_code("en"), Language::English);
        assert_eq!(Language::from_code("en-US"), Language::English);
        assert_eq!(Language::from_code("ja"), Language::English);
        assert_eq!(Language::from_code("ko"), Language::English);
        assert_eq!(Language::from_code("fr"), Language::English);
        // 容易跟 zh 混的近似 code 不该误判
        assert_eq!(Language::from_code("zha"), Language::English);
    }

    #[test]
    fn current_language_defaults_to_english_when_unset() {
        with_lang(None, || {
            assert_eq!(current_language(), Language::English);
        });
    }

    #[test]
    fn set_user_language_persists_across_reads() {
        with_lang(Some("zh-CN"), || {
            assert_eq!(current_language(), Language::Chinese);
        });
        with_lang(Some("en"), || {
            assert_eq!(current_language(), Language::English);
        });
    }
}
