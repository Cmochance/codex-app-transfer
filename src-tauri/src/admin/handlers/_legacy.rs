//! `/api/*` 路由 handlers —— 1:1 翻译自 `backend/main.py`.
//!
//! 数据形态(请求/响应 JSON shape)严格对齐 v1.4,frontend/js/api.js 不需要
//! 任何修改即可工作。

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use axum::body::Bytes;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
#[cfg(test)]
use base64::{engine::general_purpose::STANDARD, Engine};
use codex_app_transfer_codex_integration::{
    apply_provider, catalog_models_for_provider, has_snapshot, read_auth, restore_codex_state,
    ApplyConfig, CodexPaths,
};
use codex_app_transfer_registry::{
    builtin_presets, normalize_model_mappings, strip_internal_model_suffix, RawConfig, MODEL_ORDER,
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE},
    StatusCode as ReqwestStatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(test)]
use sha2::{Digest, Sha256};

use crate::proxy_runner::ProxyManager;

use super::super::registry_io::{load as load_registry, public_provider, save as save_registry};
use super::super::state::AdminState;
#[cfg(test)]
use super::common::random_hex;
#[cfg(test)]
use super::common::version;
use super::common::{active_provider_name, err, read_setting_bool};
#[cfg(test)]
use super::feedback::{
    feedback_attachments, feedback_proxy_tail_content, submit_feedback_with_body, FeedbackThrottle,
};
use super::proxy::{ensure_gateway_key, read_gateway_key, read_proxy_port, start_proxy_if_needed};
#[cfg(test)]
use super::settings::{create_config_backup, import_config, list_config_backups};
#[cfg(test)]
use super::update::{
    check_update_impl, current_update_platform_for, download_update_impl,
    install_after_quit_command_parts, install_command_parts, is_newer_version,
    pick_macos_installer, pick_platform_installer, pick_windows_installer,
};

pub(super) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
// moved to handlers::feedback::FEEDBACK_WORKER_URL
const ONE_M_CONTEXT_WINDOW: u64 = 1_000_000;

// moved to handlers::feedback::FeedbackThrottle / FeedbackThrottleState
// moved to handlers::feedback::FEEDBACK_THROTTLE
// moved to handlers::feedback::feedback_throttle
// moved to handlers::feedback::feedback_worker_url

// ── 工具 ─────────────────────────────────────────────────────────────

// moved to handlers::common::err
// moved to handlers::common::open_directory

// ── Codex App 重启 ────────────────────────────────────────────────────
//
// 借鉴 codex-account-switch (`src-tauri/{mac,win}/runtime/process.rs`):旧版用
// `osascript ... quit; sleep 0.5; open -a Codex` 的 sh 一行式管道,gentle
// quit 在 Codex 卡住 / 多窗口未保存时会被忽略,sleep 0.5 也太短;表面上
// spawn 成功代码视为 OK,实则 app 没动.改成三步:
// 1. pgrep / tasklist 探活
// 2. SIGTERM / taskkill 普通退出 + 最长 4s 轮询
// 3. 仍存活 → SIGKILL / taskkill /F + 最长 2s 轮询
// 4. 解析 .app 路径(macOS:/Applications + ~/Applications)再 open;
//    Windows 直接 explorer.exe shell:AppsFolder\<APP_ID>.

const MACOS_APP_NAME: &str = "Codex";
const WINDOWS_PROCESS_NAME: &str = "Codex.exe";
/// OpenAI 官方 Windows Store 包 ID,与 codex-account-switch 保持一致;
/// 用户若装的是非 Store 版本,resolve 失败时 explorer.exe 会报错,前端会
/// 看到 INTERNAL_SERVER_ERROR,比静默假成功好。
const WINDOWS_STORE_APP_ID: &str = "OpenAI.Codex_2p2nqsd0c76g0!App";
const LINUX_BIN_NAME: &str = "codex";

const QUIT_TERM_POLL_ITERS: u32 = 20; // 20 × 200ms = 4s
const QUIT_KILL_POLL_ITERS: u32 = 10; // 10 × 200ms = 2s
const QUIT_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// 退出确认后,等 launchd reap 完旧进程的 grace 窗口。低于 ~250ms 时
/// `open -a` 仍可能误命中"已在运行"缓存。
const POST_QUIT_LAUNCHD_GRACE: Duration = Duration::from_millis(400);

/// 平台检测命令(可纯函数测试).返回 (program, args).第一个元素总是命令名。
fn running_check_command(platform: &str) -> Vec<String> {
    match platform {
        "macos" => vec!["pgrep".into(), "-x".into(), MACOS_APP_NAME.into()],
        "windows" => vec![
            "tasklist".into(),
            "/FI".into(),
            format!("IMAGENAME eq {WINDOWS_PROCESS_NAME}"),
            "/FO".into(),
            "CSV".into(),
            "/NH".into(),
        ],
        _ => vec!["pgrep".into(), "-x".into(), LINUX_BIN_NAME.into()],
    }
}

/// 退出命令(`force=false` 普通退出, `force=true` 强杀).
fn quit_command(platform: &str, force: bool) -> Vec<String> {
    match (platform, force) {
        ("macos", false) => vec![
            "pkill".into(),
            "-TERM".into(),
            "-x".into(),
            MACOS_APP_NAME.into(),
        ],
        ("macos", true) => vec![
            "pkill".into(),
            "-KILL".into(),
            "-x".into(),
            MACOS_APP_NAME.into(),
        ],
        ("windows", false) => vec!["taskkill".into(), "/IM".into(), WINDOWS_PROCESS_NAME.into()],
        ("windows", true) => vec![
            "taskkill".into(),
            "/F".into(),
            "/IM".into(),
            WINDOWS_PROCESS_NAME.into(),
        ],
        (_, false) => vec![
            "pkill".into(),
            "-TERM".into(),
            "-x".into(),
            LINUX_BIN_NAME.into(),
        ],
        (_, true) => vec![
            "pkill".into(),
            "-KILL".into(),
            "-x".into(),
            LINUX_BIN_NAME.into(),
        ],
    }
}

/// 启动命令.macOS 优先用解析后的 .app 路径,fallback 到 `open -a Codex`
/// 让 LaunchServices 自己找。
fn open_command(platform: &str, resolved_macos_app: Option<&str>) -> Vec<String> {
    match platform {
        // `-n`:即使 LaunchServices 缓存还以为 Codex 在运行,也强制启动一个新
        // 实例。我们刚 SIGTERM 杀过主进程,launchd 偶尔会在 reap 完成前误把
        // `open -a` 解读为 activate 已有实例 → 啥也不发生。`-n` 绕过这条。
        "macos" => vec![
            "open".into(),
            "-n".into(),
            "-a".into(),
            resolved_macos_app.unwrap_or(MACOS_APP_NAME).into(),
        ],
        "windows" => vec![
            "explorer.exe".into(),
            format!("shell:AppsFolder\\{WINDOWS_STORE_APP_ID}"),
        ],
        _ => vec![
            "sh".into(),
            "-c".into(),
            format!("{LINUX_BIN_NAME} >/dev/null 2>&1 &"),
        ],
    }
}

fn resolve_macos_app_path() -> Option<String> {
    let mut candidates = vec![PathBuf::from("/Applications/Codex.app")];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join("Applications").join("Codex.app"));
    }
    candidates
        .into_iter()
        .find(|p| p.is_dir())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Windows 上给 Command 加 `CREATE_NO_WINDOW`(0x08000000)flag,避免每次
/// 调 `tasklist` / `taskkill` 都 flash 一个 console 黑框。其他平台 no-op。
/// 借鉴 codex-account-switch `src-tauri/win/runtime/process.rs::hide_console_window`。
#[cfg(target_os = "windows")]
fn hide_console_window(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(target_os = "windows"))]
fn hide_console_window(command: &mut Command) -> &mut Command {
    command
}

fn is_codex_app_running(platform: &str) -> bool {
    let cmd = running_check_command(platform);
    let Some((program, args)) = cmd.split_first() else {
        return false;
    };
    if platform == "windows" {
        // tasklist 即使没匹配也 exit 0,要看 stdout 里有没有 process 名
        let mut command = Command::new(program);
        command.args(args);
        match hide_console_window(&mut command).output() {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .to_ascii_lowercase()
                .contains(&WINDOWS_PROCESS_NAME.to_ascii_lowercase()),
            Err(_) => false,
        }
    } else {
        // pgrep:有进程 exit 0,没进程 exit 1
        Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

fn run_quit_command(platform: &str, force: bool) {
    let cmd = quit_command(platform, force);
    let Some((program, args)) = cmd.split_first() else {
        return;
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = hide_console_window(&mut command).status();
}

fn quit_codex_app_with_retries(platform: &str) -> Result<(), String> {
    if !is_codex_app_running(platform) {
        return Ok(());
    }
    run_quit_command(platform, false);
    for _ in 0..QUIT_TERM_POLL_ITERS {
        if !is_codex_app_running(platform) {
            return Ok(());
        }
        std::thread::sleep(QUIT_POLL_INTERVAL);
    }
    run_quit_command(platform, true);
    for _ in 0..QUIT_KILL_POLL_ITERS {
        if !is_codex_app_running(platform) {
            return Ok(());
        }
        std::thread::sleep(QUIT_POLL_INTERVAL);
    }
    Err("Codex 未能正常退出,请手动关闭后重试".to_owned())
}

fn open_codex_app(platform: &str) -> Result<(), String> {
    let resolved = if platform == "macos" {
        resolve_macos_app_path()
    } else {
        None
    };
    let cmd = open_command(platform, resolved.as_deref());
    let Some((program, args)) = cmd.split_first() else {
        return Err("打开命令为空".to_owned());
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_console_window(&mut command)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法启动 Codex App: {e}"))
}

fn launch_codex_app_restart(platform: &str) -> Result<(), String> {
    let was_running = is_codex_app_running(platform);
    quit_codex_app_with_retries(platform)?;
    // 退出确认后给 launchd 一段 grace 让它 reap 完旧进程,LaunchServices 才会
    // 把"Codex 在运行"的缓存清掉。否则紧跟的 `open -a` 会被当成 activate
    // 一个不存在的实例,啥也不发生(2026-05-06 现场实测)。
    // 跳过条件:本来就没在运行,根本不需要等。
    if was_running {
        std::thread::sleep(POST_QUIT_LAUNCHD_GRACE);
    }
    open_codex_app(platform)
}

// moved to handlers::feedback::multipart_text_part

// moved to handlers::common::active_provider_name

// moved to handlers::feedback::feedback_proxy_tail_content
// moved to handlers::feedback::feedback_proxy_tail_part
// moved to handlers::feedback::FeedbackAttachment

// moved to handlers::common::current_epoch_secs

// moved to handlers::feedback::feedback_attachments

// moved to handlers::update::current_update_platform / current_update_platform_for / version_parts /
// is_newer_version / validate_update_url / safe_asset_name / asset_filename_from_url / file_sha256 /
// pick_platform_data / allowed_install_extensions / pick_windows_installer / pick_macos_installer /
// pick_platform_installer / install_command_parts / install_after_quit_command_parts /
// launch_update_installer / configured_update_url / fetch_latest_json / check_update_impl /
// download_asset_impl / download_update_impl

static ID_COUNTER: AtomicU32 = AtomicU32::new(0);
fn fresh_provider_id(existing: &[String]) -> String {
    loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u32)
            .unwrap_or(0);
        let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("{:08x}", nanos.wrapping_add(counter));
        if !existing.iter().any(|id| id == &candidate) {
            return candidate;
        }
    }
}

fn provider_supports_1m(provider: &Value) -> bool {
    let default_raw = provider
        .get("models")
        .and_then(|m| m.get("default"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if codex_app_transfer_registry::has_internal_one_m_suffix(default_raw) {
        return true;
    }
    let default = strip_internal_model_suffix(default_raw).to_lowercase();
    if default.starts_with("deepseek-v4-") || default.starts_with("qwen3.6-") {
        return true;
    }
    if let Some(b) = provider
        .get("modelCapabilities")
        .and_then(|c| c.get(&default))
        .and_then(|v| v.get("supports1m"))
        .and_then(|v| v.as_bool())
    {
        return b;
    }
    false
}

fn provider_default_model(provider: &Value) -> String {
    let raw = provider
        .get("models")
        .and_then(|m| m.get("default"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    strip_internal_model_suffix(raw)
}

fn provider_model_mappings(provider: &Value) -> Value {
    provider.get("models").cloned().unwrap_or_else(|| json!({}))
}

fn provider_model_capabilities(provider: &Value) -> Value {
    provider
        .get("modelCapabilities")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn provider_display_name(provider: &Value) -> String {
    provider
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Provider")
        .to_owned()
}

fn normalize_provider_api_format(api_format: Option<&str>) -> &'static str {
    match api_format
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "openai" | "openai_chat" | "chat_completions" => "openai_chat",
        _ => "responses",
    }
}

fn build_provider_test_url(base_url: &str, api_format: &str) -> String {
    let clean = base_url.trim().trim_end_matches('/');
    let lower = clean.to_ascii_lowercase();
    if api_format == "openai_chat" {
        if lower.ends_with("/chat/completions") {
            return clean.to_owned();
        }
        return format!("{clean}/chat/completions");
    }
    if lower.ends_with("/v1/responses") {
        return clean.to_owned();
    }
    if lower.ends_with("/v1") {
        return format!("{clean}/responses");
    }
    format!("{clean}/v1/responses")
}

fn provider_api_key(provider: &Value) -> String {
    provider
        .get("apiKey")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
}

fn provider_test_model(provider: &Value) -> String {
    let mappings = normalize_model_mappings(provider.get("models"));
    let default = mappings.get("default").map(|s| s.trim()).unwrap_or("");
    if !default.is_empty() {
        return strip_internal_model_suffix(default);
    }
    for slot in MODEL_ORDER
        .iter()
        .copied()
        .filter(|slot| *slot != "default")
    {
        let model = mappings.get(slot).map(|s| s.trim()).unwrap_or("");
        if !model.is_empty() {
            return strip_internal_model_suffix(model);
        }
    }
    "claude-sonnet-4-6".to_owned()
}

fn provider_test_body(provider: &Value, api_format: &str) -> Value {
    let model = provider_test_model(provider);
    if api_format == "openai_chat" {
        return json!({
            "model": model,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 8,
            "stream": false,
        });
    }
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 8,
    })
}

fn is_kimi_provider(provider: &Value) -> bool {
    let probe = format!(
        "{} {}",
        provider.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    )
    .to_ascii_lowercase();
    probe.contains("kimi") || probe.contains("moonshot")
}

fn provider_test_headers(provider: &Value, include_content_type: bool) -> HeaderMap {
    let api_key = provider_api_key(provider);
    let mut headers = HeaderMap::new();
    if include_content_type {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

    if !api_key.is_empty() {
        let auth_scheme = provider
            .get("authScheme")
            .and_then(|v| v.as_str())
            .unwrap_or("bearer")
            .trim()
            .to_ascii_lowercase();
        match auth_scheme.as_str() {
            "x-api-key" | "x_api_key" | "xapikey" | "apikey" => {
                if let Ok(value) = HeaderValue::from_str(&api_key) {
                    headers.insert(HeaderName::from_static("x-api-key"), value);
                }
            }
            "none" | "no" => {}
            _ => {
                if let Ok(value) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
                    headers.insert(reqwest::header::AUTHORIZATION, value);
                }
            }
        }
    }

    if let Some(extra) = provider.get("extraHeaders").and_then(|v| v.as_object()) {
        for (key, value) in extra {
            let Some(raw_value) = value.as_str() else {
                continue;
            };
            let header_value = raw_value.replace("{apiKey}", &api_key);
            let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(key.as_bytes()),
                HeaderValue::from_str(&header_value),
            ) else {
                continue;
            };
            headers.insert(name, value);
        }
    }

    let provider_id = provider.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let base_url = provider
        .get("baseUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if provider_id == "kimi-code" || base_url.contains("api.kimi.com/coding") {
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("KimiCLI/1.40.0"),
        );
    }

    headers
}

fn provider_test_error_label(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "Timeout"
    } else if error.is_connect() {
        "ConnectError"
    } else {
        "RequestError"
    }
}

fn clean_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_owned()
}

fn replace_path_suffix(url: &str, suffixes: &[&str], replacement: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return url.to_owned();
    };
    let mut path = parsed.path().trim_end_matches('/').to_owned();
    let lower = path.to_ascii_lowercase();
    for suffix in suffixes {
        if lower.ends_with(suffix) {
            let keep = path.len().saturating_sub(suffix.len());
            path.truncate(keep);
            break;
        }
    }
    let next = format!(
        "{}/{}",
        path.trim_end_matches('/'),
        replacement.trim_start_matches('/')
    );
    parsed.set_path(&next);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string().trim_end_matches('/').to_owned()
}

fn model_endpoint_candidates(provider: &Value) -> Vec<String> {
    let base_url = clean_base_url(
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    if base_url.is_empty() {
        return Vec::new();
    }

    let api_format =
        normalize_provider_api_format(provider.get("apiFormat").and_then(|v| v.as_str()));
    let upstream = build_provider_test_url(&base_url, api_format);
    let mut candidates = Vec::new();

    if api_format == "openai_chat" {
        candidates.push(replace_path_suffix(
            &upstream,
            &["/chat/completions", "/completions"],
            "/models",
        ));
        candidates.push(format!("{base_url}/models"));
    } else {
        candidates.push(replace_path_suffix(
            &upstream,
            &["/v1/responses", "/responses"],
            "/v1/models",
        ));
        if base_url.to_ascii_lowercase().ends_with("/v1") {
            candidates.push(format!("{base_url}/models"));
        }
        candidates.push(format!("{base_url}/models"));
        if let Ok(parsed) = reqwest::Url::parse(&base_url) {
            let stripped_path = parsed.path().trim_end_matches('/');
            let lower = stripped_path.to_ascii_lowercase();
            if lower.ends_with("/anthropic") || lower.ends_with("/v1") {
                let root_path = if lower.ends_with("/anthropic") {
                    &stripped_path[..stripped_path.len().saturating_sub("/anthropic".len())]
                } else {
                    &stripped_path[..stripped_path.len().saturating_sub("/v1".len())]
                };
                let mut root = parsed.clone();
                root.set_path(root_path.trim_end_matches('/'));
                root.set_query(None);
                root.set_fragment(None);
                let root_url = root.to_string().trim_end_matches('/').to_owned();
                candidates.push(format!("{root_url}/models"));
                candidates.push(format!("{root_url}/v1/models"));
            }
        }
    }

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|item| !item.is_empty() && seen.insert(item.clone()))
        .collect()
}

fn model_id_from_item(item: &Value) -> Option<String> {
    if let Some(s) = item.as_str() {
        return Some(s.to_owned());
    }
    let obj = item.as_object()?;
    for key in ["id", "name", "model", "model_id"] {
        if let Some(value) = obj.get(key).and_then(|v| v.as_str()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

fn extract_model_ids(payload: &Value) -> Vec<String> {
    let mut candidates: Vec<Value> = Vec::new();
    if let Some(items) = payload.as_array() {
        candidates = items.clone();
    } else if let Some(obj) = payload.as_object() {
        for key in ["data", "models", "items", "result"] {
            if let Some(items) = obj.get(key).and_then(|v| v.as_array()) {
                candidates = items.clone();
                break;
            }
        }
        if candidates.is_empty() {
            if let Some(data) = obj.get("data").and_then(|v| v.as_object()) {
                for key in ["models", "items"] {
                    if let Some(items) = data.get(key).and_then(|v| v.as_array()) {
                        candidates = items.clone();
                        break;
                    }
                }
            }
        }
    }

    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for item in candidates {
        let Some(model_id) = model_id_from_item(&item) else {
            continue;
        };
        if seen.insert(model_id.clone()) {
            ids.push(model_id);
        }
    }
    ids
}

fn usable_model_ids(model_ids: &[String]) -> Vec<String> {
    const EXCLUDE: &[&str] = &[
        "embedding",
        "rerank",
        "moderation",
        "whisper",
        "tts",
        "image",
        "vision",
        "audio",
    ];
    let usable: Vec<String> = model_ids
        .iter()
        .filter(|model_id| {
            let lower = model_id.to_ascii_lowercase();
            !EXCLUDE.iter().any(|keyword| lower.contains(keyword))
        })
        .cloned()
        .collect();
    if usable.is_empty() {
        model_ids.to_vec()
    } else {
        usable
    }
}

fn pick_model(model_ids: &[String], keywords: &[&str], fallback_index: usize) -> String {
    for keyword in keywords {
        for model_id in model_ids {
            if model_id.to_ascii_lowercase().contains(keyword) {
                return model_id.clone();
            }
        }
    }
    if model_ids.is_empty() {
        String::new()
    } else {
        model_ids[std::cmp::min(fallback_index, model_ids.len() - 1)].clone()
    }
}

fn empty_model_mappings_value() -> Value {
    let mut out = serde_json::Map::new();
    for slot in MODEL_ORDER.iter().copied() {
        out.insert(slot.to_owned(), Value::String(String::new()));
    }
    Value::Object(out)
}

fn suggest_model_mappings(model_ids: &[String]) -> Value {
    let usable = usable_model_ids(model_ids);
    let mut result = empty_model_mappings_value();
    if usable.is_empty() {
        return result;
    }
    let chosen = pick_model(
        &usable,
        &["pro", "plus", "coder", "max", "reasoner", "v4"],
        0,
    );
    if let Some(obj) = result.as_object_mut() {
        obj.insert("default".to_owned(), Value::String(chosen));
    }
    result
}

async fn fetch_provider_models_impl(provider: &Value) -> Value {
    let endpoints = model_endpoint_candidates(provider);
    if endpoints.is_empty() {
        return json!({"success": false, "message": "API 地址无效", "models": [], "suggested": {}});
    }

    let headers = provider_test_headers(provider, false);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return json!({
                "success": false,
                "message": "无法自动获取模型列表",
                "models": [],
                "suggested": {},
                "errors": [format!("client: {}", provider_test_error_label(&error))],
            });
        }
    };

    let mut errors: Vec<String> = Vec::new();
    for endpoint in endpoints {
        let response = match client.get(&endpoint).headers(headers.clone()).send().await {
            Ok(response) => response,
            Err(error) => {
                errors.push(format!("{endpoint}: {}", provider_test_error_label(&error)));
                continue;
            }
        };
        if !response.status().is_success() {
            errors.push(format!("{endpoint}: HTTP {}", response.status().as_u16()));
            continue;
        }
        let payload = match response.json::<Value>().await {
            Ok(payload) => payload,
            Err(_) => {
                errors.push(format!("{endpoint}: 非 JSON 响应"));
                continue;
            }
        };
        let model_ids = extract_model_ids(&payload);
        if !model_ids.is_empty() {
            return json!({
                "success": true,
                "endpoint": endpoint,
                "models": model_ids,
                "suggested": suggest_model_mappings(&model_ids),
            });
        }
        errors.push(format!("{endpoint}: 未发现模型列表"));
    }

    let start = errors.len().saturating_sub(5);
    json!({
        "success": false,
        "message": "无法自动获取模型列表",
        "models": [],
        "suggested": {},
        "errors": errors[start..].to_vec(),
    })
}

fn provider_kind(provider: &Value) -> &'static str {
    let probe = format!(
        "{} {}",
        provider.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    )
    .to_ascii_lowercase();
    if probe.contains("deepseek") {
        "deepseek"
    } else if probe.contains("siliconflow") {
        "siliconflow"
    } else if probe.contains("openrouter") {
        "openrouter"
    } else if probe.contains("novita") {
        "novita"
    } else if probe.contains("stepfun") || probe.contains("step") {
        "stepfun"
    } else {
        "unknown"
    }
}

fn balance_endpoint(provider: &Value) -> Option<(&'static str, String)> {
    let kind = provider_kind(provider);
    let base = clean_base_url(
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    )
    .to_ascii_lowercase();
    match kind {
        "deepseek" => Some((kind, "https://api.deepseek.com/user/balance".to_owned())),
        "siliconflow" => {
            let host = if base.contains(".com") {
                "https://api.siliconflow.com"
            } else {
                "https://api.siliconflow.cn"
            };
            Some((kind, format!("{host}/v1/user/info")))
        }
        "openrouter" => Some((kind, "https://openrouter.ai/api/v1/credits".to_owned())),
        "novita" => Some((kind, "https://api.novita.ai/v3/user/balance".to_owned())),
        "stepfun" => Some((kind, "https://api.stepfun.com/v1/accounts".to_owned())),
        _ => None,
    }
}

fn float_or_none(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) if !s.is_empty() => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn money_item(
    label: impl Into<String>,
    remaining: Option<f64>,
    total: Option<f64>,
    used: Option<f64>,
    unit: impl Into<String>,
) -> Value {
    json!({
        "label": label.into(),
        "remaining": remaining,
        "total": total,
        "used": used,
        "unit": unit.into(),
    })
}

fn normalize_balance_payload(kind: &str, payload: &Value) -> Vec<Value> {
    if kind == "deepseek" {
        let mut items = Vec::new();
        for item in payload
            .get("balance_infos")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let currency = obj
                .get("currency")
                .and_then(|v| v.as_str())
                .unwrap_or("CNY")
                .to_owned();
            items.push(money_item(
                currency.clone(),
                float_or_none(obj.get("total_balance")),
                float_or_none(obj.get("granted_balance")),
                float_or_none(obj.get("topped_up_balance")),
                currency,
            ));
        }
        return items;
    }

    if kind == "openrouter" {
        let data = payload.get("data").unwrap_or(payload);
        let total = float_or_none(data.get("total_credits"));
        let used = float_or_none(data.get("total_usage"));
        let remaining = match (total, used) {
            (Some(total), Some(used)) => Some(total - used),
            _ => None,
        };
        return vec![money_item("credits", remaining, total, used, "USD")];
    }

    let data = payload.get("data").unwrap_or(payload);
    if let Some(obj) = data.as_object() {
        for remaining_key in [
            "balance",
            "remaining",
            "available_balance",
            "availableBalance",
            "credit",
        ] {
            if obj.contains_key(remaining_key) {
                let unit = obj
                    .get("currency")
                    .or_else(|| obj.get("unit"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                return vec![money_item(
                    "balance",
                    float_or_none(obj.get(remaining_key)),
                    float_or_none(
                        obj.get("total")
                            .or_else(|| obj.get("totalBalance"))
                            .or_else(|| obj.get("total_credits")),
                    ),
                    float_or_none(
                        obj.get("used")
                            .or_else(|| obj.get("usage"))
                            .or_else(|| obj.get("usedBalance")),
                    ),
                    unit,
                )];
            }
        }
    }
    Vec::new()
}

async fn query_provider_usage_impl(provider: &Value) -> Value {
    if provider_api_key(provider).is_empty() {
        return json!({"success": false, "message": "请先保存 API Key"});
    }
    let Some((kind, endpoint)) = balance_endpoint(provider) else {
        return json!({
            "success": true,
            "supported": false,
            "items": [],
            "message": "这个提供商暂未适配余额/用量接口",
        });
    };

    let headers = provider_test_headers(provider, false);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return json!({
                "success": true,
                "supported": true,
                "ok": false,
                "message": format!("查询失败：{}", provider_test_error_label(&error)),
                "items": [],
            });
        }
    };
    let response = match client.get(&endpoint).headers(headers).send().await {
        Ok(response) => response,
        Err(error) => {
            return json!({
                "success": true,
                "supported": true,
                "ok": false,
                "message": format!("查询失败：{}", provider_test_error_label(&error)),
                "items": [],
            });
        }
    };
    if !response.status().is_success() {
        return json!({
            "success": true,
            "supported": true,
            "ok": false,
            "statusCode": response.status().as_u16(),
            "message": format!("余额接口返回 HTTP {}", response.status().as_u16()),
            "items": [],
        });
    }
    let payload = match response.json::<Value>().await {
        Ok(payload) => payload,
        Err(_) => {
            return json!({
                "success": true,
                "supported": true,
                "ok": false,
                "message": "余额接口返回了非 JSON 响应",
                "items": [],
            });
        }
    };
    let items = normalize_balance_payload(kind, &payload);
    let ok = !items.is_empty();
    let message = if ok {
        "查询完成"
    } else {
        "余额接口响应中未识别到余额字段"
    };
    json!({
        "success": true,
        "supported": true,
        "ok": ok,
        "endpoint": endpoint,
        "items": items,
        "message": message,
    })
}

fn provider_compatibility_item(provider: &Value) -> Value {
    let api_format =
        normalize_provider_api_format(provider.get("apiFormat").and_then(|v| v.as_str()));
    let id = provider.get("id").cloned().unwrap_or(Value::Null);
    let name = provider.get("name").cloned().unwrap_or(Value::Null);
    if api_format == "responses" {
        return json!({
            "id": id,
            "name": name,
            "apiFormat": api_format,
            "level": "stable",
            "message": "Responses 兼容接口，适合 Codex App 主流程。",
            "checks": {
                "models": true,
                "text": true,
                "stream": true,
                "tools": true,
                "streamingTools": true,
            },
        });
    }
    if api_format == "openai_chat" {
        return json!({
            "id": id,
            "name": name,
            "apiFormat": api_format,
            "level": "experimental",
            "message": "OpenAI Chat 实验适配：文本和非流式工具调用可测试，流式工具调用暂不作为稳定能力。",
            "checks": {
                "models": true,
                "text": true,
                "stream": true,
                "tools": true,
                "streamingTools": false,
            },
        });
    }
    json!({
        "id": id,
        "name": name,
        "apiFormat": api_format,
        "level": "unsupported",
        "message": format!("{api_format} 暂未适配。"),
        "checks": {
            "models": false,
            "text": false,
            "stream": false,
            "tools": false,
            "streamingTools": false,
        },
    })
}

async fn test_provider_connection(provider: &Value) -> Value {
    let api_format = normalize_provider_api_format(
        provider
            .get("apiFormat")
            .and_then(|v| v.as_str())
            .or(Some("responses")),
    );
    let base_url = build_provider_test_url(
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        api_format,
    );
    let parsed = reqwest::Url::parse(&base_url);
    let valid_url = parsed
        .as_ref()
        .map(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        .unwrap_or(false);
    if !valid_url {
        return json!({
            "message": "API 地址无效",
            "success": false,
        });
    }

    let started = Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return json!({
                "success": true,
                "ok": false,
                "latencyMs": started.elapsed().as_millis(),
                "message": format!("连接失败：{}", provider_test_error_label(&error)),
            });
        }
    };

    let probe_headers = provider_test_headers(provider, false);
    let content_headers = provider_test_headers(provider, true);
    let mut response = match client
        .head(&base_url)
        .headers(probe_headers.clone())
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return json!({
                "success": true,
                "ok": false,
                "latencyMs": started.elapsed().as_millis(),
                "message": format!("连接失败：{}", provider_test_error_label(&error)),
            });
        }
    };

    if matches!(
        response.status(),
        ReqwestStatusCode::NOT_FOUND | ReqwestStatusCode::METHOD_NOT_ALLOWED
    ) {
        response = match client.get(&base_url).headers(probe_headers).send().await {
            Ok(response) => response,
            Err(error) => {
                return json!({
                    "success": true,
                    "ok": false,
                    "latencyMs": started.elapsed().as_millis(),
                    "message": format!("连接失败：{}", provider_test_error_label(&error)),
                });
            }
        };
    }

    if matches!(
        response.status(),
        ReqwestStatusCode::NOT_FOUND | ReqwestStatusCode::METHOD_NOT_ALLOWED
    ) && !provider_api_key(provider).is_empty()
    {
        response = match client
            .post(&base_url)
            .headers(content_headers)
            .json(&provider_test_body(provider, api_format))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return json!({
                    "success": true,
                    "ok": false,
                    "latencyMs": started.elapsed().as_millis(),
                    "message": format!("连接失败：{}", provider_test_error_label(&error)),
                });
            }
        };
    }

    let latency_ms = started.elapsed().as_millis();
    let status_code = response.status().as_u16();
    let mut reachable = status_code < 500;
    let message = if (200..300).contains(&status_code) {
        format!("连接正常，{latency_ms} ms")
    } else if matches!(status_code, 401 | 403) {
        reachable = false;
        if is_kimi_provider(provider) {
            format!(
                "Kimi 认证失败，HTTP {status_code}。Kimi Platform Key 请使用 https://api.moonshot.cn/v1；Kimi Code 会员 Key 请使用 https://api.kimi.com/coding，{latency_ms} ms"
            )
        } else {
            format!(
                "认证失败，HTTP {status_code}，请检查 API Key 和 API 地址是否匹配，{latency_ms} ms"
            )
        }
    } else if matches!(status_code, 404 | 405) {
        reachable = false;
        format!("接口不可用，HTTP {status_code}，请检查 API 地址是否填到了兼容 Codex 的接口，{latency_ms} ms")
    } else {
        format!("地址可达，HTTP {status_code}，{latency_ms} ms")
    };

    json!({
        "success": true,
        "ok": reachable,
        "latencyMs": latency_ms,
        "statusCode": status_code,
        "message": message,
    })
}

// moved to handlers::proxy::read_proxy_port
// moved to handlers::proxy::read_gateway_key
// moved to handlers::common::read_setting_bool
// moved to handlers::proxy::ensure_gateway_key

pub(super) struct DesktopConfigTarget {
    pub(super) base_url: String,
    pub(super) api_key: String,
    pub(super) supports_1m: bool,
    pub(super) provider_name: String,
    pub(super) default_model: String,
    pub(super) model_mappings: Value,
    pub(super) model_capabilities: Value,
    pub(super) requires_proxy: bool,
    pub(super) mode: &'static str,
    pub(super) proxy_port: u16,
}

fn desktop_config_target_for_provider(
    cfg: &mut RawConfig,
    provider: &Value,
    proxy_port_override: Option<u16>,
) -> DesktopConfigTarget {
    let proxy_port = proxy_port_override.unwrap_or_else(|| read_proxy_port(cfg));
    let api_format =
        normalize_provider_api_format(provider.get("apiFormat").and_then(|v| v.as_str()));
    let requires_proxy = api_format != "responses";
    let (base_url, api_key, mode) = if requires_proxy {
        (
            format!("http://127.0.0.1:{proxy_port}"),
            ensure_gateway_key(cfg),
            "local_proxy",
        )
    } else {
        (
            clean_base_url(
                provider
                    .get("baseUrl")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            ),
            provider_api_key(provider),
            "direct_provider",
        )
    };
    DesktopConfigTarget {
        base_url,
        api_key,
        supports_1m: provider_supports_1m(provider),
        provider_name: provider_display_name(provider),
        default_model: provider_default_model(provider),
        model_mappings: provider_model_mappings(provider),
        model_capabilities: provider_model_capabilities(provider),
        requires_proxy,
        mode,
        proxy_port,
    }
}

pub(super) fn desktop_target_for_active_provider(cfg: &RawConfig) -> Option<DesktopConfigTarget> {
    let provider = active_provider(cfg)?;
    let mut snapshot = cfg.clone();
    Some(desktop_config_target_for_provider(
        &mut snapshot,
        &provider,
        None,
    ))
}

fn desktop_expected_model_items(target: &DesktopConfigTarget) -> Vec<Value> {
    catalog_models_for_provider(
        &target.provider_name,
        &target.default_model,
        target.supports_1m,
        Some(&target.model_mappings),
        Some(&target.model_capabilities),
    )
    .into_iter()
    .map(|model| {
        let mut item = json!({
            "name": model.slug,
            "displayName": model.display_name,
        });
        if model.context_window >= ONE_M_CONTEXT_WINDOW {
            item["supports1m"] = Value::Bool(true);
        }
        item
    })
    .collect()
}

fn desktop_inference_models_json(target: Option<&DesktopConfigTarget>) -> String {
    let Some(target) = target else {
        return "[]".to_owned();
    };
    serde_json::to_string(&desktop_expected_model_items(target)).unwrap_or_else(|_| "[]".to_owned())
}

pub(super) fn read_codex_toml_root_string(paths: &CodexPaths, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(&paths.config_toml).ok()?;
    for line in content.lines() {
        let stripped = line.trim_start();
        if stripped.starts_with('[') {
            break;
        }
        if !stripped.starts_with(key) {
            continue;
        }
        let after = &stripped[key.len()..];
        let mut rest = after.trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        rest = rest[1..].trim();
        if let Some(idx) = rest.find('#') {
            rest = rest[..idx].trim_end();
        }
        let trimmed = rest.trim_matches(|c: char| c == '"' || c == '\'');
        return Some(trimmed.to_owned());
    }
    None
}

pub(super) fn codex_openai_api_key_present(paths: &CodexPaths) -> bool {
    read_auth(&paths.auth_json)
        .ok()
        .and_then(|auth| {
            auth.get("OPENAI_API_KEY")
                .and_then(|v| v.as_str())
                .map(|s| !s.trim().is_empty())
        })
        .unwrap_or(false)
}

fn one_million_catalog_ready(paths: &CodexPaths, target: &DesktopConfigTarget) -> bool {
    let one_million_names: Vec<String> = desktop_expected_model_items(target)
        .into_iter()
        .filter_map(|item| {
            if item.get("supports1m").and_then(|v| v.as_bool()) == Some(true) {
                item.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned())
            } else {
                None
            }
        })
        .collect();
    if one_million_names.is_empty() {
        return true;
    }

    let Some(catalog_path) = read_codex_toml_root_string(paths, "model_catalog_json") else {
        return false;
    };
    let catalog_path = PathBuf::from(catalog_path);
    let catalog = fs::read_to_string(catalog_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or_else(|| json!({}));
    let Some(models) = catalog.get("models").and_then(|v| v.as_array()) else {
        return false;
    };
    models.iter().any(|item| {
        let slug = item
            .get("slug")
            .or_else(|| item.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !one_million_names.iter().any(|name| name == slug) {
            return false;
        }
        let context_window = item
            .get("context_window")
            .or_else(|| item.get("max_context_window"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        context_window >= ONE_M_CONTEXT_WINDOW
    })
}

pub(super) fn desktop_health(
    paths: Option<&CodexPaths>,
    configured: bool,
    actual_base_url: Option<&str>,
    actual_api_key_present: bool,
    target: Option<&DesktopConfigTarget>,
) -> Value {
    let expected_base_url = target
        .map(|target| target.base_url.trim_end_matches('/').to_owned())
        .unwrap_or_default();
    let actual_base_url = actual_base_url
        .unwrap_or("")
        .trim()
        .trim_end_matches('/')
        .to_owned();
    let mut issues = Vec::new();

    if !configured {
        if !actual_base_url.is_empty() || actual_api_key_present {
            issues.push(json!({
                "code": "not_managed_by_cas",
                "message": "当前 Codex CLI 配置不是由本工具最新版本写入。",
            }));
        } else {
            issues.push(json!({
                "code": "codex_snapshot_missing",
                "message": "尚未由本工具应用 Codex CLI 配置，请重新一键生成配置。",
            }));
        }
    }

    if !actual_base_url.is_empty()
        && !expected_base_url.is_empty()
        && actual_base_url != expected_base_url
    {
        issues.push(json!({
            "code": "gateway_base_url_mismatch",
            "message": "Codex CLI 仍指向旧地址，请重新一键生成 Codex CLI 配置。",
        }));
    }

    let one_million_ready = match (paths, target) {
        (Some(paths), Some(target)) => one_million_catalog_ready(paths, target),
        _ => true,
    };
    if !one_million_ready {
        issues.push(json!({
            "code": "one_million_not_written",
            "message": "1M 上下文模型尚未写入 Codex CLI 配置，请重新一键生成配置并重启终端。",
        }));
    }

    json!({
        "needsApply": !configured || !issues.is_empty(),
        "oneMillionReady": one_million_ready,
        "expectedBaseUrl": expected_base_url,
        "actualBaseUrl": actual_base_url,
        "mode": target.map(|target| target.mode),
        "requiresProxy": target.map(|target| target.requires_proxy).unwrap_or(false),
        "issues": issues,
    })
}

fn apply_desktop_target(target: &DesktopConfigTarget) -> Result<Value, String> {
    let paths = CodexPaths::from_home_env().map_err(|e| e.to_string())?;
    let result = apply_provider(
        &paths,
        &ApplyConfig {
            base_url: &target.base_url,
            gateway_api_key: &target.api_key,
            supports_1m: target.supports_1m,
            provider_name: &target.provider_name,
            default_model: &target.default_model,
            model_mappings: Some(&target.model_mappings),
            model_capabilities: Some(&target.model_capabilities),
            app_version: APP_VERSION,
        },
    )
    .map_err(|e| format!("apply 失败: {e}"))?;
    serde_json::to_value(result).map_err(|e| format!("apply 结果序列化失败: {e}"))
}

// moved to handlers::proxy::start_proxy_if_needed

async fn sync_desktop_for_active_provider(state: &AdminState) -> Value {
    let mut cfg = match load_registry() {
        Ok(cfg) => cfg,
        Err(e) => {
            return json!({"attempted": true, "success": false, "message": e});
        }
    };
    let Some(provider) = active_provider(&cfg) else {
        return json!({
            "attempted": false,
            "success": false,
            "message": "没有默认提供商",
        });
    };

    let target = desktop_config_target_for_provider(&mut cfg, &provider, None);
    if let Err(e) = save_registry(&cfg) {
        return json!({"attempted": true, "success": false, "message": e});
    }

    let mut proxy_started = false;
    if target.requires_proxy {
        match start_proxy_if_needed(&state.proxy_manager, target.proxy_port).await {
            Ok(started) => proxy_started = started,
            Err(e) => {
                return json!({"attempted": true, "success": false, "mode": target.mode, "requiresProxy": target.requires_proxy, "message": e});
            }
        }
    } else {
        state.proxy_manager.stop_silent();
    }

    match apply_desktop_target(&target) {
        Ok(mut result) => {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("attempted".into(), Value::Bool(true));
                obj.insert("success".into(), Value::Bool(true));
                obj.insert("mode".into(), Value::String(target.mode.to_owned()));
                obj.insert("requiresProxy".into(), Value::Bool(target.requires_proxy));
                obj.insert("proxyStarted".into(), Value::Bool(proxy_started));
            }
            result
        }
        Err(e) => {
            json!({"attempted": true, "success": false, "mode": target.mode, "requiresProxy": target.requires_proxy, "proxyStarted": proxy_started, "message": e})
        }
    }
}

pub async fn auto_apply_on_startup_if_enabled(proxy_manager: Arc<ProxyManager>) -> Value {
    let cfg = match load_registry() {
        Ok(cfg) => cfg,
        Err(e) => {
            return json!({"applied": false, "requiresProxy": false, "proxyStarted": false, "message": format!("failed: {e}")})
        }
    };
    if !read_setting_bool(&cfg, "autoApplyOnStart", true) {
        return json!({"applied": false, "requiresProxy": false, "proxyStarted": false, "message": "disabled by settings"});
    }
    if active_provider(&cfg).is_none() {
        return json!({"applied": false, "requiresProxy": false, "proxyStarted": false, "message": "no active provider; skip"});
    }
    let state = AdminState { proxy_manager };
    let result = sync_desktop_for_active_provider(&state).await;
    if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        return json!({
            "applied": true,
            "requiresProxy": result.get("requiresProxy").and_then(|v| v.as_bool()).unwrap_or(false),
            "proxyStarted": result.get("proxyStarted").and_then(|v| v.as_bool()).unwrap_or(false),
            "message": format!("applied {}", active_provider_name(&cfg)),
        });
    }
    json!({
        "applied": false,
        "requiresProxy": result.get("requiresProxy").and_then(|v| v.as_bool()).unwrap_or(false),
        "proxyStarted": result.get("proxyStarted").and_then(|v| v.as_bool()).unwrap_or(false),
        "message": format!("failed: {}", result.get("message").and_then(|v| v.as_str()).unwrap_or("unknown")),
    })
}

pub fn restore_codex_if_enabled(reason: &str) -> Value {
    let cfg = match load_registry() {
        Ok(cfg) => cfg,
        Err(e) => {
            return json!({"attempted": true, "restored": false, "success": false, "reason": reason, "message": e})
        }
    };
    if !read_setting_bool(&cfg, "restoreCodexOnExit", true) {
        return json!({"attempted": false, "restored": false, "success": true, "reason": reason, "message": "disabled by settings"});
    }
    let paths = match CodexPaths::from_home_env() {
        Ok(paths) => paths,
        Err(e) => {
            return json!({"attempted": true, "restored": false, "success": false, "reason": reason, "message": e.to_string()})
        }
    };
    if !has_snapshot(&paths) {
        return json!({"attempted": false, "restored": false, "success": true, "reason": reason, "message": "no snapshot; skip"});
    }
    match restore_codex_state(&paths) {
        Ok(restored) => {
            json!({"attempted": true, "restored": restored, "success": true, "reason": reason})
        }
        Err(e) => {
            json!({"attempted": true, "restored": false, "success": false, "reason": reason, "message": e.to_string()})
        }
    }
}

pub async fn switch_provider_and_sync(
    proxy_manager: Arc<ProxyManager>,
    provider_id: String,
) -> Value {
    let mut cfg = match load_registry() {
        Ok(cfg) => cfg,
        Err(e) => return json!({"success": false, "message": e}),
    };
    if provider_index(&cfg, &provider_id).is_none() {
        return json!({"success": false, "message": "提供商不存在"});
    }
    cfg.as_object_mut()
        .unwrap()
        .insert("activeProvider".into(), Value::String(provider_id));
    if let Err(e) = save_registry(&cfg) {
        return json!({"success": false, "message": e});
    }
    let state = AdminState { proxy_manager };
    let desktop_sync = sync_desktop_for_active_provider(&state).await;
    json!({
        "success": true,
        "message": "默认提供商已更新",
        "desktopSync": desktop_sync,
    })
}

// moved to handlers::settings::ensure_settings_object

fn provider_index(cfg: &RawConfig, id: &str) -> Option<usize> {
    cfg.get("providers")
        .and_then(|v| v.as_array())?
        .iter()
        .position(|p| {
            p.as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                == Some(id)
        })
}

pub(super) fn active_provider(cfg: &RawConfig) -> Option<Value> {
    let active_id = cfg
        .get("activeProvider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let providers = cfg.get("providers").and_then(|v| v.as_array())?;
    let chosen = match active_id {
        Some(id) => providers.iter().find(|p| {
            p.as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                == Some(id.as_str())
        }),
        None => providers.first(),
    };
    chosen.cloned()
}

// moved to handlers::common::generate_gateway_key_value
// moved to handlers::common::random_hex

// moved to handlers::settings::app_config_dir
// moved to handlers::settings::app_config_file
// moved to handlers::settings::app_backup_dir
// moved to handlers::settings::system_time_iso_seconds
// moved to handlers::settings::default_config_value

// moved to handlers::settings::normalize_imported_provider
// moved to handlers::settings::normalize_imported_config
// moved to handlers::settings::preserve_existing_provider_secrets
// moved to handlers::settings::create_config_backup
// moved to handlers::settings::list_config_backups

// moved to handlers::common::instance_info
// moved to handlers::common::instance_show_window
// moved to handlers::common::status

// ── /api/providers ────────────────────────────────────────────────────

pub async fn list_providers() -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers: Vec<Value> = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(public_provider)
        .collect();
    let active_id = cfg
        .get("activeProvider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    Json(json!({
        "providers": providers,
        "activeId": active_id,
    }))
    .into_response()
}

pub async fn get_secret(Path(id): Path<String>) -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let provider = providers.iter().find(|p| {
        p.as_object()
            .and_then(|o| o.get("id"))
            .and_then(|v| v.as_str())
            == Some(id.as_str())
    });
    match provider {
        Some(p) => Json(json!({
            "apiKey": p.get("apiKey").and_then(|v| v.as_str()).unwrap_or(""),
            "extraHeaders": p.get("extraHeaders").cloned().unwrap_or_else(|| json!({})),
        }))
        .into_response(),
        None => err(StatusCode::NOT_FOUND, "提供商不存在").into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct AddProviderInput {
    pub name: Option<String>,
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(rename = "authScheme")]
    pub auth_scheme: Option<String>,
    #[serde(rename = "apiFormat")]
    pub api_format: Option<String>,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    pub models: Option<Value>,
    #[serde(rename = "extraHeaders")]
    pub extra_headers: Option<Value>,
    #[serde(rename = "modelCapabilities")]
    pub model_capabilities: Option<Value>,
    #[serde(rename = "requestOptions")]
    pub request_options: Option<Value>,
}

pub async fn add_provider(Json(input): Json<AddProviderInput>) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let existing_ids: Vec<String> = providers
        .iter()
        .filter_map(|p| {
            p.as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
        })
        .collect();
    let new_id = fresh_provider_id(&existing_ids);

    let mut new_provider = serde_json::Map::new();
    new_provider.insert("id".into(), Value::String(new_id.clone()));
    new_provider.insert(
        "name".into(),
        Value::String(input.name.unwrap_or_else(|| "Unnamed Provider".into())),
    );
    new_provider.insert(
        "baseUrl".into(),
        Value::String(input.base_url.unwrap_or_default()),
    );
    new_provider.insert(
        "authScheme".into(),
        Value::String(input.auth_scheme.unwrap_or_else(|| "bearer".into())),
    );
    new_provider.insert(
        "apiFormat".into(),
        Value::String(
            input
                .api_format
                .filter(|s| matches!(s.as_str(), "openai_chat" | "responses"))
                .unwrap_or_else(|| "responses".into()),
        ),
    );
    new_provider.insert(
        "apiKey".into(),
        Value::String(input.api_key.unwrap_or_default()),
    );
    new_provider.insert(
        "models".into(),
        input.models.unwrap_or_else(|| {
            json!({"default":"","gpt_5_5":"","gpt_5_4":"","gpt_5_4_mini":"","gpt_5_3_codex":"","gpt_5_2":""})
        }),
    );
    new_provider.insert(
        "extraHeaders".into(),
        input.extra_headers.unwrap_or_else(|| json!({})),
    );
    new_provider.insert(
        "modelCapabilities".into(),
        input.model_capabilities.unwrap_or_else(|| json!({})),
    );
    new_provider.insert(
        "requestOptions".into(),
        input.request_options.unwrap_or_else(|| json!({})),
    );
    new_provider.insert("isBuiltin".into(), Value::Bool(false));
    new_provider.insert("sortIndex".into(), Value::Number(providers.len().into()));

    let new_provider_value = Value::Object(new_provider);
    let mut new_providers = providers;
    new_providers.push(new_provider_value.clone());
    let was_empty = new_providers.len() == 1;

    let obj = cfg.as_object_mut().unwrap();
    obj.insert("providers".into(), Value::Array(new_providers));
    if was_empty {
        obj.insert("activeProvider".into(), Value::String(new_id));
    }
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true, "provider": public_provider(&new_provider_value)})).into_response()
}

pub async fn update_provider(
    Path(id): Path<String>,
    Json(input): Json<AddProviderInput>,
) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let Some(idx) = provider_index(&cfg, &id) else {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    };
    let providers = cfg
        .get_mut("providers")
        .and_then(|v| v.as_array_mut())
        .expect("providers array");
    let existing = providers[idx].as_object().unwrap().clone();
    let mut updated = existing.clone();
    let is_builtin = existing
        .get("isBuiltin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if let Some(name) = input.name {
        updated.insert("name".into(), Value::String(name));
    }
    if !is_builtin {
        if let Some(base_url) = input.base_url {
            updated.insert("baseUrl".into(), Value::String(base_url));
        }
    }
    if let Some(auth_scheme) = input.auth_scheme {
        updated.insert("authScheme".into(), Value::String(auth_scheme));
    }
    if let Some(api_format) = input.api_format {
        let normalized = if matches!(api_format.as_str(), "openai_chat" | "responses") {
            api_format
        } else {
            "responses".to_owned()
        };
        updated.insert("apiFormat".into(), Value::String(normalized));
    }
    // apiKey 留空表示"不修改"
    if let Some(key) = input.api_key.filter(|s| !s.is_empty()) {
        updated.insert("apiKey".into(), Value::String(key));
    }
    if let Some(headers) = input.extra_headers {
        if !headers.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            updated.insert("extraHeaders".into(), headers);
        }
    }
    if let Some(caps) = input.model_capabilities {
        updated.insert("modelCapabilities".into(), caps);
    }
    if let Some(opts) = input.request_options {
        updated.insert("requestOptions".into(), opts);
    }
    if let Some(models) = input.models {
        if models.is_object() {
            let mut merged = existing
                .get("models")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            for (k, v) in models.as_object().unwrap() {
                merged.insert(k.clone(), v.clone());
            }
            updated.insert("models".into(), Value::Object(merged));
        }
    }
    updated.insert("id".into(), Value::String(id));
    updated.insert("isBuiltin".into(), Value::Bool(is_builtin));

    providers[idx] = Value::Object(updated.clone());
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true, "provider": public_provider(&Value::Object(updated))}))
        .into_response()
}

pub async fn delete_provider(Path(id): Path<String>) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let original_len = providers.len();
    let mut remaining: Vec<Value> = providers
        .into_iter()
        .filter(|p| {
            p.as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                != Some(id.as_str())
        })
        .collect();
    if remaining.len() == original_len {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    }
    for (i, p) in remaining.iter_mut().enumerate() {
        if let Some(o) = p.as_object_mut() {
            o.insert("sortIndex".into(), Value::Number(i.into()));
        }
    }
    let active = cfg
        .get("activeProvider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let new_active = match active {
        Some(a) if a == id => remaining
            .first()
            .and_then(|p| p.as_object())
            .and_then(|o| o.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| Value::String(s.to_owned()))
            .unwrap_or(Value::Null),
        Some(a) => Value::String(a),
        None => Value::Null,
    };
    let obj = cfg.as_object_mut().unwrap();
    obj.insert("providers".into(), Value::Array(remaining));
    obj.insert("activeProvider".into(), new_active);
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true})).into_response()
}

pub async fn set_default_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let result = switch_provider_and_sync(state.proxy_manager.clone(), id).await;
    if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        Json(result).into_response()
    } else {
        let status = if result.get("message").and_then(|v| v.as_str()) == Some("提供商不存在")
        {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        err(
            status,
            result
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("提供商不存在"),
        )
        .into_response()
    }
}

pub async fn activate_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    set_default_provider(State(state), Path(id)).await
}

#[derive(Debug, Deserialize)]
pub struct ReorderInput {
    #[serde(rename = "providerIds")]
    pub provider_ids: Vec<String>,
}

pub async fn reorder_providers(Json(input): Json<ReorderInput>) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let by_id: std::collections::HashMap<String, Value> = providers
        .iter()
        .filter_map(|p| {
            let id = p
                .as_object()
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())?
                .to_owned();
            Some((id, p.clone()))
        })
        .collect();
    let mut ordered = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in &input.provider_ids {
        if let Some(p) = by_id.get(id) {
            if seen.insert(id.clone()) {
                ordered.push(p.clone());
            }
        }
    }
    for p in &providers {
        if let Some(id) = p
            .as_object()
            .and_then(|o| o.get("id"))
            .and_then(|v| v.as_str())
        {
            if seen.insert(id.to_owned()) {
                ordered.push(p.clone());
            }
        }
    }
    if ordered.len() != providers.len() {
        return err(StatusCode::BAD_REQUEST, "排序数量不一致").into_response();
    }
    for (i, p) in ordered.iter_mut().enumerate() {
        if let Some(o) = p.as_object_mut() {
            o.insert("sortIndex".into(), Value::Number(i.into()));
        }
    }
    let public_ordered: Vec<Value> = ordered.iter().map(public_provider).collect();
    let obj = cfg.as_object_mut().unwrap();
    obj.insert("providers".into(), Value::Array(ordered));
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true, "providers": public_ordered})).into_response()
}

// /api/providers/{id}/draft —— v1 当 update 用,我们直接复用
pub async fn save_draft(
    Path(id): Path<String>,
    Json(input): Json<AddProviderInput>,
) -> impl IntoResponse {
    update_provider(Path(id), Json(input)).await
}

#[derive(Debug, Deserialize)]
pub struct UpdateModelsInput {
    pub models: Value,
}

pub async fn update_models(
    Path(id): Path<String>,
    Json(input): Json<UpdateModelsInput>,
) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let Some(idx) = provider_index(&cfg, &id) else {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    };
    let providers = cfg
        .get_mut("providers")
        .and_then(|v| v.as_array_mut())
        .unwrap();
    if let Some(o) = providers[idx].as_object_mut() {
        o.insert("models".into(), input.models);
    }
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({"success": true})).into_response()
}

// ── /api/presets ─────────────────────────────────────────────────────

pub async fn list_presets() -> impl IntoResponse {
    let presets: Vec<Value> = builtin_presets().to_vec();
    Json(json!({"presets": presets})).into_response()
}

// ── /api/desktop/* ───────────────────────────────────────────────────

pub async fn desktop_status() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let configured = has_snapshot(&paths);
    let cfg = load_registry().unwrap_or_else(|_| json!({}));
    let proxy_port = read_proxy_port(&cfg);
    let actual_base_url = read_codex_toml_root_string(&paths, "openai_base_url");
    let actual_api_key_present = codex_openai_api_key_present(&paths);
    let desktop_target = desktop_target_for_active_provider(&cfg);
    let fallback_base_url = desktop_target
        .as_ref()
        .map(|target| target.base_url.clone())
        .unwrap_or_else(|| format!("http://127.0.0.1:{proxy_port}"));
    let api_key_present = actual_api_key_present
        || desktop_target
            .as_ref()
            .map(|target| !target.api_key.is_empty())
            .unwrap_or_else(|| !read_gateway_key(&cfg).is_empty());
    let health = desktop_health(
        Some(&paths),
        configured,
        actual_base_url.as_deref(),
        actual_api_key_present,
        desktop_target.as_ref(),
    );
    Json(json!({
        "configured": configured,
        "health": health,
        "keys": {
            "inferenceProvider": "gateway",
            "inferenceGatewayBaseUrl": actual_base_url.unwrap_or(fallback_base_url),
            "inferenceGatewayApiKey": if api_key_present { "******" } else { "" },
            "inferenceGatewayAuthScheme": "bearer",
            "inferenceModels": desktop_inference_models_json(desktop_target.as_ref()),
        },
    }))
    .into_response()
}

pub async fn desktop_configure() -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let Some(active) = active_provider(&cfg) else {
        return err(StatusCode::BAD_REQUEST, "请先添加 provider").into_response();
    };
    let target = desktop_config_target_for_provider(&mut cfg, &active, None);
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    match apply_desktop_target(&target) {
        Ok(mut result) => {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("success".into(), Value::Bool(true));
                obj.insert("mode".into(), Value::String(target.mode.to_owned()));
                obj.insert("requiresProxy".into(), Value::Bool(target.requires_proxy));
            }
            Json(result).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn desktop_clear() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match restore_codex_state(&paths) {
        Ok(_) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn desktop_snapshot_status() -> impl IntoResponse {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    Json(json!({
        "hasSnapshot": has_snapshot(&paths),
    }))
    .into_response()
}

pub async fn restart_codex_app() -> impl IntoResponse {
    match launch_codex_app_restart(std::env::consts::OS) {
        Ok(_) => Json(json!({"success": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// moved to handlers::common::version

// ── /api/proxy/* ─────────────────────────────────────────────────────
// moved to handlers::proxy::*

// moved to handlers::settings::get_settings
// moved to handlers::settings::save_settings
// moved to handlers::settings::create_backup
// moved to handlers::settings::list_backups
// moved to handlers::settings::export_config
// moved to handlers::settings::import_config

// moved to handlers::update::UpdateCheckQuery / UpdateInstallInput / update_check / update_install

// ── /api/feedback ────────────────────────────────────────────────────

// moved to handlers::feedback::submit_feedback
// moved to handlers::feedback::submit_feedback_with_body

// ── 测速 / 模型探测 / 兼容性 ─────────────────────────────────

pub async fn test_provider(Path(id): Path<String>) -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let provider = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .and_then(|providers| {
            providers.iter().find(|provider| {
                provider
                    .as_object()
                    .and_then(|o| o.get("id"))
                    .and_then(|v| v.as_str())
                    == Some(id.as_str())
            })
        });
    let Some(provider) = provider else {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    };
    Json(test_provider_connection(provider).await).into_response()
}

pub async fn query_provider_usage(Path(id): Path<String>) -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let provider = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .and_then(|providers| {
            providers.iter().find(|provider| {
                provider
                    .as_object()
                    .and_then(|o| o.get("id"))
                    .and_then(|v| v.as_str())
                    == Some(id.as_str())
            })
        });
    let Some(provider) = provider else {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    };
    let result = query_provider_usage_impl(provider).await;
    Json(result).into_response()
}

pub async fn provider_compatibility() -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let providers: Vec<Value> = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(provider_compatibility_item)
        .collect();
    let experimental_count = providers
        .iter()
        .filter(|item| item.get("level").and_then(|v| v.as_str()) == Some("experimental"))
        .count();
    Json(json!({
        "success": true,
        "providers": providers,
        "experimentalCount": experimental_count,
    }))
    .into_response()
}

pub async fn test_provider_payload(Json(payload): Json<Value>) -> impl IntoResponse {
    Json(test_provider_connection(&payload).await).into_response()
}

pub async fn fetch_provider_models(Path(id): Path<String>) -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let provider = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .and_then(|providers| {
            providers.iter().find(|provider| {
                provider
                    .as_object()
                    .and_then(|o| o.get("id"))
                    .and_then(|v| v.as_str())
                    == Some(id.as_str())
            })
        });
    let Some(provider) = provider else {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    };
    let result = fetch_provider_models_impl(provider).await;
    let status = if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(result)).into_response()
}

pub async fn fetch_provider_models_payload(Json(payload): Json<Value>) -> impl IntoResponse {
    let result = fetch_provider_models_impl(&payload).await;
    let status = if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(result)).into_response()
}

pub async fn autofill_provider_models(Path(id): Path<String>) -> impl IntoResponse {
    let mut cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let Some(idx) = provider_index(&cfg, &id) else {
        return err(StatusCode::NOT_FOUND, "提供商不存在").into_response();
    };
    let provider = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .and_then(|providers| providers.get(idx))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = fetch_provider_models_impl(&provider).await;
    if result.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return (StatusCode::BAD_REQUEST, Json(result)).into_response();
    }
    let suggested = result
        .get("suggested")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(providers) = cfg.get_mut("providers").and_then(|v| v.as_array_mut()) {
        if let Some(provider) = providers.get_mut(idx).and_then(|v| v.as_object_mut()) {
            provider.insert("models".into(), suggested.clone());
        }
    }
    if let Err(e) = save_registry(&cfg) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({
        "success": true,
        "models": result.get("models").cloned().unwrap_or_else(|| json!([])),
        "suggested": suggested,
        "endpoint": result.get("endpoint").cloned().unwrap_or(Value::Null),
        "message": "模型映射已自动填充",
    }))
    .into_response()
}

// moved to handlers::common::_state_typecheck

#[derive(Serialize)]
pub struct _Marker;

#[cfg(test)]
mod tests {
    use super::*;

    fn with_isolated_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        static HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = HOME_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();

        struct EnvGuard {
            home: Option<std::ffi::OsString>,
            userprofile: Option<std::ffi::OsString>,
            root: PathBuf,
        }

        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match &self.userprofile {
                    Some(value) => std::env::set_var("USERPROFILE", value),
                    None => std::env::remove_var("USERPROFILE"),
                }
                let _ = fs::remove_dir_all(&self.root);
            }
        }

        let root = std::env::temp_dir().join(format!(
            "cas-admin-test-{}-{}",
            std::process::id(),
            random_hex(6)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let env_guard = EnvGuard {
            home: std::env::var_os("HOME"),
            userprofile: std::env::var_os("USERPROFILE"),
            root: root.clone(),
        };
        std::env::set_var("HOME", &root);
        std::env::remove_var("USERPROFILE");

        let result = f(&root);
        drop(env_guard);
        result
    }

    fn config_with_secret() -> Value {
        json!({
            "version": APP_VERSION,
            "activeProvider": "p1",
            "gatewayApiKey": "cas_existing",
            "providers": [{
                "id": "p1",
                "name": "Provider One",
                "baseUrl": "https://api.example.com/v1",
                "authScheme": "bearer",
                "apiFormat": "openai_chat",
                "apiKey": "sk-existing",
                "extraHeaders": {"x-extra-secret": "secret-header"},
                "models": {"default": "model-one"},
                "sortIndex": 0
            }],
            "settings": {
                "theme": "default",
                "language": "zh",
                "proxyPort": 18080,
                "adminPort": 18081,
                "autoStart": false,
                "autoApplyOnStart": true,
                "exposeAllProviderModels": false,
                "restoreCodexOnExit": true,
                "updateUrl": "https://github.com/Cmochance/codex-app-transfer/releases/latest/download/latest.json"
            }
        })
    }

    #[test]
    fn provider_test_url_matches_legacy_chat_rules() {
        assert_eq!(
            build_provider_test_url("https://api.example.com/v1", "openai_chat"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            build_provider_test_url("https://api.example.com/v1/chat/completions", "openai_chat"),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn provider_test_url_matches_legacy_responses_rules() {
        assert_eq!(
            build_provider_test_url("https://api.example.com/v1", "responses"),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(
            build_provider_test_url("https://api.example.com", "responses"),
            "https://api.example.com/v1/responses"
        );
    }

    #[test]
    fn provider_test_model_prefers_real_provider_mapping() {
        let provider = json!({
            "models": {
                "default": "kimi-k2.6[1m]",
                "gpt_5_5": "gpt-side-name"
            }
        });

        assert_eq!(provider_test_model(&provider), "kimi-k2.6");
    }

    #[test]
    fn fetch_provider_models_reads_openai_compatible_models() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            use axum::{routing::get, Router};
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new().route(
                "/v1/models",
                get(|| async {
                    Json(json!({
                        "data": [
                            {"id": "text-embedding-3-small"},
                            {"id": "deepseek-v4-pro"},
                            {"id": "deepseek-chat"}
                        ]
                    }))
                }),
            );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let provider = json!({
                "baseUrl": format!("http://{addr}/v1"),
                "apiFormat": "responses",
                "authScheme": "none"
            });
            let result = fetch_provider_models_impl(&provider).await;
            server.abort();

            assert_eq!(result.get("success").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(
                result.get("endpoint").and_then(|v| v.as_str()),
                Some(format!("http://{addr}/v1/models").as_str())
            );
            assert_eq!(
                result.get("models").and_then(|v| v.as_array()).cloned(),
                Some(vec![
                    json!("text-embedding-3-small"),
                    json!("deepseek-v4-pro"),
                    json!("deepseek-chat"),
                ])
            );
            assert_eq!(result["suggested"]["default"], json!("deepseek-v4-pro"));
        });
    }

    #[test]
    fn provider_connection_posts_legacy_minimal_ping_after_probe_fallback() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            use axum::{routing::post, Router};
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new().route(
                "/v1/chat/completions",
                post(Json(json!({"id": "ok", "choices": []}))),
            );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let provider = json!({
                "name": "Mock OpenAI Chat",
                "baseUrl": format!("http://{addr}/v1"),
                "apiFormat": "openai_chat",
                "apiKey": "test-key",
                "models": {"default": "deepseek-chat"}
            });
            let result = test_provider_connection(&provider).await;
            server.abort();

            assert_eq!(result.get("success").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(result.get("ok").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(result.get("statusCode").and_then(|v| v.as_u64()), Some(200));
        });
    }

    #[test]
    fn provider_connection_distinguishes_invalid_url_and_bad_key() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let invalid = json!({
                "baseUrl": "not a url",
                "apiFormat": "responses",
            });
            let result = test_provider_connection(&invalid).await;
            assert_eq!(result["success"], json!(false));
            assert_eq!(result["message"], json!("API 地址无效"));

            use axum::{
                http::{HeaderMap as AxumHeaderMap, StatusCode as AxumStatusCode},
                routing::post,
                Router,
            };
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new().route(
                "/v1/chat/completions",
                post(|headers: AxumHeaderMap| async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok())
                        == Some("Bearer good-key")
                    {
                        (AxumStatusCode::OK, Json(json!({"id": "ok", "choices": []})))
                    } else {
                        (
                            AxumStatusCode::UNAUTHORIZED,
                            Json(json!({"error": "bad key"})),
                        )
                    }
                }),
            );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let bad_key = json!({
                "name": "Mock Provider",
                "baseUrl": format!("http://{addr}/v1"),
                "apiFormat": "openai_chat",
                "apiKey": "bad-key",
                "models": {"default": "deepseek-chat"}
            });
            let result = test_provider_connection(&bad_key).await;
            server.abort();

            assert_eq!(result["success"], json!(true));
            assert_eq!(result["ok"], json!(false));
            assert_eq!(result["statusCode"], json!(401));
            assert!(result["message"]
                .as_str()
                .unwrap_or("")
                .contains("认证失败"));
        });
    }

    #[test]
    fn provider_usage_preserves_legacy_no_key_and_unsupported_payloads() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let no_key = json!({
                "name": "DeepSeek",
                "baseUrl": "https://api.deepseek.com",
            });
            let result = query_provider_usage_impl(&no_key).await;
            assert_eq!(result["success"], json!(false));
            assert_eq!(result["message"], json!("请先保存 API Key"));

            let unsupported = json!({
                "name": "Unknown",
                "baseUrl": "https://api.example.com/v1",
                "apiKey": "test-key",
            });
            let result = query_provider_usage_impl(&unsupported).await;
            assert_eq!(result["success"], json!(true));
            assert_eq!(result["supported"], json!(false));
            assert_eq!(result["items"], json!([]));
        });
    }

    #[test]
    fn balance_payloads_match_legacy_normalization() {
        let deepseek = normalize_balance_payload(
            "deepseek",
            &json!({
                "balance_infos": [{
                    "currency": "CNY",
                    "total_balance": "8.5",
                    "granted_balance": "10",
                    "topped_up_balance": "1.5"
                }]
            }),
        );
        assert_eq!(deepseek[0]["label"], json!("CNY"));
        assert_eq!(deepseek[0]["remaining"], json!(8.5));
        assert_eq!(deepseek[0]["total"], json!(10.0));
        assert_eq!(deepseek[0]["used"], json!(1.5));

        let openrouter = normalize_balance_payload(
            "openrouter",
            &json!({"data": {"total_credits": 12.0, "total_usage": 5.25}}),
        );
        assert_eq!(openrouter[0]["label"], json!("credits"));
        assert_eq!(openrouter[0]["remaining"], json!(6.75));
        assert_eq!(openrouter[0]["unit"], json!("USD"));

        let generic = normalize_balance_payload(
            "siliconflow",
            &json!({"data": {"availableBalance": "3.25", "totalBalance": "4", "usedBalance": "0.75", "currency": "CNY"}}),
        );
        assert_eq!(generic[0]["remaining"], json!(3.25));
        assert_eq!(generic[0]["total"], json!(4.0));
        assert_eq!(generic[0]["used"], json!(0.75));
        assert_eq!(generic[0]["unit"], json!("CNY"));
    }

    #[test]
    fn provider_compatibility_matches_legacy_matrix() {
        let responses = provider_compatibility_item(&json!({
            "id": "responses",
            "name": "Responses",
            "apiFormat": "responses",
        }));
        assert_eq!(responses["level"], json!("stable"));
        assert_eq!(responses["checks"]["streamingTools"], json!(true));

        let openai_chat = provider_compatibility_item(&json!({
            "id": "chat",
            "name": "OpenAI Chat",
            "apiFormat": "openai_chat",
        }));
        assert_eq!(openai_chat["level"], json!("experimental"));
        assert_eq!(openai_chat["checks"]["models"], json!(true));
        assert_eq!(openai_chat["checks"]["streamingTools"], json!(false));

        let legacy_alias = provider_compatibility_item(&json!({
            "id": "legacy",
            "name": "Legacy",
            "apiFormat": "anthropic",
        }));
        assert_eq!(legacy_alias["apiFormat"], json!("responses"));
        assert_eq!(legacy_alias["level"], json!("stable"));
        assert_eq!(legacy_alias["checks"]["models"], json!(true));
    }

    #[test]
    fn config_backup_list_uses_real_files() {
        with_isolated_home(|home| {
            let cfg = config_with_secret();
            save_registry(&cfg).unwrap();

            let backup = create_config_backup("manual").unwrap();
            let name = backup.get("name").and_then(|v| v.as_str()).unwrap();
            assert!(name.starts_with("config-"));
            assert!(name.ends_with(".json"));
            assert!(backup.get("size").and_then(|v| v.as_u64()).unwrap() > 0);

            let backup_path = home.join(".codex-app-transfer").join("backups").join(name);
            assert!(backup_path.is_file());
            let saved: Value =
                serde_json::from_str(&fs::read_to_string(&backup_path).unwrap()).unwrap();
            assert_eq!(saved["providers"][0]["apiKey"], json!("sk-existing"));

            let backups = list_config_backups().unwrap();
            assert_eq!(backups.len(), 1);
            assert_eq!(backups[0]["name"], backup["name"]);
        });
    }

    #[test]
    fn import_config_backs_up_and_preserves_existing_provider_secrets_when_missing() {
        with_isolated_home(|_| {
            save_registry(&config_with_secret()).unwrap();

            let incoming = json!({
                "format": "codex-app-transfer.config",
                "config": {
                    "version": "1.0.3",
                    "activeProvider": "p1",
                    "gatewayApiKey": "cas_imported",
                    "providers": [{
                        "id": "p1",
                        "name": "Imported Provider",
                        "baseUrl": "https://imported.example.com/v1",
                        "authScheme": "bearer",
                        "apiFormat": "openai_chat",
                        "models": {"default": "imported-model"}
                    }],
                    "settings": {"proxyPort": 19090}
                }
            });

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let response = runtime.block_on(async { import_config(Json(incoming)).await });
            assert_eq!(response.into_response().status(), StatusCode::OK);

            let saved = load_registry().unwrap();
            assert_eq!(saved["activeProvider"], json!("p1"));
            assert_eq!(saved["gatewayApiKey"], json!("cas_imported"));
            assert_eq!(saved["settings"]["proxyPort"], json!(19090));
            assert_eq!(saved["providers"][0]["name"], json!("Imported Provider"));
            assert_eq!(saved["providers"][0]["apiKey"], json!("sk-existing"));
            assert_eq!(
                saved["providers"][0]["extraHeaders"]["x-extra-secret"],
                json!("secret-header")
            );

            let backups = list_config_backups().unwrap();
            assert_eq!(backups.len(), 1);
            assert!(backups[0]
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap()
                .contains("before-import"));
        });
    }

    #[test]
    fn desktop_config_target_matches_legacy_proxy_and_direct_modes() {
        let mut proxy_cfg = config_with_secret();
        proxy_cfg["gatewayApiKey"] = Value::Null;
        let proxy_provider = active_provider(&proxy_cfg).unwrap();
        let proxy_target =
            desktop_config_target_for_provider(&mut proxy_cfg, &proxy_provider, Some(19090));
        assert_eq!(proxy_target.mode, "local_proxy");
        assert!(proxy_target.requires_proxy);
        assert_eq!(proxy_target.base_url, "http://127.0.0.1:19090");
        assert!(proxy_target.api_key.starts_with("cas_"));
        assert_eq!(
            proxy_cfg
                .get("gatewayApiKey")
                .and_then(|v| v.as_str())
                .unwrap(),
            proxy_target.api_key
        );

        let mut direct_cfg = config_with_secret();
        direct_cfg["gatewayApiKey"] = Value::Null;
        let direct_provider = json!({
            "id": "direct",
            "name": "Direct Provider",
            "baseUrl": "https://direct.example.com/v1/",
            "authScheme": "bearer",
            "apiFormat": "responses",
            "apiKey": "sk-direct",
            "models": {"default": "direct-model"},
        });
        let direct_target =
            desktop_config_target_for_provider(&mut direct_cfg, &direct_provider, Some(19090));
        assert_eq!(direct_target.mode, "direct_provider");
        assert!(!direct_target.requires_proxy);
        assert_eq!(direct_target.base_url, "https://direct.example.com/v1");
        assert_eq!(direct_target.api_key, "sk-direct");
        assert!(direct_cfg
            .get("gatewayApiKey")
            .and_then(|v| v.as_str())
            .is_none());
    }

    #[test]
    fn startup_auto_apply_starts_proxy_and_exit_restore_uses_snapshot() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        with_isolated_home(|home| {
            runtime.block_on(async {
                let mut cfg = config_with_secret();
                cfg["gatewayApiKey"] = Value::Null;
                cfg["settings"]["proxyPort"] = json!(0);
                save_registry(&cfg).unwrap();

                let codex_dir = home.join(".codex");
                fs::create_dir_all(&codex_dir).unwrap();
                let config_toml = codex_dir.join("config.toml");
                fs::write(&config_toml, "approval_policy = \"on-request\"\n").unwrap();

                let manager = Arc::new(ProxyManager::new());
                let result = auto_apply_on_startup_if_enabled(Arc::clone(&manager)).await;
                assert_eq!(result["applied"], json!(true));
                assert_eq!(result["requiresProxy"], json!(true));
                assert_eq!(result["proxyStarted"], json!(true));
                assert!(manager.status().running);

                let saved = load_registry().unwrap();
                assert!(saved
                    .get("gatewayApiKey")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .starts_with("cas_"));
                let paths = CodexPaths::from_home_env().unwrap();
                assert!(has_snapshot(&paths));
                let applied_config = fs::read_to_string(&config_toml).unwrap();
                assert!(applied_config.contains("approval_policy = \"on-request\""));
                assert!(applied_config.contains("openai_base_url = \"http://127.0.0.1:0\""));

                let restored = restore_codex_if_enabled("test-exit");
                assert_eq!(restored["success"], json!(true));
                assert_eq!(restored["attempted"], json!(true));
                assert!(!has_snapshot(&paths));
                let restored_config = fs::read_to_string(&config_toml).unwrap();
                assert!(restored_config.contains("approval_policy = \"on-request\""));
                assert!(!restored_config.contains("openai_base_url"));
                manager.stop_silent();
            });
        });
    }

    #[test]
    fn startup_auto_apply_respects_disabled_setting() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        with_isolated_home(|_| {
            runtime.block_on(async {
                let mut cfg = config_with_secret();
                cfg["settings"]["autoApplyOnStart"] = json!(false);
                save_registry(&cfg).unwrap();

                let manager = Arc::new(ProxyManager::new());
                let result = auto_apply_on_startup_if_enabled(Arc::clone(&manager)).await;
                assert_eq!(result["applied"], json!(false));
                assert_eq!(result["message"], json!("disabled by settings"));
                assert!(!manager.status().running);
            });
        });
    }

    #[test]
    fn provider_switch_syncs_desktop_without_proxy_for_direct_provider() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        with_isolated_home(|home| {
            runtime.block_on(async {
                let mut cfg = config_with_secret();
                cfg["providers"] = json!([
                    cfg["providers"][0].clone(),
                    {
                        "id": "p2",
                        "name": "Direct Provider",
                        "baseUrl": "https://direct.example.com/v1/",
                        "authScheme": "bearer",
                        "apiFormat": "responses",
                        "apiKey": "sk-direct",
                        "models": {"default": "direct-model"},
                        "sortIndex": 1
                    }
                ]);
                save_registry(&cfg).unwrap();
                fs::create_dir_all(home.join(".codex")).unwrap();

                let manager = Arc::new(ProxyManager::new());
                let result = switch_provider_and_sync(Arc::clone(&manager), "p2".to_owned()).await;
                assert_eq!(result["success"], json!(true));
                assert_eq!(result["desktopSync"]["success"], json!(true));
                assert_eq!(result["desktopSync"]["mode"], json!("direct_provider"));
                assert_eq!(result["desktopSync"]["requiresProxy"], json!(false));
                assert!(!manager.status().running);

                let saved = load_registry().unwrap();
                assert_eq!(saved["activeProvider"], json!("p2"));
                let config_toml = fs::read_to_string(home.join(".codex").join("config.toml")).unwrap();
                assert!(config_toml.contains("openai_base_url = \"https://direct.example.com/v1\""));
                let auth_json: Value =
                    serde_json::from_str(&fs::read_to_string(home.join(".codex").join("auth.json")).unwrap())
                        .unwrap();
                assert_eq!(auth_json["OPENAI_API_KEY"], json!("sk-direct"));
            });
        });
    }

    #[test]
    fn version_endpoint_matches_legacy_shape() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let Json(payload) = version().await;
            assert_eq!(payload, json!({"version": APP_VERSION}));
        });
    }

    #[test]
    fn running_check_command_is_platform_specific() {
        assert_eq!(running_check_command("macos"), vec!["pgrep", "-x", "Codex"]);
        let windows = running_check_command("windows");
        assert_eq!(windows[0], "tasklist");
        assert!(windows.iter().any(|a| a == "IMAGENAME eq Codex.exe"));
        assert_eq!(running_check_command("linux"), vec!["pgrep", "-x", "codex"]);
    }

    #[test]
    fn quit_command_uses_term_then_kill() {
        // graceful = SIGTERM / 普通 taskkill;force = SIGKILL / taskkill /F
        assert_eq!(
            quit_command("macos", false),
            vec!["pkill", "-TERM", "-x", "Codex"]
        );
        assert_eq!(
            quit_command("macos", true),
            vec!["pkill", "-KILL", "-x", "Codex"]
        );
        assert_eq!(
            quit_command("windows", false),
            vec!["taskkill", "/IM", "Codex.exe"]
        );
        assert_eq!(
            quit_command("windows", true),
            vec!["taskkill", "/F", "/IM", "Codex.exe"]
        );
        assert_eq!(
            quit_command("linux", false),
            vec!["pkill", "-TERM", "-x", "codex"]
        );
        assert_eq!(
            quit_command("linux", true),
            vec!["pkill", "-KILL", "-x", "codex"]
        );
    }

    #[test]
    fn open_command_uses_resolved_path_when_available() {
        // macOS 必带 `-n` 强制新实例(重启场景下 LaunchServices 缓存仍以为
        // 旧 Codex 在运行,不加 -n 会让 `open -a` 静默 no-op)。
        assert_eq!(
            open_command("macos", Some("/Applications/Codex.app")),
            vec!["open", "-n", "-a", "/Applications/Codex.app"]
        );
        // 落空时回到裸 app 名,让 LaunchServices 找
        assert_eq!(
            open_command("macos", None),
            vec!["open", "-n", "-a", "Codex"]
        );
        let windows = open_command("windows", None);
        assert_eq!(windows[0], "explorer.exe");
        assert!(windows[1].starts_with("shell:AppsFolder\\"));
        assert!(windows[1].contains("OpenAI.Codex"));
        let linux = open_command("linux", None);
        assert_eq!(linux[0], "sh");
        assert_eq!(linux[1], "-c");
        assert!(linux[2].contains("codex"));
    }

    #[test]
    fn desktop_inference_models_use_current_codex_catalog_slots() {
        let mut cfg = config_with_secret();
        cfg["providers"][0]["models"] = json!({
            "default": "deepseek-v4-pro[1m]",
            "gpt_5_5": "kimi-k2",
            "gpt_5_4": "glm-4.6",
        });
        let provider = active_provider(&cfg).unwrap();
        let target = desktop_config_target_for_provider(&mut cfg, &provider, Some(19090));
        let raw = desktop_inference_models_json(Some(&target));

        assert!(!raw.contains("sonnet"));
        assert!(!raw.contains("haiku"));
        assert!(!raw.contains("opus"));

        let models: Vec<Value> = serde_json::from_str(&raw).unwrap();
        let names: Vec<&str> = models
            .iter()
            .filter_map(|item| item.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"gpt-5.5"));
        assert!(names.contains(&"gpt-5.4"));
        assert!(names.contains(&"gpt-5.4-mini"));
        assert!(names.contains(&"deepseek-v4-pro"));
        assert!(models
            .iter()
            .any(|item| item.get("supports1m").and_then(|v| v.as_bool()) == Some(true)));
    }

    #[test]
    fn desktop_status_reports_current_models_and_health_issues() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        with_isolated_home(|home| {
            runtime.block_on(async {
                let mut cfg = config_with_secret();
                cfg["providers"][0]["models"] = json!({
                    "default": "deepseek-v4-pro[1m]",
                    "gpt_5_5": "kimi-k2",
                });
                save_registry(&cfg).unwrap();

                let codex_dir = home.join(".codex");
                fs::create_dir_all(&codex_dir).unwrap();
                fs::write(
                    codex_dir.join("config.toml"),
                    "openai_base_url = \"http://127.0.0.1:18080\"\n",
                )
                .unwrap();
                fs::write(
                    codex_dir.join("auth.json"),
                    "{\"OPENAI_API_KEY\":\"cas_existing\"}\n",
                )
                .unwrap();

                let response = desktop_status().await.into_response();
                assert_eq!(response.status(), StatusCode::OK);
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let payload: Value = serde_json::from_slice(&body).unwrap();

                let models_raw = payload["keys"]["inferenceModels"].as_str().unwrap();
                assert!(!models_raw.contains("sonnet"));
                assert!(models_raw.contains("gpt-5.5"));
                assert!(models_raw.contains("deepseek-v4-pro"));
                assert_eq!(payload["configured"], json!(false));
                assert_eq!(payload["health"]["needsApply"], json!(true));
                assert_eq!(payload["health"]["oneMillionReady"], json!(false));

                let codes: Vec<&str> = payload["health"]["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|issue| issue.get("code").and_then(|v| v.as_str()))
                    .collect();
                assert!(codes.contains(&"not_managed_by_cas"));
                assert!(codes.contains(&"one_million_not_written"));
            });
        });
    }

    #[test]
    fn desktop_health_reports_base_url_mismatch() {
        with_isolated_home(|home| {
            let cfg = config_with_secret();
            let provider = active_provider(&cfg).unwrap();
            let mut target_cfg = cfg.clone();
            let target =
                desktop_config_target_for_provider(&mut target_cfg, &provider, Some(19090));

            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            fs::write(
                codex_dir.join("config.toml"),
                "openai_base_url = \"http://127.0.0.1:18080\"\n",
            )
            .unwrap();
            fs::write(
                codex_dir.join("auth.json"),
                "{\"OPENAI_API_KEY\":\"cas_old\"}\n",
            )
            .unwrap();

            let paths = CodexPaths::from_home_env().unwrap();
            let actual_base_url = read_codex_toml_root_string(&paths, "openai_base_url");
            let health = desktop_health(
                Some(&paths),
                false,
                actual_base_url.as_deref(),
                true,
                Some(&target),
            );

            assert_eq!(health["needsApply"], json!(true));
            assert_eq!(health["expectedBaseUrl"], json!("http://127.0.0.1:19090"));
            assert_eq!(health["actualBaseUrl"], json!("http://127.0.0.1:18080"));
            let codes: Vec<&str> = health["issues"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|issue| issue.get("code").and_then(|v| v.as_str()))
                .collect();
            assert!(codes.contains(&"not_managed_by_cas"));
            assert!(codes.contains(&"gateway_base_url_mismatch"));
        });
    }

    #[test]
    fn update_platform_version_and_installer_selection_match_legacy() {
        assert_eq!(
            current_update_platform_for("darwin", "arm64"),
            "macos-arm64"
        );
        assert_eq!(current_update_platform_for("win32", "AMD64"), "windows-x64");
        assert_eq!(current_update_platform_for("linux", "x86_64"), "linux-x64");
        assert_eq!(
            current_update_platform_for("freebsd", ""),
            "freebsd-unknown"
        );

        assert!(is_newer_version("v2.0.10", "2.0.9"));
        assert!(is_newer_version("2.1", "2.0.99"));
        assert!(!is_newer_version("2.0", "2.0.0"));

        let windows_assets = vec![
            json!({"name": "Codex-App-Transfer-Windows-Portable.exe"}),
            json!({"name": "Codex-App-Transfer-Windows-Setup.exe"}),
        ];
        assert_eq!(
            pick_windows_installer(&windows_assets).unwrap()["name"],
            json!("Codex-App-Transfer-Windows-Setup.exe")
        );

        let macos_assets = vec![
            json!({"name": "Codex-App-Transfer.dmg"}),
            json!({"name": "Codex-App-Transfer.pkg"}),
        ];
        assert_eq!(
            pick_macos_installer(&macos_assets).unwrap()["name"],
            json!("Codex-App-Transfer.pkg")
        );
        assert_eq!(
            pick_platform_installer(&macos_assets, "linux-x64").unwrap_err(),
            "当前平台暂不支持应用内安装: linux-x64"
        );

        assert_eq!(
            install_command_parts("/tmp/Codex-App-Transfer.pkg", "macos-arm64").unwrap(),
            vec!["open", "/tmp/Codex-App-Transfer.pkg"]
        );
        assert_eq!(
            install_command_parts("C:\\Codex-App-Transfer-Windows-Setup.exe", "windows-x64")
                .unwrap(),
            vec!["C:\\Codex-App-Transfer-Windows-Setup.exe"]
        );
        assert_eq!(
            install_after_quit_command_parts("/tmp/Codex-App-Transfer.pkg", "macos-arm64", 1234)
                .unwrap(),
            vec![
                "/bin/sh",
                "-c",
                "pid=\"$1\"; installer=\"$2\"; while kill -0 \"$pid\" 2>/dev/null; do sleep 0.2; done; exec open \"$installer\"",
                "cas-update-installer",
                "1234",
                "/tmp/Codex-App-Transfer.pkg",
            ]
        );
        assert_eq!(
            install_after_quit_command_parts("/tmp/Codex-App-Transfer.pkg", "macos-arm64", 0)
                .unwrap_err(),
            "等待退出的进程 ID 无效"
        );
    }

    #[test]
    fn update_check_reads_latest_json_and_platform_assets() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            use axum::{routing::get, Router};
            use tokio::net::TcpListener;

            let app = Router::new().route(
                "/latest.json",
                get(|| async {
                    Json(json!({
                        "version": "2.0.2",
                        "pub_date": "2026-05-06",
                        "notes": "update notes",
                        "minimum_supported_version": "2.0.0",
                        "update_protocol": 1,
                        "platforms": {
                            "macos-arm64": {
                                "assets": [
                                    {"name": "Codex-App-Transfer.pkg", "url": "https://example.com/Codex-App-Transfer.pkg"}
                                ]
                            }
                        }
                    }))
                }),
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();
            let result = check_update_impl(
                &client,
                &format!("http://{addr}/latest.json"),
                "2.0.1",
                "macos-arm64",
            )
            .await
            .unwrap();
            server.abort();

            assert_eq!(result["success"], json!(true));
            assert_eq!(result["updateAvailable"], json!(true));
            assert_eq!(result["currentVersion"], json!("2.0.1"));
            assert_eq!(result["latestVersion"], json!("2.0.2"));
            assert_eq!(result["platform"], json!("macos-arm64"));
            assert_eq!(result["pubDate"], json!("2026-05-06"));
            assert_eq!(result["notes"], json!("update notes"));
            assert_eq!(result["minimumSupportedVersion"], json!("2.0.0"));
            assert_eq!(result["updateProtocol"], json!(1));
            assert_eq!(
                result["assets"][0]["name"],
                json!("Codex-App-Transfer.pkg")
            );
        });
    }

    #[test]
    fn update_downloads_installer_and_checks_sha256() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            use axum::{routing::get, Router};
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let installer_bytes = Arc::new(b"pkg-bytes".to_vec());
            let installer_sha = format!("{:x}", Sha256::digest(installer_bytes.as_ref()));
            let app = Router::new()
                .route(
                    "/latest.json",
                    get({
                        let installer_sha = installer_sha.clone();
                        move || async move {
                            Json(json!({
                                "version": "2.0.2",
                                "platforms": {
                                    "macos-arm64": {
                                        "assets": [{
                                            "name": "../Codex-App-Transfer.pkg",
                                            "url": format!("http://{addr}/Codex-App-Transfer.pkg"),
                                            "sha256": installer_sha,
                                        }]
                                    }
                                }
                            }))
                        }
                    }),
                )
                .route(
                    "/Codex-App-Transfer.pkg",
                    get({
                        let installer_bytes = Arc::clone(&installer_bytes);
                        move || {
                            let installer_bytes = Arc::clone(&installer_bytes);
                            async move { installer_bytes.as_ref().clone() }
                        }
                    }),
                );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let target_dir = std::env::temp_dir().join(format!(
                "cas-update-download-{}-{}",
                std::process::id(),
                random_hex(6)
            ));
            let _ = fs::remove_dir_all(&target_dir);
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();
            let result = download_update_impl(
                &client,
                &format!("http://{addr}/latest.json"),
                "2.0.1",
                "macos-arm64",
                Some(&target_dir),
            )
            .await
            .unwrap();
            server.abort();

            assert_eq!(result["downloaded"], json!(true));
            assert_eq!(
                result["installerAsset"]["name"],
                json!("../Codex-App-Transfer.pkg")
            );
            assert_eq!(result["installerSha256"], json!(installer_sha));
            assert_eq!(result["installerSize"], json!(9));
            let installer_path = result["installerPath"].as_str().unwrap();
            assert!(installer_path.ends_with("Codex-App-Transfer.pkg"));
            assert_eq!(fs::read(installer_path).unwrap(), b"pkg-bytes");
            let _ = fs::remove_dir_all(&target_dir);
        });
    }

    #[test]
    fn update_download_rejects_bad_sha_and_unsupported_platform() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            use axum::{routing::get, Router};
            use tokio::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new()
                .route(
                    "/latest.json",
                    get(move || async move {
                        Json(json!({
                            "version": "2.0.2",
                            "platforms": {
                                "macos-arm64": {
                                    "assets": [{
                                        "name": "Codex-App-Transfer.pkg",
                                        "url": format!("http://{addr}/Codex-App-Transfer.pkg"),
                                        "sha256": "bad-sha",
                                    }]
                                },
                                "linux-x64": {
                                    "assets": [{
                                        "name": "Codex-App-Transfer.AppImage",
                                        "url": format!("http://{addr}/Codex-App-Transfer.AppImage")
                                    }]
                                }
                            }
                        }))
                    }),
                )
                .route(
                    "/Codex-App-Transfer.pkg",
                    get(|| async { b"pkg-bytes".to_vec() }),
                );
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let target_dir = std::env::temp_dir().join(format!(
                "cas-update-bad-sha-{}-{}",
                std::process::id(),
                random_hex(6)
            ));
            let _ = fs::remove_dir_all(&target_dir);
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();

            let bad_sha = download_update_impl(
                &client,
                &format!("http://{addr}/latest.json"),
                "2.0.1",
                "macos-arm64",
                Some(&target_dir),
            )
            .await
            .unwrap_err();
            assert_eq!(bad_sha, "安装包校验失败，已取消安装");

            let unsupported = download_update_impl(
                &client,
                &format!("http://{addr}/latest.json"),
                "2.0.1",
                "linux-x64",
                Some(&target_dir),
            )
            .await
            .unwrap_err();
            server.abort();
            assert_eq!(unsupported, "当前平台暂不支持应用内安装: linux-x64");
            let _ = fs::remove_dir_all(&target_dir);
        });
    }

    #[test]
    fn feedback_throttle_matches_legacy_success_and_failure_cooldowns() {
        let throttle = FeedbackThrottle::new();
        assert!(throttle.acquire().is_ok());
        throttle.record_success();
        assert!(throttle.acquire().unwrap_err().contains("刚提交成功"));

        let throttle = FeedbackThrottle::new();
        for _ in 0..FeedbackThrottle::FAILURE_LIMIT {
            throttle.record_failure();
        }
        assert!(throttle
            .acquire()
            .unwrap_err()
            .contains("连续提交失败次数过多"));
    }

    #[test]
    fn feedback_attachments_match_legacy_limits_and_fields() {
        let oversized = STANDARD.encode(vec![b'x'; 5 * 1024 * 1024 + 1]);
        let input = json!({
            "attachments": [
                {"kind": "screenshot", "name": "shot.png", "content_type": "image/png", "content_b64": STANDARD.encode(b"image-bytes")},
                {"kind": "log", "content_type": "not-a-mime", "content_b64": STANDARD.encode(b"log-bytes")},
                {"kind": "log", "name": "too-large.log", "content_b64": oversized},
                {"kind": "log", "name": "bad.log", "content_b64": "%%%"}
            ]
        });

        let attachments = feedback_attachments(&input, 1234);
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].field, "screenshot0");
        assert_eq!(attachments[0].name, "shot.png");
        assert_eq!(attachments[0].content_type, "image/png");
        assert_eq!(attachments[0].raw, b"image-bytes");
        assert_eq!(attachments[1].field, "log0");
        assert_eq!(attachments[1].name, "log-1234.bin");
        assert_eq!(attachments[1].content_type, "application/octet-stream");
        assert_eq!(attachments[1].raw, b"log-bytes");
    }

    #[test]
    fn feedback_proxy_tail_reads_last_200_lines_lossily() {
        let root = std::env::temp_dir().join(format!(
            "cas-feedback-tail-{}-{}",
            std::process::id(),
            random_hex(6)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("proxy.log");

        let mut content = Vec::new();
        for i in 0..205 {
            content.extend_from_slice(format!("line-{i}\n").as_bytes());
        }
        content.extend_from_slice(b"bad-\xff\n");
        fs::write(&path, content).unwrap();

        let tail = feedback_proxy_tail_content(&path).unwrap();
        assert!(!tail.contains("line-0"));
        assert!(tail.contains("line-6"));
        assert!(tail.contains("line-204"));
        assert!(tail.contains("bad-\u{fffd}"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn feedback_submit_posts_json_payload_as_multipart_to_worker() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            use axum::{
                body::Bytes as AxumBytes,
                http::{header::CONTENT_TYPE as AXUM_CONTENT_TYPE, HeaderMap as AxumHeaderMap},
                routing::post,
                Router,
            };
            use tokio::net::TcpListener;

            let seen_body = Arc::new(Mutex::new(Vec::<u8>::new()));
            let seen_content_type = Arc::new(Mutex::new(String::new()));
            let app = Router::new().route(
                "/feedback",
                post({
                    let seen_body = Arc::clone(&seen_body);
                    let seen_content_type = Arc::clone(&seen_content_type);
                    move |headers: AxumHeaderMap, body: AxumBytes| {
                        let seen_body = Arc::clone(&seen_body);
                        let seen_content_type = Arc::clone(&seen_content_type);
                        async move {
                            *seen_content_type.lock().unwrap() = headers
                                .get(AXUM_CONTENT_TYPE)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                                .to_owned();
                            *seen_body.lock().unwrap() = body.to_vec();
                            Json(json!({"ok": true, "id": "fb-test", "email_sent": true}))
                        }
                    }
                }),
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let payload = json!({
                "title": "short title",
                "body": "feedback body",
                "include_diagnostics": false,
                "attachments": [
                    {"kind": "screenshot", "name": "shot.png", "content_type": "image/png", "content_b64": STANDARD.encode(b"png-bytes")},
                    {"kind": "log", "content_b64": STANDARD.encode(b"log-bytes")}
                ]
            });
            let throttle = FeedbackThrottle::new();
            let response = submit_feedback_with_body(
                Bytes::from(payload.to_string()),
                &format!("http://{addr}/feedback"),
                &throttle,
            )
            .await;
            server.abort();

            assert_eq!(response.status(), StatusCode::OK);
            let response_body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let data: Value = serde_json::from_slice(&response_body).unwrap();
            assert_eq!(data["success"], json!(true));
            assert_eq!(data["id"], json!("fb-test"));
            assert_eq!(data["email_sent"], json!(true));

            assert!(seen_content_type
                .lock()
                .unwrap()
                .starts_with("multipart/form-data"));
            let seen = seen_body.lock().unwrap().clone();
            let multipart = String::from_utf8_lossy(&seen);
            assert!(multipart.contains("name=\"meta\""));
            assert!(multipart.contains("name=\"title\""));
            assert!(multipart.contains("short title"));
            assert!(multipart.contains("name=\"body\""));
            assert!(multipart.contains("feedback body"));
            assert!(multipart.contains("name=\"screenshot0\"; filename=\"shot.png\""));
            assert!(multipart.contains("name=\"log0\"; filename=\"log-"));
        });
    }

    #[test]
    fn feedback_submit_preserves_legacy_validation_and_upstream_errors() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            use axum::{routing::post, Router};
            use tokio::net::TcpListener;

            let throttle = FeedbackThrottle::new();
            let response =
                submit_feedback_with_body(Bytes::from("not-json"), "http://127.0.0.1", &throttle)
                    .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let data: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(data["message"], json!("请求体非 JSON"));

            let throttle = FeedbackThrottle::new();
            let response = submit_feedback_with_body(
                Bytes::from(json!({"body": ""}).to_string()),
                "http://127.0.0.1",
                &throttle,
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let data: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(data["message"], json!("请填写描述"));

            let throttle = FeedbackThrottle::new();
            let response = submit_feedback_with_body(
                Bytes::from(json!({"body": "configured"}).to_string()),
                "",
                &throttle,
            )
            .await;
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let data: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(data["message"], json!("反馈服务未配置"));

            let app = Router::new().route(
                "/feedback",
                post(|| async { Json(json!({"ok": false, "error": "worker failed"})) }),
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let throttle = FeedbackThrottle::new();
            let response = submit_feedback_with_body(
                Bytes::from(json!({"body": "goes upstream"}).to_string()),
                &format!("http://{addr}/feedback"),
                &throttle,
            )
            .await;
            server.abort();
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let data: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(data["message"], json!("worker failed"));
        });
    }
}
