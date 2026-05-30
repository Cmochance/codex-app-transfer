//! Windows MSIX Codex Desktop 启动 — `IApplicationActivationManager` COM 调用。
//!
//! ## 问题背景
//!
//! Codex Desktop 在 Windows 上是 Microsoft Store 分发的 MSIX packaged app。
//! 老的 `explorer.exe shell:AppsFolder\<AUMID>` 启动协议在 OS 层面**剥离
//! 所有命令行参数**(`tmurgent` / `advancedinstaller` / Microsoft 官方
//! docs 均一致),所以 `--remote-debugging-port=9222 --remote-allow-origins=*`
//! 静默丢失,Plugin Unlock daemon 永远连不上 CDP。
//!
//! ## 解决方案
//!
//! Windows Shell COM 接口 `IApplicationActivationManager::ActivateApplication`
//! 是**官方支持的**给 packaged app 传 args 的入口,接受 `(AUMID, arguments,
//! ACTIVATEOPTIONS, &out_process_id)` 四参数,`arguments` 透传成 PWSTR
//! 给 packaged app 的 `process.argv`。
//!
//! ## 借鉴
//!
//! 实现路径 1:1 借鉴 `BigPizzaV3/CodexPlusPlus`(MIT,2699 stars)的 Python
//! 实现 `codex_session_delete/launcher.py:283-451`(2026-05-17 同步)。同道
//! 项目实证可工作。本 Rust 实现用 `windows` crate 官方 binding 而非手搓
//! ctypes COM,稳定性更好。
//!
//! 见 [`docs/followup/33-windows-plugin-unlock-msix-store.md`](../../../docs/followup/33-windows-plugin-unlock-msix-store.md)。

#![cfg(target_os = "windows")]

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, TRUE, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::UI::Shell::{ApplicationActivationManager, IApplicationActivationManager};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};

/// 调用 `IApplicationActivationManager::ActivateApplication` 启动 packaged
/// Codex Desktop 并把 `args` 透传成单一 PWSTR 命令行字符串。
///
/// `aumid` 是 Windows Store 应用的 Application User Model ID,形如
/// `OpenAI.Codex_<publisher_id>!App`,可由 [`resolve_codex_aumid`] 自动解析。
///
/// `args` 是已经按 Windows cmdline 规则 quote 好的单一字符串(参考
/// [`escape_cmdline`])。多个 arg 必须先拼到一个 string,**不能**像 POSIX
/// 那样传 `&[String]` —— ActivateApplication 的 `arguments` 参数语义是单一
/// raw 命令行,内部 Win32 不会再帮你 quote / escape。
///
/// 借鉴 `BigPizzaV3/CodexPlusPlus` `launcher.py:347-395`(COM 调用) +
/// `launcher.py:411`(args 用 `subprocess.list2cmdline` 序列化)。
pub fn activate_packaged_app(aumid: &str, args: &str) -> Result<u32, String> {
    unsafe {
        // 1. CoInitialize 当前线程(STA — Win32 Shell COM 要求 apartment
        //    threaded,而非 multi-threaded)。如线程已 init 过(Tauri runtime
        //    某些 worker 可能先 init 了),hr 返 RPC_E_CHANGED_MODE,无害,继续。
        let init_hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let need_uninit = init_hr.is_ok();

        // 2. CoCreateInstance(ApplicationActivationManager, CLSCTX_LOCAL_SERVER)
        //    这个 CLSID 在 launcher.py:347 是 `45BA127D-10A8-46EA-8AB7-56EA9078943C`,
        //    windows crate 的 `ApplicationActivationManager` 常量是一回事。
        let manager: IApplicationActivationManager =
            match CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER) {
                Ok(m) => m,
                Err(e) => {
                    if need_uninit {
                        CoUninitialize();
                    }
                    return Err(format!("CoCreateInstance failed: {e}"));
                }
            };

        // 3. ActivateApplication(aumid, args, AO_NONE, &out_pid)
        //    `aumid` / `args` 都要转 PCWSTR(UTF-16 nul-terminated),用 HSTRING
        //    临时持有保证 PCWSTR 生命周期内 buffer 还在。
        let aumid_hstring = HSTRING::from(aumid);
        let args_hstring = HSTRING::from(args);

        let result = manager.ActivateApplication(
            PCWSTR(aumid_hstring.as_ptr()),
            PCWSTR(args_hstring.as_ptr()),
            windows::Win32::UI::Shell::AO_NONE,
        );

        if need_uninit {
            CoUninitialize();
        }

        match result {
            Ok(pid) => Ok(pid),
            Err(e) => Err(format!("ActivateApplication failed: {e}")),
        }
    }
}

/// AUMID 进程内缓存(MOC-94)。**只缓存成功值** —— 同一 Codex 安装内 AUMID 恒定,
/// 但首次解析失败(Codex 未装 / PowerShell 瞬时失败)不能缓存 None 永久毒化(否则
/// 后续即便 Codex 装好也永远走 explorer.exe fallback 丢 debug 参数),失败下次重试。
/// (沿用 MOC-91 `update.rs` "只 cache 成功 Client" 的教训。)
static CACHED_AUMID: OnceLock<String> = OnceLock::new();

/// 解析 Codex Desktop 的 AUMID,带进程内缓存(避免每次启动 spawn PowerShell
/// `Get-AppxPackage` ~150–400ms,MOC-94)。首次解析成功后缓存,后续直接返回;
/// 解析失败不缓存,下次再试。
pub fn resolve_codex_aumid() -> Option<String> {
    if let Some(cached) = CACHED_AUMID.get() {
        return Some(cached.clone());
    }
    let resolved = resolve_codex_aumid_uncached()?;
    // set 可能因并发竞争失败(另一线程已写),无所谓 —— 值相同。
    let _ = CACHED_AUMID.set(resolved.clone());
    Some(resolved)
}

/// 用原生 `CreateToolhelp32Snapshot` 进程枚举判断 Codex Desktop 是否在运行
/// (MOC-94,替代 spawn `tasklist` —— quit 轮询里高频调用,每次 spawn 进程
/// 在 Windows 上 ~50–200ms)。比较 `Codex.exe`(大小写不敏感)。
///
/// 返回:
/// - `Some(true)` / `Some(false)`:确切判定。
/// - `None`:快照创建失败,无法判定 → caller 应 fallback 到 tasklist。
pub fn is_codex_running() -> Option<bool> {
    enum_codex_pids().map(|pids| !pids.is_empty())
}

/// 原生枚举所有 Codex 进程 PID(MOC-94/95 共用 —— is_codex_running 判存在、
/// graceful_close_codex 据此匹配窗口)。返回 `None` = 快照创建失败(caller fallback)。
fn enum_codex_pids() -> Option<Vec<u32>> {
    // SAFETY:Toolhelp32 API 全程在 unsafe 块内按文档用法调用 —— snapshot 句柄
    // 创建成功后保证 CloseHandle;PROCESSENTRY32W 按 dwSize 初始化;遍历到 NULL
    // 终止符截断进程名。所有原始指针来自栈上 entry,无悬垂。
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut pids = Vec::new();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if name.eq_ignore_ascii_case(WINDOWS_PROCESS_NAME) {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        Some(pids)
    }
}

/// MOC-95:原生优雅关闭 Codex —— 给所有 Codex 进程的顶层窗口 `PostMessage(WM_CLOSE)`。
///
/// 跟 PowerShell `Stop-Process`(非 `-Force`)的 `CloseMainWindow` 是**同一机制**
/// (都向窗口投 WM_CLOSE),但省掉 PowerShell 进程启动 + `Get-CimInstance` WMI 冷
/// 初始化的 ~1s 开销(MOC-93 实测大头)。MSIX 允许向窗口投 WM_CLOSE(不像
/// taskkill/TerminateProcess 那样 access-denied)。
///
/// 返回 PostMessage 成功的窗口数;`0` = 没找到 Codex 窗口(快照失败 / 进程无可见
/// 顶层窗口 / 投递失败)→ caller 应 fallback 到 PowerShell graceful 兜底。
pub fn graceful_close_codex() -> usize {
    let pids = match enum_codex_pids() {
        Some(p) if !p.is_empty() => p,
        _ => return 0,
    };
    let mut ctx = CloseCtx {
        pids: &pids,
        posted: 0,
    };
    // SAFETY:EnumWindows 同步遍历顶层窗口,回调期间栈上的 ctx 始终有效;LPARAM
    // 透传 ctx 裸指针,回调内解引用即取回。PostMessageW 异步投递不阻塞。
    unsafe {
        let _ = EnumWindows(Some(enum_close_proc), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.posted
}

struct CloseCtx<'a> {
    pids: &'a [u32],
    posted: usize,
}

/// EnumWindows 回调:窗口属于 Codex 进程则投 WM_CLOSE。返回 TRUE 继续遍历。
unsafe extern "system" fn enum_close_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut CloseCtx);
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != 0
        && ctx.pids.contains(&pid)
        && PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok()
    {
        ctx.posted += 1;
    }
    TRUE
}

/// Codex Desktop 的 Windows 进程名(与 `process.rs::WINDOWS_PROCESS_NAME` 一致)。
const WINDOWS_PROCESS_NAME: &str = "Codex.exe";

/// 用 PowerShell `Get-AppxPackage -Name "OpenAI.Codex"` 反推 Codex Desktop
/// 的 AUMID。
///
/// AppxPackage InstallLocation 形如 `C:\Program Files\WindowsApps\
/// OpenAI.Codex_X.Y.Z.0_x64__<publisher_id>\`,从中提取 `<publisher_id>`
/// 再拼成 `OpenAI.Codex_<publisher_id>!App`(`!App` 是 Codex 的 entry point
/// alias,从 AppxManifest 来,绝大多数 Electron MSIX 用 `!App`)。
///
/// 1:1 借鉴 `BigPizzaV3/CodexPlusPlus` `codex_session_delete/launcher.py:298-304`
/// + `app_paths.py:30-49`。上游同样没硬编码 fallback —— 找不到包就 None,
/// 让 caller 走 explorer.exe 老路径或 last-resort Method 6(非 Store 直装 .exe);
/// 写死 publisher hash 作 fallback 会让 ActivateApplication 用错的 AUMID 报错
/// 比 None 更难诊断,且 explorer.exe fallback 已是 safety net。
fn resolve_codex_aumid_uncached() -> Option<String> {
    // PowerShell `Get-AppxPackage` 需要 `-NoProfile` 加速启动(否则会跑
    // 用户 PSProfile 几百 ms 起步)。
    // `CREATE_NO_WINDOW = 0x0800_0000`:防止 powershell 在前台 flash 一个
    // console 黑框(本项目 GUI app 无 stdio,console 弹出会被用户感知为
    // "终端窗口"打扰)。跟 `desktop.rs::hide_console_window` 同模式
    // (借鉴 codex-account-switch `src-tauri/win/runtime/process.rs::
    // hide_console_window`)。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-AppxPackage -Name 'OpenAI.Codex' | Select-Object -ExpandProperty PackageFamilyName",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let pfn = String::from_utf8(output.stdout).ok()?;
    let pfn = pfn.trim();
    if pfn.is_empty() {
        return None;
    }
    // PackageFamilyName 形如 `OpenAI.Codex_<publisher_id>`,直接拼 `!App`
    Some(format!("{pfn}!App"))
}

/// 完整的 "尝试用 ActivateApplication 启动 Codex MSIX" 流程封装。
///
/// 返 `true` = 成功(caller 应该立刻 return,不走 fallback);`false` =
/// 应 fall through 到 explorer.exe `shell:AppsFolder` 老路径(args 会丢失,
/// Plugin Unlock 在 fallback 下不工作,但 Codex 至少能启动)。
///
/// 失败原因 + 成功 PID 都记 tracing,caller 不用再 log。
pub fn try_launch_codex(extra_args: &[String]) -> bool {
    let Some(aumid) = resolve_codex_aumid() else {
        tracing::warn!(
            "MSIX package not found via Get-AppxPackage, falling back to explorer.exe shell:AppsFolder (无 debug port)"
        );
        return false;
    };
    let cmdline = list2cmdline(extra_args);
    tracing::info!(
        aumid = %aumid,
        cmdline = %cmdline,
        "launching Codex Desktop via IApplicationActivationManager"
    );
    match activate_packaged_app(&aumid, &cmdline) {
        Ok(pid) => {
            tracing::info!(pid, "Codex Desktop activated via COM");
            true
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ActivateApplication failed, falling back to explorer.exe shell:AppsFolder (无 debug port)"
            );
            false
        }
    }
}

/// 把 `Vec<String>` 按 Windows cmdline quoting 规则序列化成单一 PWSTR-ready
/// 字符串,等价于 Python `subprocess.list2cmdline`。
///
/// 规则(Microsoft `CommandLineToArgvW` 文档):
/// - 含空格 / tab / 引号 的 arg 必须用 `"..."` 包裹
/// - arg 内的 `"` 转义成 `\"`
/// - arg 内 `\` 仅在紧跟 `"` 时才需要 double:`\\"`
/// - 简单 args(无空格无引号)直接拼空格分隔
///
/// 借鉴 `BigPizzaV3/CodexPlusPlus` `launcher.py:411` 的
/// `subprocess.list2cmdline(build_codex_arguments(debug_port))` 路径。
pub fn list2cmdline(args: &[String]) -> String {
    args.iter()
        .map(|a| escape_cmdline(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn escape_cmdline(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\n')
    {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else if c == '"' {
            // 紧跟 `"` 的 backslash 全部 double + escape `"`
            for _ in 0..(backslashes * 2 + 1) {
                out.push('\\');
            }
            out.push('"');
            backslashes = 0;
        } else {
            for _ in 0..backslashes {
                out.push('\\');
            }
            backslashes = 0;
            out.push(c);
        }
    }
    // 末尾 backslash(在收尾 `"` 前)同样 double
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_cmdline_simple_arg_returns_as_is() {
        assert_eq!(
            escape_cmdline("--remote-debugging-port=9222"),
            "--remote-debugging-port=9222"
        );
        assert_eq!(escape_cmdline("noargs"), "noargs");
    }

    #[test]
    fn escape_cmdline_arg_with_space_gets_quoted() {
        assert_eq!(escape_cmdline("hello world"), "\"hello world\"");
    }

    #[test]
    fn escape_cmdline_arg_with_quote_gets_escaped() {
        assert_eq!(escape_cmdline("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn list2cmdline_joins_with_space() {
        let args = vec![
            "--remote-debugging-port=9222".to_owned(),
            "--remote-allow-origins=http://127.0.0.1:9222".to_owned(),
        ];
        assert_eq!(
            list2cmdline(&args),
            "--remote-debugging-port=9222 --remote-allow-origins=http://127.0.0.1:9222"
        );
    }
}
