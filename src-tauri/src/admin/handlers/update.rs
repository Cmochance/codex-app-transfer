//! `/api/update/*` —— 升级检查 + 安装包下载 + 平台判断.

use std::fs;
use std::io::{Read, Write};
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

pub(super) fn file_sha256(path: &FsPath) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("read installer failed: {e}"))?;
    let mut digest = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read installer failed: {e}"))?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
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
        return Ok(vec![path.to_owned()]);
    }
    if platform.starts_with("macos-") {
        return Ok(vec!["open".to_owned(), path.to_owned()]);
    }
    Err(format!(
        "in-app install is not supported on platform: {platform}"
    ))
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
    let command = install_command_parts(installer_path, platform)?;
    let Some((program, args)) = command.split_first() else {
        return Err("install command is empty".to_owned());
    };
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
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
/// 失败原因:
/// - 网络 / DNS 错
/// - HTTP 4xx/5xx (404 = 服务端漏签,严重错误,客户端必须硬拒)
/// - 响应非合法 UTF-8 文本
async fn fetch_signature_text(client: &reqwest::Client, sig_url: &str) -> Result<String, String> {
    let response = client
        .get(sig_url)
        .send()
        .await
        .map_err(|e| format!("signature request failed: {e}"))?;
    response
        .error_for_status_ref()
        .map_err(|e| format!("signature request failed: {e}"))?;
    response
        .text()
        .await
        .map_err(|e| format!("signature read failed: {e}"))
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
    let target = updates_dir.join(filename);
    let partial = target.with_file_name(format!(
        "{}.download",
        target
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("update")
    ));

    let download_result: Result<(), String> = async {
        let mut response = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("download installer failed: {e}"))?;
        response
            .error_for_status_ref()
            .map_err(|e| format!("download installer failed: {e}"))?;
        let mut file =
            fs::File::create(&partial).map_err(|e| format!("write installer failed: {e}"))?;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("download installer failed: {e}"))?
        {
            if !chunk.is_empty() {
                file.write_all(&chunk)
                    .map_err(|e| format!("write installer failed: {e}"))?;
            }
        }
        file.flush()
            .map_err(|e| format!("write installer failed: {e}"))?;
        Ok(())
    }
    .await;
    if let Err(e) = download_result {
        if let Err(rm_err) = fs::remove_file(&partial) {
            tracing::warn!("failed to remove partial after download err: {rm_err}");
        }
        return Err(e);
    }

    // RSA 验签 installer bytes — true trust gate, 必须先于 sha256 check:
    //   - sha256 是 attacker-controlled(它在 latest.json 里, latest.json 改 url
    //     的同时可同步改 sha256, sha256 mismatch 只是 backup signal)
    //   - RSA sig 由 CI 用私钥离线签, attacker 无私钥不能伪造
    // 失败删 partial + 拒绝继续。code-reviewer #196 IMPORTANT-1 修。
    let installer_sig = fetch_signature_text(client, &installer_sig_url)
        .await
        .map_err(|e| {
            if let Err(rm_err) = fs::remove_file(&partial) {
                tracing::warn!("failed to remove partial after sig fetch err: {rm_err}");
            }
            format!("installer signature unavailable: {e}")
        })?;
    let installer_bytes = fs::read(&partial).map_err(|e| {
        if let Err(rm_err) = fs::remove_file(&partial) {
            tracing::warn!("failed to remove partial after read err: {rm_err}");
        }
        format!("read installer for verify failed: {e}")
    })?;
    if let Err(e) = verify_signed_bytes(&installer_bytes, &installer_sig) {
        if let Err(rm_err) = fs::remove_file(&partial) {
            tracing::warn!("failed to remove partial after verify err: {rm_err}");
        }
        return Err(format!("installer signature invalid: {e}"));
    }

    // sha256 redundant integrity check — RSA verify 已过,这里只是 defense-in-depth
    // 跟 release_bundle.rs 写的 sha256 字段对账。expected_sha 缺字段时跳过 (latest.json
    // 旧 schema 兼容)。
    let actual_sha = file_sha256(&partial)?;
    let expected_sha = asset
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !expected_sha.is_empty() && actual_sha.to_ascii_lowercase() != expected_sha {
        if let Err(rm_err) = fs::remove_file(&partial) {
            tracing::warn!("failed to remove partial after sha mismatch: {rm_err}");
        }
        return Err("installer checksum mismatch, install cancelled".to_owned());
    }

    if target.exists() {
        fs::remove_file(&target).map_err(|e| format!("write installer failed: {e}"))?;
    }
    fs::rename(&partial, &target).map_err(|e| format!("write installer failed: {e}"))?;
    let size = fs::metadata(&target)
        .map_err(|e| format!("read installer failed: {e}"))?
        .len();
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
