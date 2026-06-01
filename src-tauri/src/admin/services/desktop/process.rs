use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const MACOS_APP_NAME: &str = "Codex";
// [MOC-100 B] macOS 进程树匹配 token:`pkill -x Codex` / `pgrep -x Codex` 只认主进程名,
// Electron helper(`Codex Helper (Renderer/GPU)`)+ Frameworks/bare-modifier-monitor 子进程
// 在主进程被 KILL 后会被孤儿化存活 → 存活检查误判"已死"放行 → `open -n` 又拉新的 → 实例
// 堆积(实测 3 次重启累积 27 个进程 → 启动挂死)。改用 `-f` 匹配整个 .app bundle 路径,
// 覆盖主进程 + 全部 helper(任何安装位置的 `Codex.app/Contents/` 都命中,不误伤
// `Codex App Transfer.app`——其路径不含 `Codex.app/Contents/`)。
const MACOS_APP_PROCESS_MATCH: &str = "Codex.app/Contents/";
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
        // [MOC-100 B→优化] 退出判定只看**主进程**(快 ~1-2s)。B 当初改 `-f` 等整个
        // 进程树(含 helper)reap 是为防 `open -n` 堆积;现在 E 去掉了 `-n`(单实例
        // `open -a`),helper 残留也不会堆出第二实例(自己会死)→ 不必再等全 helper,
        // 等主进程死即可(LaunchServices 按主进程判 app 是否在跑,主进程 reaped 后
        // `open -a` 就会启新实例,不撞 activate-已有 的 race)。KILL 阶段仍用 `-f`
        // 强杀整树兜底(见 quit_command)。实测把"点击→重启弹窗"从 4-8s 降到 ~1-2s。
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
        // [MOC-100 B] KILL 阶段杀整个 .app 进程树(主进程 + 孤儿 helper),
        // 否则 helper 残留 + `open -n` → 实例堆积。TERM 阶段(上面)保持 `-x Codex`
        // 优雅杀主进程,让 Electron 自己 reap helper;只有 graceful 没清干净才升级到这条 KILL-all。
        ("macos", true) => vec![
            "pkill".into(),
            "-KILL".into(),
            "-f".into(),
            MACOS_APP_PROCESS_MATCH.into(),
        ],
        // follow-up #33 P2-b:从 `taskkill /IM` 切到 PowerShell CIM 路径。
        //
        // taskkill 在 Codex Desktop 这种 MSIX packaged Store app 上经常报
        // access-denied(packaged app 进程隔离机制),失败时本项目 quit_codex_
        // app_with_retries 走 KILL 路径仍是 taskkill,**两层 fallback 都失败**
        // → Codex 永远关不掉 → "重启 Codex" 实际只 ActivateApplication
        // 把现有进程带到前台,config.toml 不重读。
        //
        // PowerShell `Get-CimInstance Win32_Process` 走 WMI 拿到 process ID
        // 后 `Stop-Process -Id` 优雅清理,绕过 MSIX 进程隔离的 taskkill 限制。
        // 借鉴 BigPizzaV3/CodexPlusPlus `codex_session_delete/launcher.py:
        // 434-451`(MIT)实证可用。`hide_console_window` (line 192-202) 已加
        // CREATE_NO_WINDOW flag 给 powershell,不弹 console。
        ("windows", false) => vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-Command".into(),
            "Get-CimInstance Win32_Process -Filter \"Name='Codex.exe' OR Name='codex.exe'\" | ForEach-Object { Stop-Process -Id $_.ProcessId -ErrorAction SilentlyContinue }".into(),
        ],
        ("windows", true) => vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-Command".into(),
            "Get-CimInstance Win32_Process -Filter \"Name='Codex.exe' OR Name='codex.exe'\" | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }".into(),
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
///
/// `extra_args`: 附加给 Codex Desktop 本身的参数(如 `--remote-debugging-port=9222`)。
/// macOS 通过 `open` 的 `--args` 传递;Linux 直接追加到命令;Windows Store
/// 应用暂不支持命令行参数(忽略)。
fn open_command(
    platform: &str,
    resolved_macos_app: Option<&str>,
    extra_args: &[String],
) -> Vec<String> {
    match platform {
        // [MOC-100 E] 去掉 `-n`(原来强制开新实例以绕过「刚杀完进程、launchd 还没
        // reap 完 → open -a 被当成 activate 不存在实例 → 啥也不发生」的 race)。但 `-n`
        // 会在旧实例没彻底死时**堆出第二个实例** → 撞 Electron 单实例锁 → 卡在启动
        // (图标跳)/ 多窗口(daemon 注进 A、用户看 B 卡加载)。现在 quit_codex_app_with_retries
        // 已用 `pgrep -f Codex.app/Contents`(MOC-100 B)verify 旧实例含 helper 彻底死才
        // 走到这里 + POST_QUIT_LAUNCHD_GRACE,那条 race 已不存在 → 用 `open -a` 启**单**实例。
        "macos" => {
            let mut cmd = vec![
                "open".into(),
                "-a".into(),
                resolved_macos_app.unwrap_or(MACOS_APP_NAME).into(),
            ];
            if !extra_args.is_empty() {
                cmd.push("--args".into());
                cmd.extend(extra_args.iter().cloned());
            }
            cmd
        }
        "windows" => {
            // Windows Store 应用不支持通过 explorer.exe 传递命令行参数。
            // 如需调试端口，需用户手动修改快捷方式或使用其他启动方式。
            vec![
                "explorer.exe".into(),
                format!("shell:AppsFolder\\{WINDOWS_STORE_APP_ID}"),
            ]
        }
        _ => {
            let args_str = if extra_args.is_empty() {
                String::new()
            } else {
                format!(" {}", extra_args.join(" "))
            };
            vec![
                "sh".into(),
                "-c".into(),
                format!("{LINUX_BIN_NAME}{args_str} >/dev/null 2>&1 &"),
            ]
        }
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

pub fn is_codex_app_running(platform: &str) -> bool {
    // MOC-94:Windows 用原生 Toolhelp32 进程枚举替 spawn `tasklist`。本函数在
    // quit_codex_app_with_retries 轮询里被高频调用,每次 spawn tasklist 在 Windows
    // 上 ~50–200ms;原生枚举是 μs 级、无进程 spawn。快照失败(None)才 fallback
    // 到下面的 tasklist 命令路径(保留兜底,避免误判成"未运行"而跳过 quit)。
    #[cfg(target_os = "windows")]
    if platform == "windows" {
        if let Some(running) = crate::windows_msix::is_codex_running() {
            return running;
        }
    }
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
    // MOC-95:Windows 优雅退出(非 force / TERM 阶段)优先用原生 PostMessage(WM_CLOSE),
    // 替 PowerShell `Get-CimInstance Win32_Process | Stop-Process`(WMI 冷启动 ~1s,
    // MOC-93 实测重启路径大头)。两者同为 WM_CLOSE / CloseMainWindow 机制,native 省掉
    // PowerShell + WMI 开销。找到并投递了 ≥1 个 Codex 窗口即返回;0 个窗口(罕见:Codex
    // 无可见顶层窗口 / 快照失败)才 fall through 到下面 PowerShell graceful 兜底。force
    // (KILL 阶段)保持 PowerShell `Stop-Process -Force` 不动(原生 TerminateProcess 在
    // MSIX 上 access-denied,见 quit_command 注释)。
    #[cfg(target_os = "windows")]
    if platform == "windows" && !force && crate::windows_msix::graceful_close_codex() > 0 {
        return;
    }
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
            // [MOC-100 优化] 主进程已优雅退出(is_codex_app_running 只看主进程,~1-2s,弹窗秒出)。
            // 但 Electron helper 还在异步收尾 —— 不等它们 reap 就 open -a,残留 helper 会跟新
            // Codex 抢资源 → 新实例 DevToolsActivePort + 页面 load 变慢 → 注入延后(实测进程数
            // 涨到 19、launch→port 从 ~0.5s 涨到 ~2s)。这里补一发 KILL-all(`pkill -KILL -f`,
            // 一次性 ~50ms,不轮询等待)把残留 helper 立即 reap → 下次启动干净、注入快,且不拖慢弹窗。
            run_quit_command(platform, true);
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

/// 把 autoWakeCodexPet 设置双向同步到 Codex 全局状态文件的
/// `electron-avatar-overlay-open` 字段。enabled=true 写 true(自动开 pet),
/// enabled=false 写 false(显式关 pet 覆盖之前残留的 true)。
///
/// MOC-34: 旧实现只在 enabled=true 时写,enabled=false 时 early return,导致
/// 用户之前开过 pet(或 Codex Desktop 里手动开过)后,状态文件里残留的 true
/// 在设置关掉后仍生效,Codex 启动时还是会自动开 pet。
///
/// 失败路径(state 文件不存在 / 读失败 / 解析失败 / 非 object / 写失败)都不
/// 主动创建文件,但会 `tracing::warn!` 记录,方便复现 MOC-34 类报告 — 因为
/// enabled=false 时的写入承载着用户的关闭意图,静默丢弃会让用户怀疑开关坏了。
fn sync_codex_pet_state() {
    let cfg = match crate::admin::registry_io::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "[Pet] 读 registry 失败,跳过同步");
            return;
        }
    };
    // 默认 true 跟 settings.rs:61 / frontend app.js 的 `!== false` 默认对齐 —
    // 首启 / setting key 缺失时倾向自动开 pet。
    let enabled = cfg
        .get("settings")
        .and_then(|s| s.get("autoWakeCodexPet"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let Some(home) = codex_app_transfer_registry::paths::resolve_home() else {
        tracing::warn!("[Pet] 无法解析 home 目录,跳过同步");
        return;
    };
    let path = home.join(".codex").join(".codex-global-state.json");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "[Pet] 读 state 文件失败,跳过同步");
            return;
        }
    };
    let mut state: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "[Pet] state JSON 解析失败,跳过同步");
            return;
        }
    };
    let Some(obj) = state.as_object_mut() else {
        tracing::warn!(path = %path.display(), "[Pet] state JSON 顶层非 object,跳过同步");
        return;
    };
    obj.insert(
        "electron-avatar-overlay-open".to_string(),
        Value::Bool(enabled),
    );
    // to_string_pretty 对合法 Value::Object 几乎不会失败,但失败时**不能** fallback
    // 空字符串(会把 state 文件截成空 corrupt 旧值)。改成 match 显式跳过。
    let serialized = match serde_json::to_string_pretty(&state) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "[Pet] state 序列化失败,跳过写回");
            return;
        }
    };
    if let Err(e) = fs::write(&path, serialized) {
        tracing::warn!(path = %path.display(), error = %e, "[Pet] 写 state 失败,关闭意图可能未生效");
    }
}

/// 探测一个可用的 CDP debug port(非 macOS):**优先 9222**(跟 Chrome 一致),
/// 占用时 fallback OS 分配的随机空闲端口。
///
/// **macOS 不走此路径**(#264):改用 `--remote-debugging-port=0` + 异步 poll
/// `DevToolsActivePort` 文件,消除 try_bind 预检 vs Codex 真实 bind 的 race。
/// 见 `should_attach_debug_port()` 的 `#[cfg(target_os = "macos")]` 分支。
///
/// 借鉴 `BigPizzaV3/CodexPlusPlus` `launcher.py:267-281`(MIT)端口冲突探测
/// 思路。Rust 实现用 `std::net::TcpListener::bind` 尝试占位,**立刻 drop**;
/// 完全失败时 fallback 到 [`DEFAULT_CDP_PORT`](crate::codex_plugin_unlocker::DEFAULT_CDP_PORT)。
#[cfg(not(target_os = "macos"))]
pub(crate) fn detect_free_cdp_port() -> u16 {
    detect_free_cdp_port_using(|port| {
        std::net::TcpListener::bind(("127.0.0.1", port))
            .ok()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
    })
}

/// 纯函数版本 — 注入端口探测器给单测调用,避免在测试中跟真实 OS 端口耦合
/// (CI 上 9222 可能被某些 sidecar 占用导致测试 flaky)。模式跟
/// `registry/src/paths.rs::resolve_home_from` 一致。
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn detect_free_cdp_port_using<F>(try_bind: F) -> u16
where
    F: Fn(u16) -> Option<u16>,
{
    use crate::codex_plugin_unlocker::DEFAULT_CDP_PORT;
    if try_bind(DEFAULT_CDP_PORT) == Some(DEFAULT_CDP_PORT) {
        return DEFAULT_CDP_PORT;
    }
    try_bind(0).unwrap_or(DEFAULT_CDP_PORT)
}

/// 读取设置判断是否应附加调试端口参数。
///
/// 默认 true:setting key 缺失或 registry 读失败时,仍附加 debug port,以便
/// 新装/初始化场景下 Plugins 解锁开箱即用。用户显式关闭(=false)时才不附加。
/// 跟 main.rs setup hook 中的 auto-start 默认值保持一致。
///
/// **#264 改用 Chromium 随机端口**(从 codex-theme launcher.js 借的模式,user
/// 本地手搓不需致谢):
/// - `--remote-debugging-port=0` 让 Chromium 自己 atomic 选空闲端口,**消除**
///   Rust 端 try_bind 预检 + Codex 真实 bind 之间的 race window
/// - 启动后另起一个 task poll `~/Library/Application Support/Codex/DevToolsActivePort`
///   文件(Chromium 把真实端口写第一行),拿到端口写进 `CDP_PORT` atomic
/// - daemon 通过 `current_cdp_url()` 看到最新端口,无感切换
///
/// 旧 [`detect_free_cdp_port`] try_bind 预检路径仍保留(单测覆盖 + 跨平台
/// fallback:Windows / Linux 没有 DevToolsActivePort 路径,继续走预检)。
fn should_attach_debug_port() -> Vec<String> {
    // **任一为 true 都带 CDP 调试端口**(#264):plugin_unlock 跟 theme 是两个
    // 独立 toggle,user 可能只开 theme 不开 plugin_unlock。CDP 端口缺失会让
    // [`auto_apply_theme_on_startup`] 跑空,所以两者任一开启都要带 port。
    //
    // [MOC-104] plugin_unlock 这一侧改为「CDP daemon 实际会跑」才需要端口。daemon 在
    // ① 活动是真实 chatgpt(注入必需、无高延迟)或 ② apikey + 用户显式强制开启 时跑;
    // apikey + 未强制开启 时不跑 → 不必为它带调试端口(theme 仍可独立要求端口)。
    let cfg = crate::admin::registry_io::load().ok();
    let force_cdp = cfg
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get("autoUnlockCodexPlugins"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // [devin review] relay 真账号活动**不启 CDP daemon**(Codex 据 auth_mode==chatgpt
    // 原生显示 plugins),故不为它带调试端口;只有「强制开启」(force_cdp,走旧 CDP 伪造
    // 注入 daemon)才需要端口。theme 的端口需求独立判(下方 theme_enabled)。
    let plugin_unlock_needs_port = force_cdp;
    let theme_enabled = cfg
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get("codexUiThemeEnabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !plugin_unlock_needs_port && !theme_enabled {
        return vec![];
    }

    // macOS:用 port=0 + 异步 poll DevToolsActivePort(无 race)
    #[cfg(target_os = "macos")]
    {
        // 启动前清掉 stale DevToolsActivePort(否则可能读到上次启动的旧端口)
        let _ = std::fs::remove_file(devtools_active_port_path());
        // 预先把 CDP_PORT atomic 设为 0(sentinel:还没拿到真实端口),daemon
        // 检测到 0 应该暂时等待。
        crate::codex_plugin_unlocker::CDP_PORT.store(0, std::sync::atomic::Ordering::Relaxed);
        // 异步起一个 task poll DevToolsActivePort,拿到端口写 atomic + 自动
        // 注入主题(#264 user 反馈:开了 theme toggle 后从本应用启动 Codex 应
        // 直接应用已选主题,不需要 user 再去 Theme 页点一下)。
        tokio::spawn(async {
            if let Some(port) = wait_for_devtools_port(Duration::from_secs(15)).await {
                tracing::info!(
                    cdp_port = port,
                    "[PluginUnlock] DevToolsActivePort resolved to {port}"
                );
                crate::codex_plugin_unlocker::CDP_PORT
                    .store(port, std::sync::atomic::Ordering::Relaxed);
                auto_apply_theme_on_startup().await;
            } else {
                // **不**写 stale 9222 进 CDP_PORT — Codex 启动传 `--remote-debugging-port=0`,
                // Chromium 选了某个真实端口但 DevToolsActivePort 文件没出现(可能 sandbox /
                // 文件系统权限 / Codex 版本变更)。强行 fallback 9222 会让所有 CDP 调用
                // 连到一个 Codex 没监听的端口,user 手动 apply 也跟着失败,且看不到根因。
                // 保留 CDP_PORT=0(sentinel)→ [`codex_theme_injector::locate_main_window_ws`]
                // 检测到 0 时返"CDP 端口尚未就绪 — Codex Desktop 可能还在启动中,稍候重试"
                // 这种 actionable 错误,比 reqwest 报的"tcp connect error: Cannot assign
                // requested address"准确。同样 skip auto-apply(必然 ECONNREFUSED)。
                tracing::warn!(
                    "[PluginUnlock] DevToolsActivePort not produced within 15s; \
                     CDP_PORT left at 0 sentinel — manual theme apply will report \
                     'port not detected' instead of failing on a stale port. \
                     Possible causes: Codex sandbox / version change / FS permission."
                );
            }
        });
        return vec![
            "--remote-debugging-port=0".into(),
            "--remote-allow-origins=*".into(),
        ];
    }

    // 非 macOS(Windows / Linux):走旧 try_bind 预检路径。
    // DevToolsActivePort 路径在 Windows / Linux 的 Codex Desktop 上行为
    // 未实测,保持旧机制稳态;后续如有需求再单独 port=0 化。
    #[cfg(not(target_os = "macos"))]
    {
        let port = detect_free_cdp_port();
        crate::codex_plugin_unlocker::CDP_PORT.store(port, std::sync::atomic::Ordering::Relaxed);
        if port != crate::codex_plugin_unlocker::DEFAULT_CDP_PORT {
            tracing::info!(
                cdp_port = port,
                "[PluginUnlock] 9222 occupied, falling back to OS-assigned port"
            );
        }
        // MOC-73 / 反馈 fb-09ef05c2:Win 上点"重启 Codex"后主题不自动应用,要手动
        // 进 Theme 页点一下才生效 —— 原因是"重启后自动注入主题"过去只在 macOS 分支
        // (DevToolsActivePort resolved 后)调,Win/Linux 分支只 store 端口就 return。
        // 这里补上跨平台 auto-apply:Win/Linux 没有 DevToolsActivePort 这种"Codex 已
        // 就绪"信号(端口是启动前 try_bind 预检的),所以先等一个 grace 窗口让 Codex
        // 冷启动 + bind CDP,再调 auto_apply_theme_on_startup(其内部还有
        // 500/1000/1500ms 三次 retry)。失败只 warn 退场、退回原有"进 Theme 页"前端
        // 兜底,不变更现状(非破坏性),所以即使端口预检 race / MSIX 没透传也只是
        // 多一次无害尝试。仅在 theme toggle 开启时 spawn(只开 plugin_unlock 不需要)。
        //
        // ⚠️ 待 Windows 真机验证(MOC-73):① MSIX COM activation 是否真把
        //    `--remote-debugging-port` 透传给 Codex(explorer.exe fallback 路径会丢参);
        //    ② try_bind 预检端口与 Codex 实际监听端口是否一致。验证前开启此尝试是
        //    安全的,但"能否真正生效"取决于上述两点;若实测无效,需改走类似 macOS 的
        //    端口探测(Win 无 DevToolsActivePort,可能要别的就绪信号)。
        if theme_enabled {
            tokio::spawn(async {
                // Codex Desktop 冷启动较慢(尤其 Windows MSIX),给 ~2s grace 再尝试。
                tokio::time::sleep(Duration::from_millis(2000)).await;
                auto_apply_theme_on_startup().await;
            });
        }
        return vec![
            format!("--remote-debugging-port={port}"),
            "--remote-allow-origins=*".into(),
        ];
    }
}

/// `~/Library/Application Support/Codex/DevToolsActivePort` 路径。
/// Chromium 进程启动 `--remote-debugging-port=0` 后会把真实分配的端口写到这个
/// 文件第一行(第二行是 target ID / browser GUID,我们不用)。
#[cfg(target_os = "macos")]
fn devtools_active_port_path() -> std::path::PathBuf {
    let home = codex_app_transfer_registry::paths::resolve_home()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    home.join("Library/Application Support/Codex/DevToolsActivePort")
}

/// Poll `DevToolsActivePort` 文件首行拿端口号,最长等 `timeout`。
/// 文件第一行是端口数字(如 `54321`),第二行 GUID 不解析。
#[cfg(target_os = "macos")]
async fn wait_for_devtools_port(timeout: Duration) -> Option<u16> {
    let path = devtools_active_port_path();
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(first_line) = text.lines().next() {
                if let Ok(port) = first_line.trim().parse::<u16>() {
                    if port > 0 {
                        return Some(port);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

/// Codex 启动 + CDP 端口 ready 后,如果 user 开了 `codexUiThemeEnabled`
/// settings 就自动 apply 已选主题(#264)。Codex 主 page 可能还在 mount,
/// 用 3 次 retry(delay 500ms / 1000ms / 1500ms)cover 慢启动场景;3 次仍失败 warn 退场,
/// 不打扰 user(主题没 apply 不影响 Codex 正常用)。
///
/// **跨平台**(MOC-73):macOS 在 DevToolsActivePort resolved 后调;Windows / Linux
/// 没有该信号,由 [`should_attach_debug_port`] 的非 macOS 分支在固定 grace 窗口后调。
async fn auto_apply_theme_on_startup() {
    let theme_id = match read_theme_settings() {
        Some(id) => id,
        None => return,
    };
    for attempt in 0..3u32 {
        let delay_ms = 500 + (attempt as u64) * 500; // 500 / 1000 / 1500
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        match crate::codex_theme_injector::apply_theme(&theme_id).await {
            Ok(()) => {
                tracing::info!(
                    theme_id = %theme_id,
                    attempt,
                    "[Theme] auto-applied on Codex startup"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    theme_id = %theme_id,
                    attempt,
                    error = %e,
                    "[Theme] auto-apply attempt failed, retrying"
                );
            }
        }
    }
    tracing::warn!(
        theme_id = %theme_id,
        "[Theme] auto-apply gave up after 3 attempts (user can still apply manually)"
    );
}

/// 读 transfer settings,看 user 是否开了 theme + 选了哪个。返 `None` =
/// 未开 toggle / 没选主题 / theme_id 无效 → auto-apply 跳过。
///
/// **复用** [`crate::codex_theme_injector::read_settings`] 而不是再写一遍
/// parsing — 后者已经过滤了 `THEME_IDS` allowlist + custom-exists 检查,这里
/// 单独复写会 drift(typo'd / corrupted codexUiTheme 会绕过校验,产生 3 次
/// retry warning 无果)。
fn read_theme_settings() -> Option<String> {
    let cfg = crate::admin::registry_io::load().ok()?;
    let s = crate::codex_theme_injector::read_settings(cfg.get("settings")?);
    if s.enabled {
        s.theme_id
    } else {
        None
    }
}

fn open_codex_app(platform: &str) -> Result<(), String> {
    sync_codex_pet_state();

    // Windows MSIX activation: 见 `windows_msix.rs` module docs。失败时
    // fallthrough 到 explorer.exe shell:AppsFolder 老路径(args 丢失)。
    #[cfg(target_os = "windows")]
    if crate::windows_msix::try_launch_codex(&should_attach_debug_port()) {
        return Ok(());
    }

    let resolved = if platform == "macos" {
        resolve_macos_app_path()
    } else {
        None
    };
    let extra_args = should_attach_debug_port();
    let cmd = open_command(platform, resolved.as_deref(), &extra_args);
    let Some((program, args)) = cmd.split_first() else {
        return Err("open command is empty".to_owned());
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
        .map_err(|e| format!("cannot launch Codex App: {e}"))
}

pub fn launch_codex_app_restart(platform: &str) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_check_command_is_platform_specific() {
        // [MOC-100 B→优化] 退出判定只看主进程(快);KILL 阶段才用 -f 强杀整树
        assert_eq!(running_check_command("macos"), vec!["pgrep", "-x", "Codex"]);
        let windows = running_check_command("windows");
        assert_eq!(windows[0], "tasklist");
        assert!(windows.iter().any(|a| a == "IMAGENAME eq Codex.exe"));
        assert_eq!(running_check_command("linux"), vec!["pgrep", "-x", "codex"]);
    }

    #[test]
    fn detect_free_cdp_port_uses_9222_when_available() {
        let port = detect_free_cdp_port_using(|p| Some(p.max(1)));
        assert_eq!(port, crate::codex_plugin_unlocker::DEFAULT_CDP_PORT);
    }

    #[test]
    fn detect_free_cdp_port_falls_back_to_os_assigned_when_9222_taken() {
        let port = detect_free_cdp_port_using(|p| {
            if p == crate::codex_plugin_unlocker::DEFAULT_CDP_PORT {
                None
            } else {
                Some(54321)
            }
        });
        assert_eq!(port, 54321);
    }

    #[test]
    fn detect_free_cdp_port_falls_back_to_default_when_everything_fails() {
        let port = detect_free_cdp_port_using(|_| None);
        assert_eq!(port, crate::codex_plugin_unlocker::DEFAULT_CDP_PORT);
    }

    #[test]
    fn quit_command_uses_term_then_kill() {
        assert_eq!(
            quit_command("macos", false),
            vec!["pkill", "-TERM", "-x", "Codex"]
        );
        // [MOC-100 B] KILL 阶段改杀整个 .app 进程树(reap helper)
        assert_eq!(
            quit_command("macos", true),
            vec!["pkill", "-KILL", "-f", "Codex.app/Contents/"]
        );

        let win_graceful = quit_command("windows", false);
        assert_eq!(win_graceful[0], "powershell");
        assert_eq!(win_graceful[1], "-NoProfile");
        assert_eq!(win_graceful[2], "-Command");
        assert!(win_graceful[3].contains("Get-CimInstance Win32_Process"));
        assert!(win_graceful[3].contains("Codex.exe"));
        assert!(win_graceful[3].contains("Stop-Process"));
        assert!(
            !win_graceful[3].contains("-Force"),
            "graceful 不应该有 -Force"
        );

        let win_force = quit_command("windows", true);
        assert_eq!(win_force[0], "powershell");
        assert!(win_force[3].contains("Stop-Process"));
        assert!(win_force[3].contains("-Force"), "force 必须有 -Force");

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
        // [MOC-100 E] 去掉 `-n`,改单实例 `open -a`
        assert_eq!(
            open_command("macos", Some("/Applications/Codex.app"), &[]),
            vec!["open", "-a", "/Applications/Codex.app"]
        );
        assert_eq!(
            open_command("macos", None, &[]),
            vec!["open", "-a", "Codex"]
        );
        assert_eq!(
            open_command("macos", None, &["--remote-debugging-port=9222".into()]),
            vec![
                "open",
                "-a",
                "Codex",
                "--args",
                "--remote-debugging-port=9222"
            ]
        );
        let windows = open_command("windows", None, &[]);
        assert_eq!(windows[0], "explorer.exe");
        assert!(windows[1].starts_with("shell:AppsFolder\\"));
        assert!(windows[1].contains("OpenAI.Codex"));
        let linux = open_command("linux", None, &[]);
        assert_eq!(linux[0], "sh");
        assert_eq!(linux[1], "-c");
        assert!(linux[2].contains("codex"));
    }
}
