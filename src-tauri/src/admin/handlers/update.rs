//! `/api/update/*` —— 升级检查 + 安装包下载 + 平台判断.

use std::fs;
use std::path::{Path as FsPath, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_registry::DEFAULT_UPDATE_URL;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::super::registry_io::load as load_registry;
use super::super::signature::verify_signed_bytes;
use super::common::{err, APP_VERSION};

pub(super) fn current_update_platform() -> String {
    current_update_platform_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub(super) fn current_update_platform_for(raw_platform: &str, raw_machine: &str) -> String {
    let machine = raw_machine.to_ascii_lowercase();
    let arch = match machine.as_str() {
        "amd64" | "x86_64" => "x64".to_owned(),
        "arm64" | "aarch64" => "arm64".to_owned(),
        "" => "unknown".to_owned(),
        value => value.to_owned(),
    };
    let platform = raw_platform.to_ascii_lowercase();
    if platform.starts_with("win") || platform == "windows" {
        return format!("windows-{arch}");
    }
    if platform == "darwin" || platform == "macos" {
        return format!("macos-{arch}");
    }
    if platform.starts_with("linux") {
        return format!("linux-{arch}");
    }
    format!("{platform}-{arch}")
}

pub(super) fn version_parts(version: &str) -> Vec<u64> {
    let text = version.trim().trim_start_matches(['v', 'V']);
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            parts.push(current.parse::<u64>().unwrap_or(0));
            current.clear();
        }
    }
    if !current.is_empty() {
        parts.push(current.parse::<u64>().unwrap_or(0));
    }
    if parts.is_empty() {
        parts.push(0);
    }
    parts
}

pub(super) fn is_newer_version(latest: &str, current: &str) -> bool {
    let mut latest_parts = version_parts(latest);
    let mut current_parts = version_parts(current);
    let width = latest_parts.len().max(current_parts.len());
    latest_parts.resize(width, 0);
    current_parts.resize(width, 0);
    latest_parts > current_parts
}

pub(super) fn validate_update_url(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url.trim())
        .map_err(|_| "update URL must be http or https".to_owned())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("update URL must be http or https".to_owned());
    }
    Ok(parsed.to_string())
}

pub(super) fn safe_asset_name(name: &str) -> Result<String, String> {
    let filename = FsPath::new(name.trim())
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    if filename.is_empty() {
        Err("update asset missing filename".to_owned())
    } else {
        Ok(filename)
    }
}

pub(super) fn asset_filename_from_url(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(|name| name.to_owned())
        })
        .unwrap_or_default()
}

pub(super) fn pick_platform_data<'a>(
    latest_json: &'a Value,
    platform: &str,
) -> Result<&'a Value, String> {
    latest_json
        .get("platforms")
        .and_then(|v| v.as_object())
        .and_then(|platforms| platforms.get(platform))
        .filter(|v| v.as_object().is_some())
        .ok_or_else(|| format!("latest.json has no asset for platform {platform}"))
}

pub(super) fn allowed_install_extensions(platform: &str) -> &'static [&'static str] {
    if platform.starts_with("windows-") {
        &[".exe"]
    } else if platform.starts_with("macos-") {
        &[".pkg", ".dmg"]
    } else {
        &[]
    }
}

pub(super) fn pick_windows_installer(assets: &[Value]) -> Result<Value, String> {
    assets
        .iter()
        .find(|asset| {
            asset
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .ends_with("windows-setup.exe")
        })
        .cloned()
        .ok_or_else(|| "current release has no Windows installer asset".to_owned())
}

pub(super) fn pick_macos_installer(assets: &[Value]) -> Result<Value, String> {
    if let Some(pkg) = assets.iter().find(|asset| {
        asset
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .ends_with(".pkg")
    }) {
        return Ok(pkg.clone());
    }
    assets
        .iter()
        .find(|asset| {
            asset
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .ends_with(".dmg")
        })
        .cloned()
        .ok_or_else(|| "current release has no macOS installer asset".to_owned())
}

pub(super) fn pick_platform_installer(assets: &[Value], platform: &str) -> Result<Value, String> {
    if platform.starts_with("windows-") {
        return pick_windows_installer(assets);
    }
    if platform.starts_with("macos-") {
        return pick_macos_installer(assets);
    }
    Err(format!(
        "in-app install is not supported on platform: {platform}"
    ))
}

pub(super) fn install_command_parts(path: &str, platform: &str) -> Result<Vec<String>, String> {
    if platform.starts_with("windows-") {
        // **/D= 参数不在 args vec 里** — NSIS `/D=` 必须不带引号,但
        // std::process::Command Windows 序列化时对含空格的 arg 自动加
        // 引号(CommandLineToArgvW 反向规则),会让 /D=D:\\Apps\\Codex App
        // Transfer 被包成 "/D=D:\\Apps\\Codex App Transfer" → NSIS 忽略
        // /D 回默认 InstallDir(chatgpt-codex PR #205 P2 review 指出)。
        //
        // 真实 /D= 通过 launch_update_installer Windows 路径的
        // CommandExt::raw_arg() bypass quoting 注入,这里 args vec 只放
        // installer path 本身。follow-up #36 的语义不变,实现路径分两层。
        return Ok(vec![path.to_owned()]);
    }
    if platform.starts_with("macos-") {
        return Ok(vec!["open".to_owned(), path.to_owned()]);
    }
    Err(format!(
        "in-app install is not supported on platform: {platform}"
    ))
}

/// 返回当前 binary 的 parent dir(字符串形式)。用于 Windows `/D=` 参数。
/// 失败(current_exe / parent / to_str)返 None,caller 应不传 `/D=` 走默认。
///
/// `#[cfg(any(target_os = "windows", test))]`:production only-Windows
/// 用(launch_update_installer raw_arg),非-Windows production dead 但
/// test 仍跑(verify cargo test runner 路径可解析)。
#[cfg(any(target_os = "windows", test))]
fn current_exe_parent_dir() -> Option<String> {
    std::env::current_exe()
        .ok()?
        .parent()?
        .to_str()
        .map(str::to_owned)
}

/// follow-up #35 a: macOS Gatekeeper translocation 检测。
///
/// 当用户**直接从 .dmg 打开**本 app(未拖到 /Applications/),macOS 把 .app
/// "translocate"到只读临时挂载点 `/private/var/folders/.../AppTranslocation/`
/// 路径下执行 — 此时跑 update install 会失败(translocation 是 read-only)
/// 或装到错位置(替换 mount 副本不影响真 .app)。
///
/// 检测当前 binary 路径含 `/AppTranslocation/`,有则 hard fail 让用户先把
/// .app 拖到 /Applications/ 再升级。借鉴 AiMaMi `update.rs:47-113`。
///
/// 非-macOS 平台 / 检测失败一律返 Ok (不能因边界 case 卡住 update flow)。
pub(super) fn macos_translocation_precheck() -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let Some(exe) = std::env::current_exe().ok() else {
            return Ok(());
        };
        let exe_str = exe.to_string_lossy();
        if exe_str.contains("/AppTranslocation/") {
            return Err(
                "Codex App Transfer 当前从 .dmg 临时挂载点 (AppTranslocation) 运行 — \
                 请先把 .app 拖到 /Applications/ 再执行升级,否则 macOS Gatekeeper 会让升级失败"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

/// follow-up #35 b: 清掉 downloaded installer 的 `com.apple.quarantine`
/// xattr — 否则装完 Gatekeeper 会弹"是否打开此应用"二次弹窗影响体验。
///
/// macOS-only,失败 silent ignore(xattr 不存在是 happy path,权限失败
/// 不应该阻塞 install 主路径)。借鉴 AiMaMi `update.rs:47-113`。
pub(super) fn macos_strip_quarantine(path: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("xattr")
            .args(["-d", "com.apple.quarantine", path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path; // unused on non-macOS
    }
}

#[cfg(test)]
pub(super) fn install_after_quit_command_parts(
    path: &str,
    platform: &str,
    wait_for_pid: u32,
) -> Result<Vec<String>, String> {
    if wait_for_pid == 0 {
        return Err("wait-for-exit pid is invalid".to_owned());
    }
    if platform.starts_with("macos-") {
        return Ok(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "pid=\"$1\"; installer=\"$2\"; while kill -0 \"$pid\" 2>/dev/null; do sleep 0.2; done; exec open \"$installer\"".to_owned(),
            "cas-update-installer".to_owned(),
            wait_for_pid.to_string(),
            path.to_owned(),
        ]);
    }
    install_command_parts(path, platform)
}

pub(super) fn launch_update_installer(
    installer_path: &str,
    platform: &str,
) -> Result<bool, String> {
    // follow-up #35 b: macOS 装前 strip quarantine xattr,防 Gatekeeper
    // 弹"是否打开此应用"二次弹窗。失败 silent ignore 不阻塞主路径。
    if platform.starts_with("macos-") {
        macos_strip_quarantine(installer_path);
    }
    let command = install_command_parts(installer_path, platform)?;
    let Some((program, args)) = command.split_first() else {
        return Err("install command is empty".to_owned());
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // follow-up #36 真正注入 /D= NSIS install dir — 必须 raw_arg bypass
    // Rust 自动 quoting(否则含空格路径如 `D:\Apps\Codex App Transfer`
    // 会被包成 `"/D=D:\\Apps\\Codex App Transfer"` 让 NSIS ignore /D=)。
    // raw_arg 直接 append 到 command line,Windows-only。
    #[cfg(target_os = "windows")]
    if platform.starts_with("windows-") {
        if let Some(install_dir) = current_exe_parent_dir() {
            use std::os::windows::process::CommandExt;
            cmd.raw_arg(format!("/D={install_dir}"));
        }
    }
    cmd.spawn()
        .map(|_| false)
        .map_err(|e| format!("launch installer failed: {e}"))
}

/// 返回当前 binary 真正使用的“规范更新地址”。
/// 优先级：
/// 1. build.rs 通过 CODEX_APP_TRANSFER_REPO 注入的 `CODEX_APP_TRANSFER_DEFAULT_UPDATE_URL`
///    （CI release 时等于实际发布仓库的 latest.json，满足“跟随当前发布仓库”）
/// 2. 库常量 DEFAULT_UPDATE_URL（Cmochance，统一官方源，本地 dev fallback）。
pub(super) fn canonical_update_url() -> String {
    option_env!("CODEX_APP_TRANSFER_DEFAULT_UPDATE_URL")
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_UPDATE_URL.to_owned())
}

pub(super) fn configured_update_url(input: Option<&str>) -> String {
    if let Some(url) = input.map(str::trim).filter(|url| !url.is_empty()) {
        return url.to_owned();
    }
    load_registry()
        .ok()
        .and_then(|cfg| {
            cfg.get("settings")
                .and_then(|settings| settings.get("updateUrl"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(canonical_update_url)
}

/// 从原 URL 推导对应 `.sig` URL —— 注入到 path 段而非裸字符串拼接,
/// 这样 query string / fragment 会被保留 (S3 presigned URL / CDN cache-bust
/// 等场景必要)。
///
/// 例:
/// - `https://host/latest.json?token=abc` → `https://host/latest.json.sig?token=abc`
/// - `https://host/path/installer.dmg#frag` → `https://host/path/installer.dmg.sig#frag`
///
/// codex-bot PR #197 review thread PRRT_kwDOSRcxvM6CpmHk 修 — 之前
/// `format!("{url}.sig")` 在 query string URL 下生成 `?token=abc.sig` 错路径。
fn sig_url_for(url: &str) -> Result<String, String> {
    let mut parsed =
        reqwest::Url::parse(url).map_err(|e| format!("URL parse for .sig failed: {e}"))?;
    let new_path = format!("{}.sig", parsed.path());
    parsed.set_path(&new_path);
    Ok(parsed.to_string())
}

/// 拉同名 `.sig` 文件文本(base64 编码 RSA 签名)。
///
/// 错误信息只 emit HTTP status 数字 + 简短诊断 hint,不附完整 URL — self-host 用
/// token-query URL 时 reqwest `error_for_status_ref()` 默认 format 会把全 URL
/// 含 query 塞错误链, 通过 stderr / 日志 / UI 反馈泄漏。code-reviewer PR #197 NIT-3 修。
/// 区分 404 (操作员漏签) vs 5xx (源站故障) vs 其他 — silent-failure-hunter
/// followup #37 IMPORTANT-2 (诊断 actionability)。
async fn fetch_signature_text(client: &reqwest::Client, sig_url: &str) -> Result<String, String> {
    let response = client
        .get(sig_url)
        .send()
        .await
        .map_err(|e| format!("signature request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let code = status.as_u16();
        let hint = if code == 404 {
            " (check whether .sig is published)"
        } else if status.is_server_error() {
            " (origin error)"
        } else {
            ""
        };
        return Err(format!("signature request failed: HTTP {code}{hint}"));
    }
    response
        .text()
        .await
        .map_err(|e| format!("signature read failed: {e}"))
}

/// in-memory installer 单文件 hard cap, 防 self-host 操作员误配置 / latest.json
/// asset.size 字段失实导致 Vec 无界扩张 OOM 整个 Tauri 进程 (silent-failure-hunter
/// followup #37 IMPORTANT-1)。
///
/// 500MB 是合理上限:历史最大 installer ~50MB (Linux x86_64 .tar.gz),
/// 留 10x headroom 给 SDK 内置 deps / multi-arch fat binary。超此值视为攻击 /
/// 错配 / 自伤场景, 早 fail。
const INSTALLER_SIZE_HARD_CAP: u64 = 500 * 1024 * 1024;

/// 校验 installer bytes sha256 跟 latest.json 声明值匹配。
///
/// 返回 actual sha256 hex (lowercase) 供 caller 写进 download_asset_impl 返回的 JSON。
/// expected_sha 空白 / 缺字段时跳过比较 (latest.json 旧 schema 兼容)。
///
/// 抽独立函数让 update.rs::tests 可以单测 sha256 mismatch 路径,不依赖 mock server +
/// 真签名 (code-reviewer PR #197 IMPORTANT-2 — bad-sha 测试 re-add)。
fn verify_installer_sha256(bytes: &[u8], expected_sha: &str) -> Result<String, String> {
    let actual_sha = format!("{:x}", Sha256::digest(bytes));
    let expected_lower = expected_sha.trim().to_ascii_lowercase();
    if !expected_lower.is_empty() && actual_sha.to_ascii_lowercase() != expected_lower {
        return Err("installer checksum mismatch, install cancelled".to_owned());
    }
    Ok(actual_sha)
}

pub(super) async fn fetch_latest_json(
    client: &reqwest::Client,
    url: &str,
) -> Result<Value, String> {
    let safe_url = validate_update_url(url)?;
    let response = client
        .get(&safe_url)
        .send()
        .await
        .map_err(|e| format!("update URL request failed: {e}"))?;
    response
        .error_for_status_ref()
        .map_err(|e| format!("update URL request failed: {e}"))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("update URL request failed: {e}"))?;

    // RSA 验签 latest.json (防 MITM 改 url + sha256 推任意 installer)。
    // sig URL 约定 path 段后缀 `.sig` — 跟 xtask release-bundle 输出对齐, 通过
    // sig_url_for() 保留 query string / fragment (S3 presigned 等场景必要)。
    // 失败硬 fail:不 fallback sha256-only 也不接受未签 URL (followup #34 决策)。
    let sig_url = sig_url_for(&safe_url)?;
    let sig = fetch_signature_text(client, &sig_url)
        .await
        .map_err(|e| format!("latest.json signature unavailable: {e}"))?;
    verify_signed_bytes(&bytes, &sig).map_err(|e| format!("latest.json signature invalid: {e}"))?;

    let data = serde_json::from_slice::<Value>(&bytes).or_else(|_| {
        let without_bom = bytes
            .strip_prefix(&[0xEF, 0xBB, 0xBF])
            .unwrap_or(bytes.as_ref());
        serde_json::from_slice::<Value>(without_bom)
    });
    let data = data.map_err(|_| "update URL did not return valid JSON".to_owned())?;
    if !data.is_object() {
        return Err("latest.json 格式错误".to_owned());
    }
    Ok(data)
}

pub(super) async fn check_update_impl(
    client: &reqwest::Client,
    url: &str,
    current_version: &str,
    platform: &str,
) -> Result<Value, String> {
    let latest_json = fetch_latest_json(client, url).await?;
    let latest_version = latest_json
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    if latest_version.is_empty() {
        return Err("latest.json 缺少 version 字段".to_owned());
    }
    let platform_data = pick_platform_data(&latest_json, platform)?;
    let assets = platform_data
        .get("assets")
        .cloned()
        .unwrap_or_else(|| json!([]));
    if !assets.is_array() {
        return Err("latest.json assets 字段格式错误".to_owned());
    }
    Ok(json!({
        "success": true,
        "updateAvailable": is_newer_version(&latest_version, current_version),
        "currentVersion": current_version,
        "latestVersion": latest_version,
        "platform": platform,
        "pubDate": latest_json.get("pub_date").cloned().unwrap_or(Value::Null),
        "notes": latest_json.get("notes").cloned().unwrap_or_else(|| json!("")),
        "assets": assets,
        "minimumSupportedVersion": latest_json.get("minimum_supported_version").cloned().unwrap_or(Value::Null),
        "updateProtocol": latest_json.get("update_protocol").cloned().unwrap_or_else(|| json!(1)),
    }))
}

pub(super) async fn download_asset_impl(
    client: &reqwest::Client,
    asset: &Value,
    target_dir: Option<&FsPath>,
    platform: &str,
) -> Result<Value, String> {
    let url = validate_update_url(asset.get("url").and_then(|v| v.as_str()).unwrap_or(""))?;
    // 算 sig URL 必须在 url 被 client.get() 消费之前 (后续 line 393 moves url)。
    // 用 sig_url_for 保留 query string / fragment (S3 presigned 必要)。
    let installer_sig_url = sig_url_for(&url)?;
    let raw_name = asset
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| asset_filename_from_url(&url));
    let filename = safe_asset_name(&raw_name)?;
    let allowed_extensions = allowed_install_extensions(platform);
    if allowed_extensions.is_empty() {
        return Err(format!(
            "in-app install is not supported on platform: {platform}"
        ));
    }
    let lower_name = filename.to_ascii_lowercase();
    if !allowed_extensions
        .iter()
        .any(|ext| lower_name.ends_with(ext))
    {
        return Err(format!(
            "platform supports download-only installer asset: {}",
            allowed_extensions.join(" / ")
        ));
    }

    let updates_dir = target_dir.map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("Codex-App-Transfer")
            .join("updates")
    });
    fs::create_dir_all(&updates_dir).map_err(|e| format!("write installer failed: {e}"))?;
    let target = updates_dir.join(&filename);

    // 流式 download 只累积到 in-memory Vec<u8> — **不写 partial 文件**。
    // 防 codex-bot PR #199 P1 报的 TOCTOU: 之前先写 partial 再 rename, attacker
    // 可在 sig fetch await 期间 swap partial 文件, 让 verify pass 在
    // in_memory 但 rename 安装 attacker 的 payload。skip partial 完全消除
    // 中间文件 race window —— verify pass 后才一次性 fs::write target。
    //
    // 也顺带防 silent-failure-hunter PR #197 IMPORTANT-2 的 Linux /tmp TOCTOU
    // (verify 跟 sha256 都 in-memory 一份 bytes, 不再 fs::read)。
    //
    // peak RAM ~ installer size (35MB dmg / 30MB exe) — desktop Tauri 可接受。
    // 流式 length check 仍必要防 latest.json size 字段失实 → in-stream OOM。
    let declared_size = asset.get("size").and_then(|v| v.as_u64());
    if let Some(size) = declared_size {
        if size > INSTALLER_SIZE_HARD_CAP {
            return Err(format!(
                "installer size {} exceeds hard cap {} bytes",
                size, INSTALLER_SIZE_HARD_CAP
            ));
        }
    }
    let initial_cap = declared_size
        .map(|s| s.min(INSTALLER_SIZE_HARD_CAP) as usize)
        .unwrap_or(64 * 1024);
    let mut in_memory: Vec<u8> = Vec::with_capacity(initial_cap);
    let download_result: Result<(), String> = async {
        let mut response = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("download installer failed: {e}"))?;
        response
            .error_for_status_ref()
            .map_err(|e| format!("download installer failed: {e}"))?;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("download installer failed: {e}"))?
        {
            if !chunk.is_empty() {
                // in-stream cap check 防 latest.json size 字段失实 / 缺失:
                // 累积超过 hard cap 时 fail 早, 不耗光 RAM。
                if (in_memory.len() as u64).saturating_add(chunk.len() as u64)
                    > INSTALLER_SIZE_HARD_CAP
                {
                    return Err(format!(
                        "installer download exceeded hard cap {} bytes",
                        INSTALLER_SIZE_HARD_CAP
                    ));
                }
                in_memory.extend_from_slice(&chunk);
            }
        }
        Ok(())
    }
    .await;
    download_result?;

    // RSA 验签 in_memory — true trust gate, 必须先于 sha256 check:
    //   - sha256 是 attacker-controlled(它在 latest.json 里, latest.json 改 url
    //     的同时可同步改 sha256, sha256 mismatch 只是 backup signal)
    //   - RSA sig 由 CI 用私钥离线签, attacker 无私钥不能伪造
    // code-reviewer #196 IMPORTANT-1 修。
    let installer_sig = fetch_signature_text(client, &installer_sig_url)
        .await
        .map_err(|e| format!("installer signature unavailable: {e}"))?;
    if let Err(e) = verify_signed_bytes(&in_memory, &installer_sig) {
        return Err(format!("installer signature invalid: {e}"));
    }

    // sha256 redundant integrity check on in_memory bytes (defense-in-depth)。
    // verify_installer_sha256 抽独立函数便于单测 mismatch 路径
    // (code-reviewer PR #197 IMPORTANT-2 测试 re-add)。
    let expected_sha = asset.get("sha256").and_then(|v| v.as_str()).unwrap_or("");
    if expected_sha.trim().is_empty() {
        // RSA verify 已经把住 trust gate, 缺 sha256 不影响安全, 但 self-host
        // 操作员可能误 drop latest.json sha256 字段 — debug log 帮排查。
        tracing::debug!(
            "latest.json asset {} missing sha256, relying on RSA signature only",
            target.display()
        );
    }
    let actual_sha = verify_installer_sha256(&in_memory, expected_sha)?;

    // verify pass 后才把 in_memory 写到 target — attacker 无中间文件可 swap。
    // 不用 partial + rename 因为引入 race window (codex-bot #199 P1);
    // fs::write 不 atomic 但 target 在 install 流程下次启动时仍由 launch_update_installer
    // 直接读 — 单 process 写入完整, 不会被半截 file 触发安装 (write 是 sequential)。
    if target.exists() {
        fs::remove_file(&target).map_err(|e| format!("write installer failed: {e}"))?;
    }
    fs::write(&target, &in_memory).map_err(|e| format!("write installer failed: {e}"))?;
    let size = in_memory.len() as u64;
    Ok(json!({
        "asset": asset,
        "path": target.to_string_lossy(),
        "sha256": actual_sha,
        "size": size,
    }))
}

pub(super) async fn download_update_impl(
    client: &reqwest::Client,
    url: &str,
    current_version: &str,
    platform: &str,
    target_dir: Option<&FsPath>,
) -> Result<Value, String> {
    let mut result = check_update_impl(client, url, current_version, platform).await?;
    if result.get("updateAvailable").and_then(|v| v.as_bool()) != Some(true) {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("downloaded".to_owned(), Value::Bool(false));
            obj.insert(
                "message".to_owned(),
                Value::String("already on the latest version".to_owned()),
            );
        }
        return Ok(result);
    }

    let assets = result
        .get("assets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let installer_asset = pick_platform_installer(&assets, platform)?;
    let downloaded = download_asset_impl(client, &installer_asset, target_dir, platform).await?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("downloaded".to_owned(), Value::Bool(true));
        obj.insert("installerAsset".to_owned(), installer_asset);
        obj.insert(
            "installerPath".to_owned(),
            downloaded.get("path").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "installerSha256".to_owned(),
            downloaded.get("sha256").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "installerSize".to_owned(),
            downloaded.get("size").cloned().unwrap_or(Value::Null),
        );
    }
    Ok(result)
}

// ── /api/update/* ────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct UpdateCheckQuery {
    pub url: Option<String>,
    pub current: Option<String>,
    pub platform: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateInstallInput {
    pub url: Option<String>,
    pub current: Option<String>,
    pub platform: Option<String>,
}

pub async fn update_check(Query(query): Query<UpdateCheckQuery>) -> impl IntoResponse {
    let update_url = configured_update_url(query.url.as_deref());
    if update_url.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "configure latest.json update URL first",
        )
        .into_response();
    }
    let current = query
        .current
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(APP_VERSION)
        .to_owned();
    let platform = query
        .platform
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(current_update_platform);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("update URL request failed: {e}"),
            )
            .into_response()
        }
    };
    match check_update_impl(&client, &update_url, &current, &platform).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

pub async fn update_install(body: Option<Json<UpdateInstallInput>>) -> impl IntoResponse {
    // follow-up #35 a: macOS translocation 前置检查 — 当前 .app 在 .dmg
    // 临时挂载点跑时 hard fail,提示用户先拖到 /Applications/ 再升级。
    // 早期 reject 避免下载完 installer 才发现装不上浪费带宽 + 给用户
    // 清晰错误指引。
    if let Err(e) = macos_translocation_precheck() {
        return err(StatusCode::BAD_REQUEST, e).into_response();
    }
    let input = body.map(|value| value.0).unwrap_or_default();
    let update_url = configured_update_url(input.url.as_deref());
    if update_url.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "configure latest.json update URL first",
        )
        .into_response();
    }
    let current = input
        .current
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(APP_VERSION)
        .to_owned();
    let platform = input
        .platform
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(current_update_platform);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("update URL request failed: {e}"),
            )
            .into_response()
        }
    };
    let mut result =
        match download_update_impl(&client, &update_url, &current, &platform, None).await {
            Ok(result) => result,
            Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
        };
    if result.get("updateAvailable").and_then(|v| v.as_bool()) != Some(true) {
        return Json(result).into_response();
    }
    let installer_path = result
        .get("installerPath")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if installer_path.is_empty() {
        return err(StatusCode::BAD_REQUEST, "download installer failed").into_response();
    }
    let quit_requested = match launch_update_installer(installer_path, &platform) {
        Ok(quit_requested) => quit_requested,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let is_macos = platform.starts_with("macos-");
    if let Some(obj) = result.as_object_mut() {
        obj.insert("success".to_owned(), Value::Bool(true));
        obj.insert("installerStarted".to_owned(), Value::Bool(true));
        obj.insert("quitRequested".to_owned(), Value::Bool(quit_requested));
        obj.insert(
            "message".to_owned(),
            Value::String(if is_macos {
                if quit_requested {
                    "Installer downloaded. App will exit and launch the installer.".to_owned()
                } else {
                    "Installer downloaded and opened. Quit the app, then follow the macOS prompts to finish installing.".to_owned()
                }
            } else {
                "Installer downloaded and launched. It will reuse the previous install location and close any running Codex App Transfer before installing.".to_owned()
            }),
        );
    }
    Json(result).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use super::super::common::random_hex;

    /// verify_installer_sha256 happy / mismatch / empty / whitespace 路径。
    /// code-reviewer PR #197 IMPORTANT-2: bad-sha 测试 re-add — 抽 verify_installer_sha256
    /// 独立函数让单测不需 mock server。
    #[test]
    fn verify_installer_sha256_paths() {
        use sha2::Digest;
        let bytes: &[u8] = b"hello-installer-bytes";
        let real_sha = format!("{:x}", Sha256::digest(bytes));

        // Happy: actual == expected
        assert_eq!(verify_installer_sha256(bytes, &real_sha).unwrap(), real_sha);

        // Mismatch: 期望 error string 完全匹配 ("installer checksum mismatch, install cancelled")
        let bad = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let err = verify_installer_sha256(bytes, bad).unwrap_err();
        assert_eq!(err, "installer checksum mismatch, install cancelled");

        // Empty expected (latest.json 缺 sha256 字段): 跳过比较, 返 actual_sha
        assert_eq!(verify_installer_sha256(bytes, "").unwrap(), real_sha);
        assert_eq!(verify_installer_sha256(bytes, "   ").unwrap(), real_sha);

        // Whitespace 容忍 + case-insensitive (xtask 输出小写, 但 self-host
        // 用户可能写大写 hex)
        assert_eq!(
            verify_installer_sha256(bytes, &format!("  {real_sha}  ")).unwrap(),
            real_sha
        );
        let upper = real_sha.to_ascii_uppercase();
        assert_eq!(verify_installer_sha256(bytes, &upper).unwrap(), real_sha);
    }

    /// sig_url_for 必须把 `.sig` 插到 URL path 段后缀 (而非裸字符串拼接),
    /// 这样 query string / fragment 在 self-host presigned URL / CDN
    /// cache-bust 场景下保留。codex-bot PR #197 PRRT_kwDOSRcxvM6CpmHk 修
    /// 之前 `format!("{url}.sig")` 让 `?token=abc` 变 `?token=abc.sig` 错路径。
    #[test]
    fn sig_url_preserves_query_string_and_fragment() {
        assert_eq!(
            sig_url_for("https://host/latest.json").unwrap(),
            "https://host/latest.json.sig"
        );
        assert_eq!(
            sig_url_for("https://host/latest.json?token=abc").unwrap(),
            "https://host/latest.json.sig?token=abc"
        );
        assert_eq!(
            sig_url_for("https://host/path/to/installer.dmg?ver=1&x=2").unwrap(),
            "https://host/path/to/installer.dmg.sig?ver=1&x=2"
        );
        assert_eq!(
            sig_url_for("https://host/latest.json#frag").unwrap(),
            "https://host/latest.json.sig#frag"
        );
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
            "in-app install is not supported on platform: linux-x64"
        );

        assert_eq!(
            install_command_parts("/tmp/Codex-App-Transfer.pkg", "macos-arm64").unwrap(),
            vec!["open", "/tmp/Codex-App-Transfer.pkg"]
        );
        // follow-up #36: Windows 路径返单 installer arg。NSIS /D= 在
        // launch_update_installer Windows 路径走 CommandExt::raw_arg
        // 注入(不在 args vec 里),原因:std::process::Command 自动对
        // 含空格 arg 加引号,会让 NSIS ignore /D= 落回默认 InstallDir
        // (chatgpt-codex PR #205 P2 review 指出)。
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
            "wait-for-exit pid is invalid"
        );
    }

    /// follow-up #35 a: macOS translocation 前置检查在非 translocation
    /// 路径(cargo test runner exe 路径)必须返 Ok。non-macOS 平台一律
    /// silent Ok(target_os 不为 macos)。真正的 translocation 路径
    /// `/private/var/folders/.../AppTranslocation/` 行为靠 #[cfg] guard +
    /// 真机 verify (单测 mock current_exe 复杂度高,跳过)。
    #[test]
    fn macos_translocation_precheck_under_cargo_test_returns_ok() {
        // cargo test 跑在 target/.../deps 路径,不含 /AppTranslocation/
        // 所以无论 macOS 还是其他平台都应该 Ok。
        assert!(macos_translocation_precheck().is_ok());
    }

    /// follow-up #36: current_exe_parent_dir 在 cargo test 下能解析成功
    /// (返 cargo target/.../deps 目录)。
    #[test]
    fn current_exe_parent_dir_resolves_in_test_runner() {
        let parent = current_exe_parent_dir();
        assert!(
            parent.is_some(),
            "current_exe parent must resolve in test runner"
        );
        let p = parent.unwrap();
        // cargo test runner 路径必含 "target" 段
        assert!(
            p.contains("target") || p.contains("Target"),
            "expected target dir in path, got: {p}"
        );
    }

    /// fetch_latest_json + 验签 end-to-end:
    /// mock server serve `release/latest.json` 真签名 + `release/latest.json.sig`,
    /// 用 embedded 官方公钥 verify → check_update_impl 返回 v1.0.3 + platforms。
    /// 任何篡改 (URL host 改, latest.json byte 改) 都会 verify 失败。
    #[test]
    fn fetch_latest_json_verifies_real_signature_end_to_end() {
        let release_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("release");
        let json_path = release_dir.join("latest.json");
        let sig_path = release_dir.join("latest.json.sig");
        if !json_path.exists() || !sig_path.exists() {
            eprintln!(
                "skipping: {} / {} missing (run `cargo run -p xtask -- release-bundle` first)",
                json_path.display(),
                sig_path.display()
            );
            return;
        }
        let json_bytes = std::fs::read(&json_path).unwrap();
        let sig_text = std::fs::read_to_string(&sig_path).unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use axum::{routing::get, Router};
            use tokio::net::TcpListener;

            let json_bytes_clone = Arc::new(json_bytes);
            let sig_text_clone = Arc::new(sig_text);

            let app = Router::new()
                .route("/latest.json", {
                    let json_bytes = Arc::clone(&json_bytes_clone);
                    get(move || {
                        let json_bytes = Arc::clone(&json_bytes);
                        async move {
                            (
                                [(axum::http::header::CONTENT_TYPE, "application/json")],
                                json_bytes.as_ref().clone(),
                            )
                        }
                    })
                })
                .route("/latest.json.sig", {
                    let sig_text = Arc::clone(&sig_text_clone);
                    get(move || {
                        let sig_text = Arc::clone(&sig_text);
                        async move { sig_text.as_ref().clone() }
                    })
                });
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();

            // Happy path: 真签名 + 真 latest.json → check_update_impl 返结果。
            // 平台用 linux-x86_64 (1.0.3 真 fixture 含此键)。
            let result = check_update_impl(
                &client,
                &format!("http://{addr}/latest.json"),
                "0.9.0",
                "linux-x86_64",
            )
            .await
            .unwrap();
            assert_eq!(result["success"], json!(true));
            assert_eq!(result["updateAvailable"], json!(true));
            assert_eq!(result["latestVersion"], json!("1.0.3"));
            assert_eq!(result["platform"], json!("linux-x86_64"));
            assert!(
                result["assets"][0]["sha256"].is_string(),
                "real latest.json must carry asset sha256"
            );

            server.abort();
        });
    }

    /// 验签失败路径: mock server serve latest.json 但 sig URL 返回随机 base64
    /// → fetch_latest_json 必须 Err 含 "signature invalid",不能 fallback 用未签数据。
    #[test]
    fn fetch_latest_json_rejects_invalid_signature() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use axum::{routing::get, Router};
            use tokio::net::TcpListener;

            let app = Router::new()
                .route(
                    "/latest.json",
                    get(|| async {
                        Json(json!({
                            "version": "9.9.9",
                            "platforms": {"macos-arm64": {"assets": []}}
                        }))
                    }),
                )
                .route(
                    "/latest.json.sig",
                    get(|| async {
                        // 合法 base64 但 RSA verify 必失败(随机 384 bytes 非签名)
                        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
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
            let err = check_update_impl(
                &client,
                &format!("http://{addr}/latest.json"),
                "0.0.1",
                "macos-arm64",
            )
            .await
            .unwrap_err();
            server.abort();
            assert!(
                err.contains("signature"),
                "verify err must mention signature, got: {err}"
            );
        });
    }

    /// 验签失败路径: sig URL 404 → fetch_latest_json 必须 Err
    /// "signature unavailable" (不接受未签 URL — followup #34 决策)。
    #[test]
    fn fetch_latest_json_rejects_missing_signature() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use axum::{routing::get, Router};
            use tokio::net::TcpListener;

            // 故意不 register .sig 路由 — 404
            let app = Router::new().route(
                "/latest.json",
                get(|| async {
                    Json(json!({
                        "version": "1.0.0",
                        "platforms": {"macos-arm64": {"assets": []}}
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
            let err = check_update_impl(
                &client,
                &format!("http://{addr}/latest.json"),
                "0.0.1",
                "macos-arm64",
            )
            .await
            .unwrap_err();
            server.abort();
            assert!(
                err.contains("signature unavailable"),
                "missing sig URL must produce 'signature unavailable', got: {err}"
            );
        });
    }

    /// download_asset_impl unsupported-platform fast-fail 保留(纯 logic,无 verify 依赖)。
    #[test]
    fn download_asset_unsupported_platform_rejected() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();
            let asset = json!({
                "name": "Codex-App-Transfer.AppImage",
                "url": "https://example.com/Codex-App-Transfer.AppImage"
            });
            let target_dir = std::env::temp_dir().join(format!(
                "cas-update-unsupported-{}-{}",
                std::process::id(),
                random_hex(6)
            ));
            let _ = fs::remove_dir_all(&target_dir);
            let err = download_asset_impl(&client, &asset, Some(&target_dir), "linux-x64")
                .await
                .unwrap_err();
            assert_eq!(
                err,
                "in-app install is not supported on platform: linux-x64"
            );
            let _ = fs::remove_dir_all(&target_dir);
        });
    }
}
