//! gemini-cli OAuth 流程的硬编码常量。
//!
//! 这些值**故意公开** —— Google 设计 installed-app OAuth 凭证为客户端嵌入,见
//! [Installed app flow](https://developers.google.com/identity/protocols/oauth2/native-app)。
//! 跟 gemini-cli 官方 (`packages/core/src/code_assist/oauth2.ts:43-51`) 保持一致。

/// gemini-cli 客户端 ID(installed-app 类型)。
pub const CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";

/// gemini-cli 客户端 secret(installed-app 设计为公开)。
pub const CLIENT_SECRET: &str = "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";

/// Google OAuth 2.0 授权端点(用户浏览器跳转目标)。
pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Google OAuth 2.0 token 端点(code → access_token + refresh_token)。
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// Cloud Code Assist 内部 API base URL —— OAuth 路径专用,**跟 API key 路径**
/// (`generativelanguage.googleapis.com`)不同。
pub const CLOUD_CODE_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";

/// OAuth scope(空格分隔)。三个 scope 缺一不可:
/// - `cloud-platform`:Cloud Code Assist API 调用权限
/// - `userinfo.email`:展示用户当前登录邮箱
/// - `userinfo.profile`:展示用户名(诊断用)
pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

/// 出站 User-Agent —— impersonate gemini-cli。Google 上游会按这个字段做客户端
/// 识别,跟 `X-Goog-Api-Client` 一起出现在所有 cloudcode-pa 请求里。值跟
/// CLIProxyAPI `header_utils.go::DetectUserAgent` 一致(format
/// `GeminiCLI/0.34.0 (<platform>; <arch>; terminal)`,platform/arch 跟
/// `process.platform` / `process.arch` 一致 — `darwin`/`linux`/`win32`,
/// `arm64`/`x64`/`ia32`)。
///
/// **不能 hardcode**:Linux 用户上传 `darwin; arm64` UA 会让 Google 上游 telemetry
/// 把 Linux 流量当 macOS 统计 + 部分 quota / abuse 检测可能 trip。
pub fn detect_user_agent() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        other => other,
    };
    format!("GeminiCLI/0.34.0 ({platform}; {arch}; terminal)")
}

/// 兼容老调用方 —— 跟 `detect_user_agent()` 同一身份格式;preset extraHeaders
/// 不能放运行时值,需要静态字符串时用此 const(macOS Apple Silicon 字面)。
/// **新代码请用 `detect_user_agent()`**。
#[deprecated(note = "use detect_user_agent() — preset extraHeaders 走 forward.rs runtime 注入")]
pub const USER_AGENT: &str = "GeminiCLI/0.34.0 (darwin; arm64; terminal)";

/// 出站 X-Goog-Api-Client header —— Google 内部 telemetry,缺这个字段
/// cloudcode-pa 端点会按"非官方客户端"分支响应。值字面对齐 CLIProxyAPI。
pub const X_GOOG_API_CLIENT: &str = "google-genai-sdk/1.41.0 gl-node/v22.19.0";

/// loopback redirect URI 路径 —— 每次启动随机 port,完整 URI 在 flow 模块
/// 动态构造:`http://127.0.0.1:<port>/oauth2callback`。
pub const REDIRECT_PATH: &str = "/oauth2callback";

/// Token expired 前多少秒自动触发 refresh —— 60s buffer 防 race(请求到上游时
/// token 刚好过期)。
pub const REFRESH_BUFFER_SECS: i64 = 60;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_matches_gemini_cli_upstream() {
        // Pin 防回归 — gemini-cli 历史上 rotate 过一次 client_id。如果 Google
        // 再 rotate 让我们 401,这条断言会被改,同时记录 rotate 时间。
        assert!(CLIENT_ID.starts_with("681255809395-"));
        assert!(CLIENT_ID.ends_with(".apps.googleusercontent.com"));
    }

    #[test]
    fn scopes_include_cloud_platform_and_userinfo() {
        let joined = SCOPES.join(" ");
        assert!(joined.contains("cloud-platform"));
        assert!(joined.contains("userinfo.email"));
        assert!(joined.contains("userinfo.profile"));
    }

    #[test]
    fn cloud_code_base_url_is_internal_endpoint() {
        // 不能误用 generativelanguage.googleapis.com — 那是 API-key 路径。
        assert_eq!(CLOUD_CODE_BASE_URL, "https://cloudcode-pa.googleapis.com");
    }
}
