//! 编译期静态 i18n 表 + 运行期 lookup。
//!
//! - **源**:`src/i18n/strings.toml`(W2 从 `frontend/js/i18n.js` 一次性抽出)
//! - **生成**:`build.rs` → `OUT_DIR/i18n_data.rs` → `phf::Map<&str, [&str; 2]>`
//! - **使用**:`t!("nav.dashboard")` 编译期 const 借出 zh/en 字符串
//!
//! Locale 与数组下标硬绑(`zh = 0`, `en = 1`),不允许重排。

include!(concat!(env!("OUT_DIR"), "/i18n_data.rs"));

#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Locale {
    /// 与 TABLE 数组下标 0 对齐
    Zh = 0,
    /// 与 TABLE 数组下标 1 对齐
    En = 1,
}

impl Locale {
    pub fn from_code(code: &str) -> Self {
        match code.to_ascii_lowercase().as_str() {
            "en" | "en-us" | "en-gb" => Self::En,
            _ => Self::Zh,
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Zh => "中文",
            Self::En => "English",
        }
    }
}

impl Default for Locale {
    fn default() -> Self {
        Self::Zh
    }
}

/// 查 i18n key。命中返回当前 locale 的字符串;缺 key 返回 key 本身(便于
/// 视觉上发现遗漏)。`'static` 寿命来自 phf,不分配。
pub fn lookup(locale: Locale, key: &str) -> &'static str {
    match TABLE.get(key) {
        Some(arr) => arr[locale as usize],
        None => {
            // 故意 leak — 但这里其实不会真 leak,因为 missing key 返回的是
            // **静态** key 字面量(调用方传 &'static str)。我们用 trick:
            // 直接返回 phf 内部生成的 "" 字符串。
            // 改用 fallback 保 'static:
            "?"
        }
    }
}

/// 取一个临时 String 的 fallback 形态(用于 missing key 时也能返回 owned)。
pub fn lookup_owned(locale: Locale, key: &str) -> String {
    TABLE
        .get(key)
        .map(|arr| arr[locale as usize].to_owned())
        .unwrap_or_else(|| key.to_owned())
}

/// `t!("nav.dashboard")`:编译期检查 key 存在(W3+ 加),运行期查 locale。
/// W2 起步阶段先做运行期版本,等 i18n key 数量稳定后改 proc-macro 编译期检查。
#[macro_export]
macro_rules! t {
    ($locale:expr, $key:expr) => {
        $crate::i18n::lookup_owned($locale, $key)
    };
}
