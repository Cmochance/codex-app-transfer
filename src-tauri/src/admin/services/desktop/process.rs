use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

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
        // `-n`:即使 LaunchServices 缓存还以为 Codex 在运行,也强制启动一个新
        // 实例。我们刚 SIGTERM 杀过主进程,launchd 偶尔会在 reap 完成前误把
        // `open -a` 解读为 activate 已有实例 → 啥也不发生。`-n` 绕过这条。
        "macos" => {
            let mut cmd = vec![
                "open".into(),
                "-n".into(),
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

/// 如果设置中开启了 autoWakeCodexPet，在启动 Codex 前将其全局状态中的
/// electron-avatar-overlay-open 设为 true，使宠物自动唤醒。
fn maybe_wake_codex_pet() {
    let cfg = match crate::admin::registry_io::load() {
        Ok(c) => c,
        Err(_) => return,
    };
    let enabled = cfg
        .get("settings")
        .and_then(|s| s.get("autoWakeCodexPet"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if !enabled {
        return;
    }
    let home = match codex_app_transfer_registry::paths::resolve_home() {
        Some(h) => h,
        None => return,
    };
    let path = home.join(".codex").join(".codex-global-state.json");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut state: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return,
    };
    if let Some(obj) = state.as_object_mut() {
        obj.insert(
            "electron-avatar-overlay-open".to_string(),
            Value::Bool(true),
        );
    }
    let _ = fs::write(
        &path,
        serde_json::to_string_pretty(&state).unwrap_or_default(),
    );
}

/// 探测一个可用的 CDP debug port:**优先 9222**(默认值,跟 Chrome 一致),
/// 占用时 fallback OS 分配的随机空闲端口。
///
/// 借鉴 `BigPizzaV3/CodexPlusPlus` `launcher.py:267-281`(MIT)端口冲突探测
/// 思路。本 Rust 实现用 `std::net::TcpListener::bind` 尝试占位,**立刻 drop
/// 释放**(只确认"该端口此刻可绑"),Codex Desktop 的 bind 在 ms 级内接管。
/// 时间窗口里被第三方进程抢占的概率极低;即便抢了,Codex 启动会报端口占用
/// 由用户视觉感知,daemon 会继续 backoff重试不至于卡死。
///
/// 完全失败(连 port 0 都拿不到 — 系统资源枯竭)时 fallback 到
/// [`DEFAULT_CDP_PORT`](crate::codex_plugin_unlocker::DEFAULT_CDP_PORT)。
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
/// **#33 P2 端口冲突探测**(issue #226 Task 1):用 [`detect_free_cdp_port`]
/// 找空闲端口(优先 9222,占用 fallback OS 分配),把结果写入 `CDP_PORT`
/// atomic,plugin_unlock daemon 通过 `current_cdp_url()` 看到最新值。
fn should_attach_debug_port() -> Vec<String> {
    let auto_unlock = match crate::admin::registry_io::load() {
        Ok(cfg) => cfg
            .get("settings")
            .and_then(|s| s.get("autoUnlockCodexPlugins"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        Err(_) => true,
    };
    if auto_unlock {
        let port = detect_free_cdp_port();
        crate::codex_plugin_unlocker::CDP_PORT.store(port, std::sync::atomic::Ordering::Relaxed);
        if port != crate::codex_plugin_unlocker::DEFAULT_CDP_PORT {
            tracing::info!(
                cdp_port = port,
                "[PluginUnlock] 9222 occupied, falling back to OS-assigned port"
            );
        }
        // `--remote-allow-origins=*` 是 Chrome 111+ / Electron 同代起的硬性
        // 要求:不带它,CDP HTTP /json/list 仍工作,但 WebSocket upgrade 完成
        // 后会被远端 reset(我们 log 里见过 "Connection reset without closing
        // handshake")。galaxywk223/codex-plugin-unlocker (MIT) 同样加这个
        // flag,见其 `launcher.py:55-58`。
        vec![
            format!("--remote-debugging-port={port}"),
            "--remote-allow-origins=*".into(),
        ]
    } else {
        vec![]
    }
}

fn open_codex_app(platform: &str) -> Result<(), String> {
    maybe_wake_codex_pet();

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
        assert_eq!(
            quit_command("macos", true),
            vec!["pkill", "-KILL", "-x", "Codex"]
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
        assert_eq!(
            open_command("macos", Some("/Applications/Codex.app"), &[]),
            vec!["open", "-n", "-a", "/Applications/Codex.app"]
        );
        assert_eq!(
            open_command("macos", None, &[]),
            vec!["open", "-n", "-a", "Codex"]
        );
        assert_eq!(
            open_command("macos", None, &["--remote-debugging-port=9222".into()]),
            vec![
                "open",
                "-n",
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
