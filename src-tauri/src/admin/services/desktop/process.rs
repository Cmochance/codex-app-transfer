use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

/// [CAT-255] 串行化所有「关 Codex → 维护 → 重启」流程(`launch_codex_app_restart` +
/// `with_codex_closed`),避免两个流程并发:一个在写 Codex 独占的 state DB,另一个把 Codex
/// 重新拉起 → DB 在 Codex 运行时被写 = 整个设计要防的数据损坏。
static CODEX_MAINTENANCE_LOCK: Mutex<()> = Mutex::new(());

/// Codex 桌面 app 的 bundle id:新旧两代(`Codex.app` / 26.707+ `ChatGPT.app`)共用,
/// 是唯一跨改名稳定的身份标识。用途:① `open -b` 兜底(resolve_macos_app_path 全落空
/// 时按 id 启动,不会解析到消费级 ChatGPT 客户端 `com.openai.chat`);② PID 归属校验
/// (见 [`macos_pid_classes`]),防止按路径正则误杀同名消费级 app。
const MACOS_BUNDLE_ID: &str = "com.openai.codex";
// [Codex→ChatGPT 改名适配] Codex 桌面 app 自 26.707 起把 bundle 从 `Codex.app`(进程名/
// 可执行 `Codex`)改名成 `ChatGPT.app`(进程名/可执行 `ChatGPT`),bundle id
// `com.openai.codex` 与 user-data-dir `Codex` 不变。进程匹配不能再用 `-x <名>`:
// (1) `-x Codex` 匹配不到新进程名 `ChatGPT`;(2) 直接换 `-x ChatGPT` 会误杀同可执行名
// 的消费者 app `ChatGPT Classic.app`(bundle `com.openai.chat`)。改用 `-f` 按 .app
// bundle 路径正则**枚举候选**,再逐 PID 按 bundle id 归属校验(见 macos_pid_classes;
// 用户若装有仍叫 `ChatGPT.app` 的消费级客户端,路径正则无法区分,必须验 Info.plist),
// 同时覆盖新旧两种安装:
// - MAIN:只命中主进程(cmdline 含 `<App>.app/Contents/MacOS/`),供运行检测 + TERM 优雅退出
//   (让 Electron 自己 reap helper),保留原「只看主进程」的速度优化。
// - APP(全树):命中主进程 + 全部 helper(cmdline 含 `<App>.app/Contents/`),供 KILL 兜底 reap。
// 两者都**不误伤** `Codex App Transfer.app`(其路径是 `Codex App Transfer.app`,`Codex`
// 后接空格非 `.app`,正则 `Codex\.app` 不命中)与 `ChatGPT Classic.app`(同理 `ChatGPT` 后接空格)。
const MACOS_MAIN_PROCESS_MATCH: &str = r"(Codex|ChatGPT)\.app/Contents/MacOS/";
const MACOS_APP_PROCESS_MATCH: &str = r"(Codex|ChatGPT)\.app/Contents/";
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

/// macOS 进程归属分类。路径正则(MACOS_*_PROCESS_MATCH)只能筛**候选**,不能定身份:
/// 消费级 ChatGPT 客户端未更名的旧安装同样叫 `ChatGPT.app`,路径字符串无法区分。
/// 借鉴 codex-account-switch PR #51 的 pid-class 方案:逐 PID 读 bundle 的
/// CFBundleIdentifier 定归属,只对 [`MacosPidClass::Ours`] 发信号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacosPidClass {
    /// CFBundleIdentifier == `com.openai.codex`(唯一可发信号的类)
    Ours,
    /// 明确属于别的 bundle(如消费级 ChatGPT 客户端 `com.openai.chat`)
    Other,
    /// 归属无法确认(cmdline 无 .app 路径 / Info.plist 读取失败)。**计入"在跑"但
    /// 绝不发信号**:宁可让重启流程报"请手动关闭"交人工处理,不冒险杀身份不明进程。
    Unknown,
}

/// 从进程 cmdline 提取最外层 `.app` bundle 根路径(纯函数,可测)。取**第一个**
/// `.app/Contents/`:helper 进程路径形如 `ChatGPT.app/Contents/Frameworks/<X> Helper
/// (Renderer).app/Contents/MacOS/...`,第一个命中即外层宿主 bundle,归属以宿主为准。
fn extract_macos_bundle_root(cmdline: &str) -> Option<&str> {
    let idx = cmdline.find(".app/Contents/")?;
    Some(&cmdline[..idx + ".app".len()])
}

fn classify_macos_bundle_id(bundle_id: Option<&str>) -> MacosPidClass {
    match bundle_id {
        Some(id) if id == MACOS_BUNDLE_ID => MacosPidClass::Ours,
        Some(id) if !id.is_empty() => MacosPidClass::Other,
        _ => MacosPidClass::Unknown,
    }
}

/// `defaults read <bundle>/Contents/Info CFBundleIdentifier`(与 codex-account-switch
/// 同法,defaults 同时兼容 XML / binary plist)。失败(非 bundle / plist 损坏)返 None。
fn read_macos_bundle_id(bundle_root: &str) -> Option<String> {
    let out = Command::new("defaults")
        .arg("read")
        .arg(format!("{bundle_root}/Contents/Info"))
        .arg("CFBundleIdentifier")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// 枚举 + 归属:`pgrep -fl <pattern>`(输出每行 `PID cmdline`)拿候选,逐 PID 验
/// bundle id。pgrep 无匹配时 exit 1 + 空输出 → 空 Vec。
fn macos_pid_classes(pattern: &str) -> Vec<(u32, MacosPidClass)> {
    let Ok(out) = Command::new("pgrep")
        .arg("-fl")
        .arg(pattern)
        .stdin(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, cmdline) = line.split_once(' ')?;
            let pid: u32 = pid.trim().parse().ok()?;
            let class = match extract_macos_bundle_root(cmdline) {
                Some(root) => classify_macos_bundle_id(read_macos_bundle_id(root).as_deref()),
                None => MacosPidClass::Unknown,
            };
            Some((pid, class))
        })
        .collect()
}

/// 平台检测命令(可纯函数测试).返回 (program, args).第一个元素总是命令名。
fn running_check_command(platform: &str) -> Vec<String> {
    match platform {
        // [改名适配] macOS 不再有单条静态命令:候选枚举 + bundle id 归属校验见
        // [`macos_pid_classes`],调用方 [`is_codex_app_running`] 在进入本函数前分流。
        // 返回空 Vec = 调用方安全 no-op(防误用)。
        "macos" => Vec::new(),
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
        // [改名适配] macOS 不再返回静态 pkill(按路径正则盲杀会误伤未更名的消费级
        // ChatGPT.app):必须逐 PID 归属校验后 kill,见 [`run_quit_command`] 的 macos
        // 分支。返回空 Vec = 调用方安全 no-op(防误用)。
        ("macos", _) => Vec::new(),
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
/// [MOC-323] Chat 接入自定义模型:守卫补丁脚本(经 `NODE_OPTIONS=--require` 注入 app 主进程,
/// hook `isDesktopAuthAllowedUrl` 白名单;详见 `resources/chat_guard_patch.js`)。
const CHAT_GUARD_PATCH_JS: &str = include_str!("../../../../resources/chat_guard_patch.js");

/// [MOC-323] 启动 Codex app 时为「Chat 接入自定义模型」注入的环境变量(仅 macOS)。
/// 设置项 `chatCustomModelEnabled` **默认开**;把守卫补丁写到 `~/.codex-app-transfer/`,令
/// app 的 Chat 对话经 `CODEX_API_BASE_URL` 流进本地 proxy。**host 必须用 `localhost`**:守卫
/// 判 `new URL(base).host`,补丁加的是 `localhost:<port>`,base 用 `127.0.0.1` 则 host 不匹配。
fn chat_launch_env(platform: &str) -> Vec<(String, String)> {
    if platform != "macos" {
        return Vec::new();
    }
    let cfg = crate::admin::registry_io::load().ok();
    let enabled = cfg
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get("chatCustomModelEnabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true); // 默认开
    if !enabled {
        return Vec::new();
    }
    let port = cfg
        .as_ref()
        .map(crate::admin::handlers::proxy::read_proxy_port)
        .unwrap_or(18080);
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dir = PathBuf::from(home).join(".codex-app-transfer");
    let patch_path = dir.join("chat_guard_patch.js");
    if fs::create_dir_all(&dir).is_err() || fs::write(&patch_path, CHAT_GUARD_PATCH_JS).is_err() {
        return Vec::new();
    }
    let host = format!("localhost:{port}");
    vec![
        (
            "NODE_OPTIONS".into(),
            format!("--require {}", patch_path.to_string_lossy()),
        ),
        (
            "CODEX_API_BASE_URL".into(),
            format!("http://{host}/backend-api"),
        ),
        ("CAS_CHAT_GUARD_HOST".into(), host),
    ]
}

fn open_command(
    platform: &str,
    resolved_macos_app: Option<&str>,
    extra_args: &[String],
    extra_env: &[(String, String)],
) -> Vec<String> {
    match platform {
        // [MOC-100 E] 去掉 `-n`(原来强制开新实例以绕过「刚杀完进程、launchd 还没
        // reap 完 → open -a 被当成 activate 不存在实例 → 啥也不发生」的 race)。但 `-n`
        // 会在旧实例没彻底死时**堆出第二个实例** → 撞 Electron 单实例锁 → 卡在启动
        // (图标跳)/ 多窗口(daemon 注进 A、用户看 B 卡加载)。现在 quit_codex_app_with_retries
        // 已用 `pgrep -f Codex.app/Contents`(MOC-100 B)verify 旧实例含 helper 彻底死才
        // 走到这里 + POST_QUIT_LAUNCHD_GRACE,那条 race 已不存在 → 用 `open -a` 启**单**实例。
        "macos" => {
            let mut cmd = vec!["open".into()];
            // [MOC-323] `open --env K=V` 把 env 传给被启动的 GUI app(NODE_OPTIONS 守卫补丁 +
            // CODEX_API_BASE_URL 路由 Chat 进本地 proxy)。必须在 `-a`/`--args` 之前。
            for (k, v) in extra_env {
                cmd.push("--env".into());
                cmd.push(format!("{k}={v}"));
            }
            match resolved_macos_app {
                Some(path) => {
                    cmd.push("-a".into());
                    cmd.push(path.into());
                }
                // [改名适配] 路径解析落空(自定义安装位置)时按 bundle id 兜底:新旧
                // 两代 app 同 id,26.707-only 安装上旧名 `open -a Codex` 启不动;且
                // 按 id 永不解析到消费级 ChatGPT 客户端(`open -a ChatGPT` 按名则可能)。
                None => {
                    cmd.push("-b".into());
                    cmd.push(MACOS_BUNDLE_ID.into());
                }
            }
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
    // [改名适配] 26.707+ 是 `ChatGPT.app`(优先);`Codex.app` 保留兜底覆盖未更新的旧安装。
    let mut candidates = vec![
        PathBuf::from("/Applications/ChatGPT.app"),
        PathBuf::from("/Applications/Codex.app"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let apps = PathBuf::from(home).join("Applications");
        candidates.push(apps.join("ChatGPT.app"));
        candidates.push(apps.join("Codex.app"));
    }
    candidates
        .into_iter()
        // [改名适配·防误选] 只认**内嵌 codex CLI** 的 bundle:光看目录存在,用户装有
        // 未更名的消费级 ChatGPT 客户端(`com.openai.chat`,亦名 ChatGPT.app)时会把
        // 它当 Codex 宿主打开。`Contents/Resources/codex` 是宿主的稳定特征(26.707
        // 真机实测仍在);全部不合格则走 open_command 的 `-b` bundle id 兜底。
        .find(|p| p.join("Contents").join("Resources").join("codex").is_file())
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
    // [MOC-100 B→优化][改名适配] macOS 运行判定只看**主进程**(MAIN 模式,快 ~1-2s:
    // LaunchServices 按主进程判 app 是否在跑,主进程 reaped 后 `open` 就会启新实例;
    // KILL 阶段才动全树,见 run_quit_command)。路径正则枚举候选后逐 PID 验 bundle id:
    // Ours 与 Unknown(身份不明,宁可误报"在跑"让流程报错交人工)都算在跑;Other
    // (如消费级 ChatGPT 客户端)不算——它在跑不该阻塞 Codex 重启,更不该被杀。
    if platform == "macos" {
        return macos_pid_classes(MACOS_MAIN_PROCESS_MATCH)
            .iter()
            .any(|(_, class)| *class != MacosPidClass::Other);
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
    // [改名适配] macOS:候选逐 PID 归属校验,**只对 Ours 发信号**(Unknown 绝不杀)。
    // TERM(优雅)只杀主进程让 Electron 自己 reap helper;KILL 兜底杀整树残留
    // (主进程 + 孤儿 helper,防实例堆积,语义与原 pkill -f MAIN/APP 两档一致)。
    if platform == "macos" {
        let pattern = if force {
            MACOS_APP_PROCESS_MATCH
        } else {
            MACOS_MAIN_PROCESS_MATCH
        };
        let signal = if force { "-KILL" } else { "-TERM" };
        let pids: Vec<String> = macos_pid_classes(pattern)
            .into_iter()
            .filter(|(_, class)| *class == MacosPidClass::Ours)
            .map(|(pid, _)| pid.to_string())
            .collect();
        if pids.is_empty() {
            return;
        }
        let _ = Command::new("kill")
            .arg(signal)
            .args(&pids)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
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

/// [MOC-285] 把本项目 catalog 暴露的思考档位(GLM 等的 `none`/`max`,见
/// [`codex_app_transfer_registry::all_reasoning_tier_efforts`])并入 Codex 全局状态文件的
/// `electron-persisted-atom-state["enabled-reasoning-efforts"]`,在启动 Codex 前补齐。
///
/// **为什么需要**:Codex 26.623(codex-cli 0.142.3)起新增用户级持久设置
/// `enabled-reasoning-efforts`(webview LM atom,默认 `["low","medium","high","xhigh","ultra"]`)。
/// reasoning 选择器实际显示 = 模型 `supported_reasoning_levels` ∩ 该集合。MOC-241 给
/// GLM/Kimi/Qwen/MiMo/DeepSeek/Gemini/MiniMax 写的 `none`/`max` 不在默认集 → 交集为空 → picker
/// 兜底成残留「Medium」、原生两档不显示。本函数把这些档位并进启用集还原显示。
///
/// **union 语义、零副作用**:只增不删用户/Codex 既有档;picker 是「模型支持档 ∩ 启用集」,启用
/// 更多档对每个模型仍只显其声明档(GPT 仍 low/medium/high/xhigh,GLM 才出现 none/max)。文件不存在
///(Codex 从未启动过)→ 跳过不创建(镜像 [`sync_codex_pet_state`]);已是超集 → 不写(幂等)。
/// 全程失败路径 `tracing::warn!` 记录、绝不阻断启动。**调用时机**:`open_codex_app` 内、Codex 已退出
/// 时写(global-state 由 Codex 主进程启动时加载,运行中写会被忽略/覆盖,故必须在拉起前)。
fn sync_codex_reasoning_efforts_state() {
    let Some(home) = codex_app_transfer_registry::paths::resolve_home() else {
        tracing::warn!("[Reasoning] 无法解析 home 目录,跳过 enabled-reasoning-efforts 同步");
        return;
    };
    let path = home.join(".codex").join(".codex-global-state.json");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        // 文件不存在 = Codex 还没首启,不主动创建(镜像 sync_codex_pet_state)。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "[Reasoning] 读 state 文件失败,跳过同步");
            return;
        }
    };
    let mut state: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "[Reasoning] state JSON 解析失败,跳过同步");
            return;
        }
    };
    let required = codex_app_transfer_registry::all_reasoning_tier_efforts();
    if !ensure_enabled_reasoning_efforts(&mut state, &required) {
        return; // 无变化(幂等)或结构异常(已在内部 warn)
    }
    // to_string_pretty 失败时**不能** fallback 空串(会把 state 截成 corrupt),显式跳过。
    let serialized = match serde_json::to_string_pretty(&state) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "[Reasoning] state 序列化失败,跳过写回");
            return;
        }
    };
    if let Err(e) = fs::write(&path, serialized) {
        tracing::warn!(path = %path.display(), error = %e, "[Reasoning] 写 state 失败,none/max 档可能仍不显示");
    }
}

/// [MOC-285] 纯函数:把 `required` 档位并入 `state` 的
/// `electron-persisted-atom-state["enabled-reasoning-efforts"]`(union 不覆盖)。返回是否发生改动
///(`true` = 需写回)。拆出纯函数便于单测,IO 留给 [`sync_codex_reasoning_efforts_state`]。
///
/// - 顶层 / 该子对象非 object → 返回 `false`(调用方跳过,内部 warn 含文件名)。
/// - `electron-persisted-atom-state` 缺失 → 创建空 object。
/// 仅强制补「Codex 默认隐藏、而我们模型需要」的档位 = `required \ CODEX_DEFAULT`(当前 = `none`/`max`);
/// `high` 等**默认可见**档不强加 —— 尊重用户/Codex 对默认档的删改(与对 `xhigh`/`ultra` 的保留一致)。
///
/// - 顶层 / 该子对象非 object → 返回 `false`(调用方跳过,内部 warn 含文件名)。
/// - `electron-persisted-atom-state` 缺失 → 创建空 object。
/// - `enabled-reasoning-efforts` **缺失 / 空 / 过滤掉非字符串项后为空** → 以 Codex 默认集
///   `["low","medium","high","xhigh","ultra"]` 为基线再并入隐藏档(`none`/`max`),**保证不把 GPT 等
///   模型默认可见档意外砍掉**(picker 与启用集求交,基线缺了它们这些档也会消失);写入返回 `true`。
/// - **已有非空(且含 ≥1 个字符串项)值** → 仅追加缺的隐藏档(`none`/`max`),全在则返回 `false`
///   (幂等);**不**回填用户删掉的默认可见档(如 `high`);已有非字符串项忽略(防御损坏数据)。
fn ensure_enabled_reasoning_efforts(state: &mut Value, required: &[&str]) -> bool {
    const KEY: &str = "enabled-reasoning-efforts";
    // Codex 该设置的内置默认集(codex-cli 0.142.3 webview LM atom default)。键缺失/空/损坏时以它
    // 为基线,避免只写 `required` 把默认可见档(low/medium/high/xhigh/ultra)挤出交集。
    // ⚠️ 维护哨兵:这是 Codex 自身的默认值副本;**若未来 Codex 改了该设置的默认集,这里需同步更新**
    // (否则首次 seed 会把用户钉在过时默认集上,可能漏掉 Codex 新增的可见档)。
    const CODEX_DEFAULT: &[&str] = &["low", "medium", "high", "xhigh", "ultra"];

    let Some(obj) = state.as_object_mut() else {
        tracing::warn!("[Reasoning] .codex-global-state.json 顶层非 object,跳过同步");
        return false;
    };
    let atoms = obj
        .entry("electron-persisted-atom-state")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(atoms) = atoms.as_object_mut() else {
        tracing::warn!(
            "[Reasoning] .codex-global-state.json 的 electron-persisted-atom-state 非 object,跳过同步"
        );
        return false;
    };

    // 先取已有数组、过滤掉非字符串项(防御损坏数据),**再**按过滤结果是否为空决定基线:
    // 过滤后非空 → 以其为基线(had_value=true);缺失 / 空 / 全非字符串过滤后为空 → 以 Codex 默认集为
    // 基线(had_value=false)。先 filter 再判空很关键——非空但全非字符串的损坏数组(如 `[1,2]`)若按
    // 原始数组判 had_value=true,会只写 required 把 GPT 默认可见档挤出交集(PR review HIGH)。
    let filtered: Vec<String> = atoms
        .get(KEY)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let (mut current, had_value) = if filtered.is_empty() {
        (
            CODEX_DEFAULT.iter().map(|s| (*s).to_owned()).collect(),
            false,
        )
    } else {
        (filtered, true)
    };

    // 只强制补「Codex 默认隐藏」的档(required \ CODEX_DEFAULT,当前 = none/max)。high 等默认可见档
    // 即便 spec(如 DeepSeek)声明也不在此强加:absent 情形 high 已由 CODEX_DEFAULT 基线提供,present
    // 情形则尊重用户/Codex 是否保留 high(与 xhigh/ultra 一视同仁)。
    // (PR #560 codex-connector P2:旧实现 union 整个 required 会把用户删掉的默认档 high 重新加回,
    // 破坏「只增隐藏档、保留用户对默认档选择」的语义。)
    let mut added = false;
    for e in required.iter().filter(|e| !CODEX_DEFAULT.contains(e)) {
        if !current.iter().any(|x| x == e) {
            current.push((*e).to_owned());
            added = true;
        }
    }
    // 缺键/空(首次 seed)即便隐藏档已在基线里也要落盘补齐键;已有值则仅在新增隐藏档时写。
    if had_value && !added {
        return false;
    }
    atoms.insert(
        KEY.to_owned(),
        Value::Array(current.into_iter().map(Value::String).collect()),
    );
    true
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
    // [MOC-204] 额度注入 daemon 也走 CDP,只开 quota 不开 theme/unlock 时同样要端口。
    let quota_enabled = cfg
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get("codexQuotaEnabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // 草稿暂存(Stash)注入 daemon 同样走 CDP,只开 stash 不开其它时也要端口。
    let stash_enabled = cfg
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get("codexStashEnabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !plugin_unlock_needs_port && !theme_enabled && !quota_enabled && !stash_enabled {
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
    // [MOC-285] Codex 启动前补齐 enabled-reasoning-efforts 持久 atom,让 GLM 等的 none/max 档
    // 在 reasoning 选择器正常显示(Codex 26.623+ 默认启用集不含这两档)。
    sync_codex_reasoning_efforts_state();

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
    let chat_env = chat_launch_env(platform);
    let cmd = open_command(platform, resolved.as_deref(), &extra_args, &chat_env);
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
    let _guard = CODEX_MAINTENANCE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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

/// [CAT-255] **关闭 Codex.app → 跑 `work`(必须 Codex 关闭时做的维护,如就地改它独占的
/// `state_<N>.sqlite`)→ 重启 Codex.app**,全程持有 [`CODEX_MAINTENANCE_LOCK`] 跟其他
/// Codex 维护流程互斥。
///
/// - 退出失败 → 直接回 `Err`(**绝不在 Codex 可能运行时跑 `work`**);
/// - `work` 无论成败都会重启 Codex(原本开着才拉起;没开则不擅自启);
/// - 返回 `(work 结果, codex_running_after)`:`codex_running_after=false` 表示原本开着但
///   重启失败,调用方据此提示用户手动打开。
///
/// 注:内部有阻塞 sleep(quit retries + launchd grace),async 调用方应包进
/// `tokio::task::spawn_blocking` 再 await,别堵 tokio worker。
pub fn with_codex_closed<T>(platform: &str, work: impl FnOnce() -> T) -> Result<(T, bool), String> {
    let _guard = CODEX_MAINTENANCE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let was_running = is_codex_app_running(platform);
    quit_codex_app_with_retries(platform)?;
    if was_running {
        std::thread::sleep(POST_QUIT_LAUNCHD_GRACE);
    }
    let out = work();
    // 维护后重启:原本开着才拉起。relaunched=true 表示「维护后 Codex 应在运行」
    // (没开过 → 无需拉起,也算 true,不让调用方误报「重启失败」)。
    let relaunched = if was_running {
        open_codex_app(platform).is_ok()
    } else {
        true
    };
    Ok((out, relaunched))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── [MOC-285] enabled-reasoning-efforts 并入(纯函数 ensure_enabled_reasoning_efforts) ──

    fn enabled_set(state: &Value) -> Vec<String> {
        state["electron-persisted-atom-state"]["enabled-reasoning-efforts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn seed_when_key_absent_uses_codex_default_base_plus_required() {
        // 键缺失 → 以 Codex 默认集为基线再并入 {none,high,max};返回 true(需写)。
        let mut state =
            json!({"electron-persisted-atom-state": {"composer-auto-context-enabled": false}});
        let required = ["high", "max", "none"];
        assert!(ensure_enabled_reasoning_efforts(&mut state, &required));
        let set = enabled_set(&state);
        // 默认可见档保留(GPT 等不被砍)
        for d in ["low", "medium", "high", "xhigh", "ultra"] {
            assert!(set.contains(&d.to_owned()), "默认档 {d} 应保留");
        }
        // 我们的隐藏档补上
        assert!(set.contains(&"none".to_owned()) && set.contains(&"max".to_owned()));
        // 不动同子对象里的其它 atom
        assert_eq!(
            state["electron-persisted-atom-state"]["composer-auto-context-enabled"],
            json!(false)
        );
    }

    #[test]
    fn seed_creates_persisted_atom_subobject_when_missing() {
        // 顶层无 electron-persisted-atom-state → 创建之,且不污染其它顶层字段。
        let mut state = json!({"electron-avatar-overlay-open": true});
        assert!(ensure_enabled_reasoning_efforts(
            &mut state,
            &["none", "max"]
        ));
        let set = enabled_set(&state);
        assert!(set.contains(&"none".to_owned()) && set.contains(&"max".to_owned()));
        assert_eq!(state["electron-avatar-overlay-open"], json!(true));
    }

    #[test]
    fn seed_unions_into_existing_user_value_preserving_it() {
        // 已有用户值(去掉了 xhigh)→ 只追加缺的 required,保留用户既有(含其去掉 xhigh 的选择)。
        let mut state = json!({"electron-persisted-atom-state": {
            "enabled-reasoning-efforts": ["low", "medium", "high"]
        }});
        assert!(ensure_enabled_reasoning_efforts(
            &mut state,
            &["high", "max", "none"]
        ));
        let set = enabled_set(&state);
        assert_eq!(set, vec!["low", "medium", "high", "max", "none"]);
        // 用户没启用的 xhigh/ultra 不被我们硬塞回去
        assert!(!set.contains(&"xhigh".to_owned()) && !set.contains(&"ultra".to_owned()));
    }

    #[test]
    fn seed_does_not_readd_user_removed_default_tier() {
        // [MOC-285 PR #560 codex-connector P2] 用户/Codex 在已有非空值里删掉了默认可见档 high
        //(present),seeding 只补隐藏档 none/max,**不**把 high 加回 —— 与对 xhigh/ultra 的保留一视同仁。
        let mut state = json!({"electron-persisted-atom-state": {
            "enabled-reasoning-efforts": ["low", "medium", "xhigh", "ultra"]
        }});
        // required 含 high(DeepSeek),但 high ∈ CODEX_DEFAULT → 不强加。
        assert!(ensure_enabled_reasoning_efforts(
            &mut state,
            &["high", "max", "none"]
        ));
        let set = enabled_set(&state);
        assert_eq!(set, vec!["low", "medium", "xhigh", "ultra", "max", "none"]);
        assert!(
            !set.contains(&"high".to_owned()),
            "不得回填用户删掉的默认档 high"
        );
    }

    #[test]
    fn seed_absent_still_provides_high_via_default_base() {
        // 反向保证:absent 情形 high 仍由 CODEX_DEFAULT 基线提供(DeepSeek 的 high 档不受 P2 修复影响)。
        let mut state = json!({"electron-persisted-atom-state": {}});
        assert!(ensure_enabled_reasoning_efforts(
            &mut state,
            &["high", "max", "none"]
        ));
        let set = enabled_set(&state);
        for d in ["low", "medium", "high", "xhigh", "ultra", "none", "max"] {
            assert!(set.contains(&d.to_owned()), "absent 基线应含 {d}");
        }
    }

    #[test]
    fn seed_is_idempotent_when_already_superset() {
        // 已是超集 → 不改、返回 false(避免无谓写盘)。
        let mut state = json!({"electron-persisted-atom-state": {
            "enabled-reasoning-efforts": ["none", "low", "medium", "high", "xhigh", "max", "ultra"]
        }});
        let before = state.clone();
        assert!(!ensure_enabled_reasoning_efforts(
            &mut state,
            &["high", "max", "none"]
        ));
        assert_eq!(state, before, "幂等:不得有任何改动");
    }

    #[test]
    fn seed_treats_empty_array_as_absent() {
        // 显式空数组(或损坏)→ 当作缺失,回落 Codex 默认集 + required,防 GPT 档全消失。
        let mut state = json!({"electron-persisted-atom-state": {"enabled-reasoning-efforts": []}});
        assert!(ensure_enabled_reasoning_efforts(
            &mut state,
            &["none", "high", "max"]
        ));
        let set = enabled_set(&state);
        assert!(set.contains(&"low".to_owned()) && set.contains(&"none".to_owned()));
    }

    #[test]
    fn seed_treats_all_non_string_array_as_absent() {
        // [MOC-285 PR review HIGH] 非空但全非字符串的损坏数组(如 [1,2])过滤后为空 →
        // 必须回落 Codex 默认集 + required,不能只写 required 把 GPT 默认可见档挤出交集。
        let mut state = json!({"electron-persisted-atom-state": {
            "enabled-reasoning-efforts": [1, 2, true]
        }});
        assert!(ensure_enabled_reasoning_efforts(
            &mut state,
            &["high", "max", "none"]
        ));
        let set = enabled_set(&state);
        for d in ["low", "medium", "high", "xhigh", "ultra", "none", "max"] {
            assert!(set.contains(&d.to_owned()), "{d} 应在(默认集 + required)内");
        }
    }

    #[test]
    fn seed_drops_non_string_items_but_keeps_valid_ones() {
        // 混合数组:保留合法字符串项 + 丢弃非字符串项 + 并入 required。
        let mut state = json!({"electron-persisted-atom-state": {
            "enabled-reasoning-efforts": ["low", 7, "medium"]
        }});
        assert!(ensure_enabled_reasoning_efforts(&mut state, &["max"]));
        let set = enabled_set(&state);
        assert_eq!(set, vec!["low", "medium", "max"], "丢非字符串项、并入 max");
    }

    #[test]
    fn seed_returns_false_when_top_level_not_object() {
        // 顶层非 object(异常文件)→ 不改、返回 false,调用方跳过写回。
        let mut state = json!(["not", "an", "object"]);
        assert!(!ensure_enabled_reasoning_efforts(
            &mut state,
            &["none", "max"]
        ));
    }

    #[test]
    fn seed_uses_registry_required_set() {
        // 与 registry 单一来源对齐:用真实 all_reasoning_tier_efforts() 也能正确补上 none/max。
        let mut state = json!({"electron-persisted-atom-state": {}});
        let required = codex_app_transfer_registry::all_reasoning_tier_efforts();
        assert!(ensure_enabled_reasoning_efforts(&mut state, &required));
        let set = enabled_set(&state);
        assert!(set.contains(&"none".to_owned()) && set.contains(&"max".to_owned()));
    }

    #[test]
    fn running_check_command_is_platform_specific() {
        // [改名适配] macOS 改走 macos_pid_classes(bundle id 归属校验),静态命令返回空
        assert!(running_check_command("macos").is_empty());
        let windows = running_check_command("windows");
        assert_eq!(windows[0], "tasklist");
        assert!(windows.iter().any(|a| a == "IMAGENAME eq Codex.exe"));
        assert_eq!(running_check_command("linux"), vec!["pgrep", "-x", "codex"]);
    }

    #[test]
    fn macos_process_match_patterns_cover_both_bundles() {
        // [MOC-100 B→优化] 运行判定只看主进程(MAIN,快);KILL 阶段才用全树(APP)
        // [改名适配] 正则同时覆盖 Codex.app / ChatGPT.app 的候选枚举
        assert_eq!(
            MACOS_MAIN_PROCESS_MATCH,
            r"(Codex|ChatGPT)\.app/Contents/MacOS/"
        );
        assert_eq!(MACOS_APP_PROCESS_MATCH, r"(Codex|ChatGPT)\.app/Contents/");
    }

    #[test]
    fn extract_macos_bundle_root_takes_outermost_bundle() {
        // 主进程
        assert_eq!(
            extract_macos_bundle_root(
                "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT --remote-debugging-port=0"
            ),
            Some("/Applications/ChatGPT.app")
        );
        // helper:嵌套 .app 取**外层**宿主 bundle
        assert_eq!(
            extract_macos_bundle_root(
                "/Applications/ChatGPT.app/Contents/Frameworks/ChatGPT Helper (Renderer).app/Contents/MacOS/ChatGPT Helper (Renderer) --type=renderer"
            ),
            Some("/Applications/ChatGPT.app")
        );
        // 旧安装
        assert_eq!(
            extract_macos_bundle_root("/Users/u/Applications/Codex.app/Contents/MacOS/Codex"),
            Some("/Users/u/Applications/Codex.app")
        );
        // 非 bundle 进程无法定位 → None(调用方按 Unknown 处理)
        assert_eq!(
            extract_macos_bundle_root("/usr/local/bin/codex serve"),
            None
        );
    }

    #[test]
    fn classify_macos_bundle_id_only_trusts_codex_id() {
        assert_eq!(
            classify_macos_bundle_id(Some("com.openai.codex")),
            MacosPidClass::Ours
        );
        // 消费级 ChatGPT 客户端(可执行名同为 ChatGPT)必须判 Other,绝不发信号
        assert_eq!(
            classify_macos_bundle_id(Some("com.openai.chat")),
            MacosPidClass::Other
        );
        assert_eq!(
            classify_macos_bundle_id(Some("store.alyse.codex-app-transfer")),
            MacosPidClass::Other
        );
        // 读不到 / 空 → Unknown(计入在跑但不杀)
        assert_eq!(classify_macos_bundle_id(None), MacosPidClass::Unknown);
        assert_eq!(classify_macos_bundle_id(Some("")), MacosPidClass::Unknown);
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
        // [改名适配] macOS 改走 run_quit_command 的 PID 归属校验路径,静态命令返回空
        assert!(quit_command("macos", false).is_empty());
        assert!(quit_command("macos", true).is_empty());

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
            open_command("macos", Some("/Applications/Codex.app"), &[], &[]),
            vec!["open", "-a", "/Applications/Codex.app"]
        );
        // [改名适配] 路径落空按 bundle id 兜底(新旧两代同 id;`-a Codex` 在
        // 26.707-only 安装上启不动,`-a ChatGPT` 可能解析到消费级客户端)
        assert_eq!(
            open_command("macos", None, &[], &[]),
            vec!["open", "-b", "com.openai.codex"]
        );
        assert_eq!(
            open_command("macos", None, &["--remote-debugging-port=9222".into()], &[]),
            vec![
                "open",
                "-b",
                "com.openai.codex",
                "--args",
                "--remote-debugging-port=9222"
            ]
        );
        let windows = open_command("windows", None, &[], &[]);
        assert_eq!(windows[0], "explorer.exe");
        assert!(windows[1].starts_with("shell:AppsFolder\\"));
        assert!(windows[1].contains("OpenAI.Codex"));
        let linux = open_command("linux", None, &[], &[]);
        assert_eq!(linux[0], "sh");
        assert_eq!(linux[1], "-c");
        assert!(linux[2].contains("codex"));
    }

    // [MOC-323] Chat env 经 `open --env` 注入,排在 `-a`/`--args` 之前
    #[test]
    fn open_command_injects_chat_env_before_app() {
        let env = vec![
            ("NODE_OPTIONS".to_string(), "--require /x/p.js".to_string()),
            (
                "CODEX_API_BASE_URL".to_string(),
                "http://localhost:18080/backend-api".to_string(),
            ),
        ];
        let cmd = open_command(
            "macos",
            Some("/Applications/ChatGPT.app"),
            &["--remote-debugging-port=0".into()],
            &env,
        );
        assert_eq!(
            cmd,
            vec![
                "open",
                "--env",
                "NODE_OPTIONS=--require /x/p.js",
                "--env",
                "CODEX_API_BASE_URL=http://localhost:18080/backend-api",
                "-a",
                "/Applications/ChatGPT.app",
                "--args",
                "--remote-debugging-port=0",
            ]
        );
    }
}
