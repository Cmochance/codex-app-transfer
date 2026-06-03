//! chrome-headless-shell 按需下载 (Chrome-for-Testing)
//!
//! 系统无 Chrome 时 ([`super::detect`] 未命中), 从 Chrome-for-Testing 拉
//! chrome-headless-shell (~86MB) 到 app data, 解压 + 复用。不打包进安装包 (体积)。
//!
//! - **版本 pin**: 写死 [`PINNED_VERSION`] 保证可复现; CfT 持久保留旧版本, pin 安全。
//! - **完整性自检**: CfT 不提供官方 hash, 故落地后 spawn `--version` 真跑一次, 验证
//!   二进制可执行 (堵网络截断/损坏 zip)。**自检不过就不写 marker → 下次自动重下**。
//! - **原子上线**: 解压到 staging 临时目录 → 自检 → `rename` 进正式 version 目录 → 写
//!   marker。半完成状态对其他读者不可见, marker (内容=版本号) 是最后一步。
//! - **复用**: marker 内容 == 当前 pin 版本且二进制在 → 直接返回, 不重下。
//! - **并发守卫**: 同进程并发首抓走 [`DOWNLOAD_LOCK`] 串行化, 避免双下载 + 交错写坏
//!   同一份。**跨进程并发不在 PoC 范围** (需文件锁), 同进程足够覆盖当前用法。

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::headless::HeadlessError;

/// pin 死的 chrome-headless-shell 版本 (Chrome-for-Testing Stable, 2026-06-03 实测)。
///
/// 升级: 改这里 + 重新真机验收。CfT (`chrome-for-testing-public`) 持久保留所有
/// known-good 版本, pin 不会因渠道更新而失效。
pub const PINNED_VERSION: &str = "149.0.7827.54";

const CFT_BASE: &str = "https://storage.googleapis.com/chrome-for-testing-public";

/// 串行化下载临界区: 防同进程并发首抓互相写坏同一份二进制 (code-review 实证)。
static DOWNLOAD_LOCK: Mutex<()> = Mutex::const_new(());

/// 当前平台的 Chrome-for-Testing slug。不支持的平台返回 `None`。
pub fn platform_slug() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "mac-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("windows", "x86_64") => "win64",
        ("windows", "x86") => "win32",
        ("linux", "x86_64") => "linux64",
        _ => return None,
    })
}

/// chrome-headless-shell 下载 URL。
///
/// 模式 (2026-06 实测两源一致):
/// `{base}/{version}/{slug}/chrome-headless-shell-{slug}.zip`
fn download_url(version: &str, slug: &str) -> String {
    format!("{CFT_BASE}/{version}/{slug}/chrome-headless-shell-{slug}.zip")
}

/// app data 根: `~/.codex-app-transfer/browsers/chrome-headless-shell`
fn browsers_root() -> Result<PathBuf, HeadlessError> {
    let home =
        dirs::home_dir().ok_or_else(|| HeadlessError::Download("无法定位 home 目录".into()))?;
    Ok(home
        .join(".codex-app-transfer")
        .join("browsers")
        .join("chrome-headless-shell"))
}

/// 解压后二进制相对 (version 目录) 的路径:
/// `chrome-headless-shell-<slug>/chrome-headless-shell[.exe]`
fn binary_rel_path(slug: &str) -> PathBuf {
    let exe = if cfg!(windows) {
        "chrome-headless-shell.exe"
    } else {
        "chrome-headless-shell"
    };
    PathBuf::from(format!("chrome-headless-shell-{slug}")).join(exe)
}

/// 复用判定: marker 文件内容 (trim 后) == 期望版本 才算命中 (不只看存在性,
/// 这样 bump `PINNED_VERSION` 时旧目录不会被误复用)。
fn marker_matches(marker: &Path, version: &str) -> bool {
    std::fs::read_to_string(marker)
        .map(|c| c.trim() == version)
        .unwrap_or(false)
}

/// 确保 chrome-headless-shell 就绪: 已下载且版本匹配 → 返回路径; 否则下载+解压+自检。
/// 返回可执行二进制的绝对路径。
pub async fn ensure_chrome_headless_shell() -> Result<PathBuf, HeadlessError> {
    let slug = platform_slug().ok_or_else(|| {
        HeadlessError::Download(format!(
            "不支持的平台 {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    })?;
    let version = PINNED_VERSION;
    let root = browsers_root()?;
    let version_dir = root.join(version);
    let bin = version_dir.join(binary_rel_path(slug));
    let marker = version_dir.join(".complete");

    // 快速路径 (无锁): 已就绪且 marker 版本匹配 → 直接返回。
    if marker_matches(&marker, version) && bin.is_file() {
        return Ok(bin);
    }

    // 串行化下载: 防并发首抓双下载 + 交错写坏同一二进制 (跨进程不保, 见模块注释)。
    let _guard = DOWNLOAD_LOCK.lock().await;
    // 双检 (等锁期间别的任务可能已下完)。
    if marker_matches(&marker, version) && bin.is_file() {
        return Ok(bin);
    }

    std::fs::create_dir_all(&root)
        .map_err(|e| HeadlessError::Download(format!("建 browsers 目录失败: {e}")))?;

    // 解压到 staging 临时目录: 自检通过后才 rename 上线, 半完成对读者不可见。
    let staging = root.join(format!(".staging-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging); // 清上次残留
    std::fs::create_dir_all(&staging)
        .map_err(|e| HeadlessError::Download(format!("建 staging 目录失败: {e}")))?;

    let result = download_extract_verify(version, slug, &staging).await;
    let staged_bin = match result {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging); // 失败清 staging, 不留半成品
            return Err(e);
        }
    };

    // 原子上线: 删旧 version 目录 → rename staging → 写 marker (最后一步)。
    let _ = std::fs::remove_dir_all(&version_dir);
    std::fs::rename(&staging, &version_dir).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        HeadlessError::Download(format!("rename staging→version 失败: {e}"))
    })?;
    write_marker_atomic(&marker, version)?;

    // staged_bin 在 staging 下, rename 后真实路径在 version_dir 下。
    debug_assert!(staged_bin.ends_with(binary_rel_path(slug)));
    Ok(bin)
}

/// 下载 zip → 解压到 `staging` → set exec + 清 quarantine → spawn `--version` 自检。
/// 返回 staging 下的二进制路径 (自检已过)。任一步失败上抛 (调用方清 staging)。
async fn download_extract_verify(
    version: &str,
    slug: &str,
    staging: &Path,
) -> Result<PathBuf, HeadlessError> {
    let url = download_url(version, slug);
    let zip_bytes = download_zip(&url).await?;
    extract_zip(zip_bytes, staging).await?;

    let staged_bin = staging.join(binary_rel_path(slug));
    if !staged_bin.is_file() {
        return Err(HeadlessError::Download(format!(
            "解压后未找到二进制: {}",
            staged_bin.display()
        )));
    }
    set_executable(&staged_bin)?;
    clear_quarantine(&staged_bin);
    verify_binary(&staged_bin).await?;
    Ok(staged_bin)
}

/// 二进制完整性自检: 真 spawn `--version`。CfT 无官方 hash, 这是验证 "可执行且非
/// 截断/损坏" 的最可靠信号。失败 → 上抛 → 不写 marker → 下次 `ensure_*` 自动重下。
async fn verify_binary(bin: &Path) -> Result<(), HeadlessError> {
    let out = tokio::process::Command::new(bin)
        .arg("--version")
        .output()
        .await
        .map_err(|e| {
            HeadlessError::Download(format!("二进制自检 spawn 失败 (可能损坏/截断): {e}"))
        })?;
    if !out.status.success() {
        return Err(HeadlessError::Download(format!(
            "二进制自检 --version 非 0 退出 (可能损坏): status={}",
            out.status
        )));
    }
    Ok(())
}

/// 原子写 marker (临时文件 + rename), 内容为版本号。
fn write_marker_atomic(marker: &Path, version: &str) -> Result<(), HeadlessError> {
    let tmp = marker.with_extension("complete.tmp");
    std::fs::write(&tmp, version)
        .map_err(|e| HeadlessError::Download(format!("写 marker 临时文件失败: {e}")))?;
    std::fs::rename(&tmp, marker)
        .map_err(|e| HeadlessError::Download(format!("rename marker 失败: {e}")))
}

async fn download_zip(url: &str) -> Result<Vec<u8>, HeadlessError> {
    // connect 超时 + read 超时(检测"连上后卡住不发数据"): 防下载永久 pending 把前端
    // _webFetchSwitching 永久锁死(chatgpt review)。read_timeout 是"两次读之间的空闲
    // 超时", 不限制正常慢速下载的总时长(86MB 在慢网下可能要几分钟)。
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| HeadlessError::Download(format!("建下载 client 失败: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| HeadlessError::Download(format!("下载请求失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(HeadlessError::Download(format!(
            "下载 HTTP {}: {url}",
            resp.status()
        )));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| HeadlessError::Download(format!("读 body 失败: {e}")))
}

/// 解压 zip 到 `dest` (同步 zip API → spawn_blocking)。用 `enclosed_name` 防路径穿越。
async fn extract_zip(zip_bytes: Vec<u8>, dest: &Path) -> Result<(), HeadlessError> {
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let reader = std::io::Cursor::new(zip_bytes);
        let mut archive = zip::ZipArchive::new(reader)
            .map_err(|e| HeadlessError::Download(format!("打开 zip 失败: {e}")))?;
        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .map_err(|e| HeadlessError::Download(format!("读 zip entry 失败: {e}")))?;
            // enclosed_name: 拒绝 `../` 等穿越路径; 异常 entry 跳过。
            let Some(rel) = file.enclosed_name() else {
                continue;
            };
            // 目录 entry 直接跳过: 真正承载文件的目录由下面文件分支的 parent 创建负责
            // (zip 不保证目录 entry 在其下文件之前出现, 单独建是冗余的)。
            if file.is_dir() {
                continue;
            }
            let out = dest.join(rel);
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| HeadlessError::Download(format!("建目录失败: {e}")))?;
            }
            let mut outfile = std::fs::File::create(&out)
                .map_err(|e| HeadlessError::Download(format!("建文件失败: {e}")))?;
            std::io::copy(&mut file, &mut outfile)
                .map_err(|e| HeadlessError::Download(format!("写文件失败: {e}")))?;
        }
        Ok::<(), HeadlessError>(())
    })
    .await
    .map_err(|e| HeadlessError::Download(format!("解压 task join 失败: {e}")))?
}

#[cfg(unix)]
fn set_executable(p: &Path) -> Result<(), HeadlessError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(p)
        .map_err(|e| HeadlessError::Download(format!("读权限失败: {e}")))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(p, perms)
        .map_err(|e| HeadlessError::Download(format!("设可执行权限失败: {e}")))
}

#[cfg(not(unix))]
fn set_executable(_p: &Path) -> Result<(), HeadlessError> {
    Ok(())
}

/// 清 macOS quarantine xattr (防御性)。
///
/// `com.apple.quarantine` 由 LaunchServices 给 "浏览器/通信 app 下载的文件" 打;
/// 程序化下载 (reqwest 落盘) 通常 **不** 带此 xattr, 故首次 spawn 预期不被 Gatekeeper
/// 拦 (2026-06-03 真机实测确认: 仅有无害的 `com.apple.provenance`, 无 quarantine)。
/// 这里防御性移除 (有就清, 没有忽略——`xattr -d` 对不存在的 attr 报错是预期常态)。
#[cfg(target_os = "macos")]
fn clear_quarantine(p: &Path) {
    let _ = std::process::Command::new("xattr")
        .args(["-d", "com.apple.quarantine"])
        .arg(p)
        .output();
}

#[cfg(not(target_os = "macos"))]
fn clear_quarantine(_p: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_matches_verified_pattern() {
        // 2026-06-03 实测确认的真实 URL 形态。
        assert_eq!(
            download_url("149.0.7827.54", "mac-arm64"),
            "https://storage.googleapis.com/chrome-for-testing-public/149.0.7827.54/mac-arm64/chrome-headless-shell-mac-arm64.zip"
        );
    }

    #[test]
    fn binary_rel_path_shape() {
        let p = binary_rel_path("mac-arm64");
        assert!(p.starts_with("chrome-headless-shell-mac-arm64"));
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("chrome-headless-shell"));
    }

    #[test]
    fn platform_slug_is_known_or_none() {
        // 当前 CI 平台应被识别 (linux64/mac-*/win64); 其余返回 None 而非 panic。
        if let Some(slug) = platform_slug() {
            assert!(["mac-arm64", "mac-x64", "win64", "win32", "linux64"].contains(&slug));
        }
    }

    #[test]
    fn marker_matches_only_on_exact_version() {
        let dir = std::env::temp_dir().join(format!("cas-marker-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = dir.join(".complete");
        assert!(!marker_matches(&m, "149.0.7827.54"), "不存在应为 false");
        std::fs::write(&m, "149.0.7827.54\n").unwrap();
        assert!(
            marker_matches(&m, "149.0.7827.54"),
            "内容匹配 (trim) 应为 true"
        );
        assert!(!marker_matches(&m, "150.0.0.0"), "版本不符应为 false");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
