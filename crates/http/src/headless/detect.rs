//! 系统已装 Chrome/Edge/Chromium 探测 (mac/win/linux)
//!
//! "命中即连" 第一优先: 用户系统已有 Chromium 系浏览器就直接用, 免下载。
//! 三端常见安装路径硬编码; linux 额外走 `PATH` 查找 (发行版二进制名各异)。
//! 返回第一个存在的二进制路径; 未命中返回 `None` (调用方应触发按需下载)。

use std::path::PathBuf;

/// 探测系统已装的 Chromium 系浏览器, 命中返回可执行路径。
///
/// 优先级: Google Chrome → Chromium → Microsoft Edge (都基于 Chromium, CDP 兼容)。
/// 未命中返回 `None`。
pub fn detect_system_chrome() -> Option<PathBuf> {
    for candidate in candidates() {
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // linux: 发行版二进制名/路径各异, 硬编码路径之外再走 PATH。
    #[cfg(target_os = "linux")]
    {
        for name in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ] {
            if let Some(p) = which_in_path(name) {
                return Some(p);
            }
        }
    }

    None
}

#[cfg(target_os = "macos")]
fn candidates() -> Vec<PathBuf> {
    vec![
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
        "/Applications/Chromium.app/Contents/MacOS/Chromium".into(),
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into(),
    ]
}

#[cfg(target_os = "windows")]
fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let bases_chrome = [
        std::env::var("ProgramFiles").ok(),
        std::env::var("ProgramFiles(x86)").ok(),
        std::env::var("LOCALAPPDATA").ok(),
    ];
    for base in bases_chrome.into_iter().flatten() {
        v.push(PathBuf::from(base).join(r"Google\Chrome\Application\chrome.exe"));
    }
    let bases_edge = [
        std::env::var("ProgramFiles(x86)").ok(),
        std::env::var("ProgramFiles").ok(),
    ];
    for base in bases_edge.into_iter().flatten() {
        v.push(PathBuf::from(base).join(r"Microsoft\Edge\Application\msedge.exe"));
    }
    v
}

#[cfg(target_os = "linux")]
fn candidates() -> Vec<PathBuf> {
    vec![
        "/usr/bin/google-chrome".into(),
        "/usr/bin/google-chrome-stable".into(),
        "/usr/bin/chromium".into(),
        "/usr/bin/chromium-browser".into(),
        "/usr/bin/microsoft-edge".into(),
        "/snap/bin/chromium".into(),
    ]
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn candidates() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let full = dir.join(name);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探测不应 panic; 命中时返回的必须是真实存在的文件 (CI 多半未装浏览器 → None)。
    #[test]
    fn detect_is_safe_and_returns_real_file_if_any() {
        match detect_system_chrome() {
            Some(p) => assert!(p.is_file(), "探测命中却不是文件: {}", p.display()),
            None => {} // 未装浏览器是合法结果
        }
    }
}
