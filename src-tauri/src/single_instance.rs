//! [MOC-196] macOS 自管单实例锁:flock 持锁 + 就绪态握手 + 超时接管。
//!
//! ## 为什么不用 tauri-plugin-single-instance(macOS)
//!
//! 插件 macOS 实现(2.4.2)的故障链(#436「Macbook air m5 打不开」真机复现):
//! `/tmp/<id>_si.sock` connect 成功 → 单向写 argv → **无条件 `exit(0)`**,无 ACK
//! 协议。socket bind 又发生在窗口创建之前 —— 一旦主实例在 setup 同步段 hang
//! (restore 文件 IO / `fs2::lock_exclusive` 阻塞锁 / 网络盘),就成为「进程活着
//! 但永远没窗口」的僵尸,之后用户每次双击都被静默 exit(0),无窗口无对话框,
//! 表现为「app 永久打不开」,且无任何日志可查。
//!
//! ## 本实现的三层防护
//!
//! 1. **flock 锁本体**(`<app_home>/instance.lock`,fs2):内核保证进程死亡
//!    (含 SIGKILL)自动释放 —— 「残留锁文件卡死启动」物理上不存在;flock
//!    非竞争性失败(网络盘 ENOTSUP 等)降级无锁启动,绝不拦用户(F1);
//! 2. **就绪态握手**:第二实例发 `ACTIVATE`,主实例按 [`mark_ready`](窗口创建
//!    成功后置位)分流回 `OK`(唤起窗口)/ `STARTING`(启动中,带 started_at)/
//!    `ERR`(收到无法解析的请求 —— 活着但拒绝,对端不得当僵尸);setup hang 的
//!    僵尸只能回 `STARTING`,超过宽限期(30s)即被判定接管 —— 插件方案正是缺
//!    这一问一答才被僵尸骗过;
//! 3. **超时接管 + 永不静默**:握手失败先隔 1s 重试一次(瞬时 IO 错不当死刑
//!    证据),仍失败 → 读锁文件 pid + exe 校验持锁者确为本应用(防 PID 复用
//!    误杀)→ SIGKILL → 重试 flock 接管;所有不可恢复路径都 osascript 弹原生
//!    alert,彻底消灭「静默 exit(0)」。
//!
//! ## 跨版本兜底(升级窗口期)
//!
//! 旧版本(插件)与本实现的锁命名空间不同(`/tmp/<id>_si.sock` vs
//! `<app_home>/instance.lock`),互相不可见会双实例并存 → 抢 proxy 端口 +
//! 任一退出时 restore 把 config.toml 还原,另一实例静默脱代理。兜底:
//! - **拿到 flock 后**探测 legacy socket(锁被占 = 新版主实例在跑,legacy 是
//!   它的兼容监听,不得误判;拿到锁才可能存在旧版),有旧版监听 → alert 提示
//!   退出旧版后 exit(0)(探测的 connect+EOF 会顺带触发旧版唤窗);
//! - 成为主实例后兼容 bind legacy socket:旧版本二次启动 connect 成功
//!   notify 后自行退出,本实例按旧 wire 格式(`cwd\0\0args`)解析唤窗。
//!
//! Windows/Linux 仍走 tauri_plugin_single_instance(各自实现/僵尸特性不同,
//! 二期按需同构);本模块整体 `#[cfg(target_os = "macos")]`(见 main.rs)。
//!
//! ## 已知不覆盖(接受边界)
//!
//! - tokio runtime 活着但主线程事件循环卡死的「半僵尸」会回 OK(识别需主线程
//!   心跳,无实证案例);
//! - 僵尸主实例 + 它当初 socket bind 就失败(socket_ok=false):不接管只提示
//!   (握手失败无法归因僵尸),用户按 alert 手动处理;
//! - 系统 wall clock 大步跳变影响 30s 宽限判定(前跳可能误判/回拨延迟接管);
//! - 用户运行中手删 instance.lock → flock 绑 inode 不绑路径,新启动开新
//!   inode 拿锁成功 → 双主实例(与旧插件手删 socket 同级,非回归)。

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use fs2::FileExt;
use tauri::{AppHandle, Emitter, Manager};

/// 主实例「窗口已就绪」标志。`RunEvent::Ready`(窗口创建成功)后置 true,
/// listener 据此回 OK / STARTING —— 僵尸(setup hang)永远到不了 Ready。
static READY: AtomicBool = AtomicBool::new(false);
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();
/// 本进程启动时刻(unix 秒),STARTING 回应携带,第二实例据此算宽限期。
static STARTED_AT_UNIX: OnceLock<u64> = OnceLock::new();
/// STARTING 期间收到的唤起请求暂存队列,`mark_ready` 时统一补放 —— 否则
/// 启动期到达的 deeplink(`codex-app-transfer://...`)会被永久丢弃
/// (chatgpt-codex P2)。READY 的判定与入队/排空必须在同一把锁内完成,
/// 防「listener 读到 READY=false → mark_ready 排空 → listener 才入队」的
/// 竞态丢失;见 [`queue_or_dispatch`] / [`mark_ready`]。
static PENDING_ACTIVATIONS: std::sync::Mutex<Vec<Vec<String>>> = std::sync::Mutex::new(Vec::new());

/// 握手等待主实例回应的超时。健康主实例的 listener 是常驻线程,回应在毫秒级;
/// 超时即视为对端无应答(僵尸/假 socket)。listener 侧 per-connection 读超时
/// 复用同值:防恶意/误连的「连而不写」对端把串行 listener 楔死,进而让真实
/// 握手全部超时、健康主实例被误判僵尸(silent-failure review F2 链 B)。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// 主实例启动宽限期:STARTING 状态超过该时长仍未 ready → 判定 setup hang 僵尸。
/// 正常启动 setup 在秒级完成;30s 足够覆盖慢盘冷启动,又不至于让用户对着
/// 真僵尸干等太久。
const STARTUP_GRACE: Duration = Duration::from_secs(30);
/// 接管时等待内核释放 flock 的重试(SIGKILL 后释放是即时的,重试只为进程
/// 表项清理的调度间隙)。
const TAKEOVER_RETRIES: u32 = 5;
const TAKEOVER_RETRY_INTERVAL: Duration = Duration::from_millis(300);

/// 持有 flock 的 guard。绑定在 main() 局部变量上活到进程结束;进程死亡
/// (任何方式)内核自动释放,无需显式清理。
pub struct InstanceLock {
    _file: std::fs::File,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LockInfo {
    pid: u32,
    /// 主实例二进制完整路径,接管 kill 前与 `ps -o command=` 比对(F4:
    /// 裸子串会误匹 `tail -f ~/.codex-app-transfer/x` 这类无关进程)。
    exe: String,
    started_at_unix: u64,
    /// listener socket 是否 bind 成功。false 时第二实例**不得**走僵尸接管
    /// (握手失败是 socket 缺位、不是对端僵尸),只提示后退出。
    socket_ok: bool,
}

fn app_home() -> PathBuf {
    codex_app_transfer_registry::config_file()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex-app-transfer")
        })
}

fn lock_path() -> PathBuf {
    app_home().join("instance.lock")
}

fn sock_path() -> PathBuf {
    app_home().join("instance.sock")
}

/// 旧版本插件(tauri-plugin-single-instance 2.4.2 macOS)的 socket 路径:
/// `/tmp/{identifier 替换 ./- 为 _}_si.sock`,identifier 固定为
/// `store.alyse.codex-app-transfer`(tauri.conf.json)。跨版本兜底用。
fn legacy_sock_path() -> PathBuf {
    PathBuf::from("/tmp/store_alyse_codex_app_transfer_si.sock")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 窗口创建成功(RunEvent::Ready)时由 main.rs 调用:置 ready + 存 AppHandle,
/// 并补放 STARTING 期间暂存的唤起请求(deeplink 不丢)。
/// set 在 store 之前:listener 读到 READY=true(SeqCst)必能取到已 set 的 handle。
/// READY 置位与队列排空在同一把锁内:之后任何 listener 入队尝试都会在锁内
/// 看到 READY=true 而改走直接 dispatch,不存在「排空后才入队」的丢失窗口。
pub fn mark_ready(app: AppHandle) {
    let _ = APP_HANDLE.set(app);
    let drained: Vec<Vec<String>> = {
        let mut pending = PENDING_ACTIVATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        READY.store(true, Ordering::SeqCst);
        pending.drain(..).collect()
    };
    for args in drained {
        dispatch_show_and_deeplink(args);
    }
}

/// listener 收到 ACTIVATE 时的统一入口:ready → 立即唤起;未 ready → 入队
/// 暂存(回应仍是 STARTING,第二实例提示后退出,请求由主实例 ready 时补放)。
/// 返回是否已 ready(决定回 OK 还是 STARTING)。
fn queue_or_dispatch(args: Vec<String>) -> bool {
    let pending = PENDING_ACTIVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if READY.load(Ordering::SeqCst) {
        drop(pending);
        dispatch_show_and_deeplink(args);
        true
    } else {
        let mut pending = pending;
        pending.push(args);
        false
    }
}

/// main() 在 Builder 之前调用。返回:
/// - `Some(lock)`:本进程是主实例(首启 / 接管成功),caller 持 guard 至退出;
/// - `None`:锁基础设施不可用(open 失败 / flock 非竞争性失败如网络盘
///   ENOTSUP),降级为无单实例保护继续启动 —— 锁故障绝不能阻止用户用 app。
///
/// 第二实例路径(锁被健康主实例持有)在内部 `exit(0)`,不返回。
pub fn acquire_or_exit() -> Option<InstanceLock> {
    let lock_path = lock_path();
    if let Some(parent) = lock_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("[single-instance] app_home 创建失败,降级无锁启动: {e}");
            return None;
        }
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("[single-instance] 锁文件打开失败,降级无锁启动: {e}");
            return None;
        }
    };

    match file.try_lock_exclusive() {
        Ok(()) => {
            // 跨版本兜底:拿到 flock = 没有任何新版本实例(新版主实例必持锁),
            // 此时 legacy socket 若有监听者只可能是**旧版本**(插件方案,不持本
            // 锁)→ 提示用户退出旧版,不静默共存(双实例抢 proxy 端口 + 退出
            // restore 互踩 config.toml)。探测必须放在锁判定**之后** —— 新版主
            // 实例自己也兼容 bind 了 legacy socket,放在锁前会让新版二次启动
            // 误报「旧版本在运行」且 deeplink argv 全丢(chatgpt-codex P2)。
            // connect+EOF 顺带触发旧版唤窗,用户能直接看到它;旧版若是僵尸
            // (#436 形态)也 connect 成功 → 同一 alert,指引明确,严格优于
            // 旧插件的静默 exit(0)。exit 自动释放刚拿的 flock。
            if UnixStream::connect(legacy_sock_path()).is_ok() {
                alert(
                    "检测到旧版本 Codex App Transfer 正在运行。请先退出它(若找不到窗口,可在「活动监视器」中结束 codex-app-transfer 进程)再打开新版本。",
                );
                std::process::exit(0);
            }
            Some(become_primary(file))
        }
        // 只有「锁被占」(EWOULDBLOCK)才意味着有另一实例;ENOTSUP/EIO 等是
        // flock 在该文件系统不可用(典型:网络盘 home)。混为一谈会让 flock
        // 不可用的机器每次启动都走 takeover→alert+exit,复刻「永远打不开」
        // (silent-failure review F1, CRITICAL)。
        Err(e) if e.kind() == fs2::lock_contended_error().kind() => secondary_flow(&lock_path),
        Err(e) => {
            tracing::warn!("[single-instance] flock 不可用(非锁竞争: {e}),降级无锁启动");
            None
        }
    }
}

/// 拿到 flock → 当主实例:bind listener socket、写锁文件元数据、起 listener 线程。
fn become_primary(file: std::fs::File) -> InstanceLock {
    let started = now_unix();
    let _ = STARTED_AT_UNIX.set(started);

    // 先 bind 再写锁文件,让 socket_ok 如实落盘(残留 socket 文件需先清 ——
    // flock 已证明无并存实例,残留必是上次强杀遗留;这里是全模块唯一安全的
    // 删除点,takeover 路径不得删 socket,见 F3)。
    let sock = sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!(
                "[single-instance] listener bind 失败(单实例握手不可用,二次启动将提示而非唤起): {e}"
            );
            None
        }
    };

    let info = LockInfo {
        pid: std::process::id(),
        exe: std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        started_at_unix: started,
        socket_ok: listener.is_some(),
    };
    if let Err(e) = write_lock_info(&file, &info) {
        tracing::warn!("[single-instance] 锁文件元数据写入失败(接管诊断信息缺失): {e}");
    }

    if let Some(listener) = listener {
        std::thread::Builder::new()
            .name("single-instance-listener".into())
            .spawn(move || listener_loop(listener))
            .map_err(|e| tracing::warn!("[single-instance] listener 线程启动失败: {e}"))
            .ok();
    }

    // 跨版本兜底:同时占住 legacy socket。旧版本二次启动 connect 它,按旧插件
    // 行为 notify(cwd\0\0args)后自行 exit(0) → 本实例解析唤窗,不双开。
    // bind 失败只 warn(场景退化为升级窗口双实例,旧版本时代既有行为)。
    let legacy = legacy_sock_path();
    let _ = std::fs::remove_file(&legacy);
    match UnixListener::bind(&legacy) {
        Ok(l) => {
            std::thread::Builder::new()
                .name("single-instance-legacy".into())
                .spawn(move || legacy_listener_loop(l))
                .map_err(|e| tracing::warn!("[single-instance] legacy listener 线程启动失败: {e}"))
                .ok();
        }
        Err(e) => {
            tracing::warn!("[single-instance] legacy socket bind 失败(升级窗口期不防旧版双开): {e}")
        }
    }

    InstanceLock { _file: file }
}

fn write_lock_info(mut file: &std::fs::File, info: &LockInfo) -> std::io::Result<()> {
    use std::io::Seek;
    let json = serde_json::to_string(info).map_err(std::io::Error::other)?;
    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    file.write_all(json.as_bytes())?;
    file.flush()
}

fn read_lock_info(path: &PathBuf) -> Option<LockInfo> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// 主实例 listener:逐连接处理 `ACTIVATE\x1f<arg>\x1f<arg>...`(对端写完
/// shutdown(Write),本端 read 到 EOF),按 ready 态回一行应答后关连接。
///
/// 串行单线程,两道防御(F2):per-connection 读超时防「连而不写」楔死循环;
/// 读失败/未知协议回 `ERR` 而非静默关 —— 静默关会让对端立刻读到 EOF →
/// parse 失败 → 误判僵尸 → SIGKILL 健康主实例。`ERR` 显式表达「活着但拒绝」。
fn listener_loop(listener: UnixListener) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT));
        let mut req = String::new();
        let reply = match stream.read_to_string(&mut req) {
            Ok(_) if req.starts_with("ACTIVATE") => {
                // skip(1) 丢掉 "ACTIVATE" token,余下为完整 argv;argv0(路径)由
                // dispatch_show_and_deeplink 的 deeplink 前缀过滤天然排除。
                let args: Vec<String> = req.split('\x1f').skip(1).map(str::to_owned).collect();
                if queue_or_dispatch(args) {
                    "OK\n".to_string()
                } else {
                    format!("STARTING {}\n", STARTED_AT_UNIX.get().copied().unwrap_or(0))
                }
            }
            Ok(_) => {
                tracing::warn!("[single-instance] 未知协议请求({}B),回 ERR", req.len());
                "ERR\n".to_string()
            }
            Err(e) => {
                tracing::warn!("[single-instance] 握手请求读取失败({e}),回 ERR");
                "ERR\n".to_string()
            }
        };
        let _ = stream.write_all(reply.as_bytes());
        let _ = stream.flush();
    }
}

/// 跨版本兼容 listener:按旧插件 wire 格式(`<cwd>\0\0<arg>\0<arg>...`,无应答
/// 协议)接收旧版本二次启动的 notify,唤窗 + deeplink 转发后关连接。
fn legacy_listener_loop(listener: UnixListener) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        let mut req = String::new();
        if stream.read_to_string(&mut req).is_err() {
            continue;
        }
        let args = parse_legacy_notify(&req);
        // 同样走暂存队列:STARTING 期间旧版 notify 的 deeplink 不丢
        let _ = queue_or_dispatch(args);
    }
}

/// 旧插件 wire 格式解析:`<cwd>\0\0<argv0>\0<argv1>...` → 返回 argv(去 argv0)。
fn parse_legacy_notify(req: &str) -> Vec<String> {
    let (_cwd, args) = req.split_once("\0\0").unwrap_or_default();
    args.split('\0').skip(1).map(str::to_owned).collect()
}

/// 唤起主窗口 + 转发 deeplink(与旧插件回调语义一致)。
fn dispatch_show_and_deeplink(args: Vec<String>) {
    let Some(app) = APP_HANDLE.get() else {
        // mark_ready 的 set→store 顺序保证 READY=true 时必有 handle,此分支
        // 理论不可达;留日志防未来顺序被改出「回 OK 但没唤起」的无影 bug(F6)。
        tracing::error!("[single-instance] APP_HANDLE 缺失,唤起请求被丢弃(不应发生)");
        return;
    };
    let app = app.clone();
    if let Err(e) = app.clone().run_on_main_thread(move || {
        crate::show_main_window(&app);
        for arg in &args {
            if arg.starts_with("codex-app-transfer:") {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.emit("codex-deeplink", arg.clone());
                }
            }
        }
    }) {
        tracing::warn!("[single-instance] run_on_main_thread 失败,唤起未执行: {e}");
    }
}

/// 第二实例:握手 → 唤起退出 / 等待退出 / 僵尸接管。
fn secondary_flow(lock_path: &PathBuf) -> Option<InstanceLock> {
    let info = read_lock_info(lock_path);

    // 锁被持有但 socket 当初就没 bind 成 → 握手必失败,但这不是僵尸证据,
    // 不能走 kill 接管(可能误杀健康实例)。提示后退出。
    if matches!(&info, Some(i) if !i.socket_ok) {
        alert("Codex App Transfer 已在运行,但无法唤起其窗口(实例间通信不可用)。请从 Dock 切换,或手动退出后重开。");
        std::process::exit(0);
    }

    // 单次握手失败可能是瞬时 IO 错/listener 忙,隔 1s 重试一次再下僵尸结论
    // (F2 链 A:把「一次失败」从死刑证据降级为「两次独立失败」)。
    let mut result = handshake();
    if result.is_err() {
        std::thread::sleep(Duration::from_secs(1));
        result = handshake();
    }

    match result {
        Ok(Reply::Ok) => {
            // 健康主实例已唤起窗口,本进程使命完成
            std::process::exit(0);
        }
        Ok(Reply::Refused) => {
            // 对端活着但拒绝了请求(读错/协议不符):不是僵尸,绝不能 kill。
            alert("Codex App Transfer 已在运行,但本次唤起请求未被接受(通信异常)。请从 Dock 切换到已打开的窗口。");
            std::process::exit(0);
        }
        Ok(Reply::Starting(started_at)) => {
            let elapsed = now_unix().saturating_sub(started_at);
            if Duration::from_secs(elapsed) < STARTUP_GRACE {
                alert("Codex App Transfer 正在启动中,请稍候几秒。本次打开请求(含链接)会在启动完成后自动处理。");
                std::process::exit(0);
            }
            // 启动超过宽限期仍未 ready = setup hang 僵尸 → 接管
            tracing::warn!(
                "[single-instance] 主实例 STARTING 超过 {}s 未就绪,按僵尸接管",
                STARTUP_GRACE.as_secs()
            );
            takeover(lock_path, info)
        }
        Err(e) => {
            // 两次握手都失败:持锁者不应答 = 僵尸(flock 在手即进程活着,
            // 死进程的锁早被内核释放、走不到 secondary_flow)
            tracing::warn!("[single-instance] 握手两次失败({e}),按僵尸接管");
            takeover(lock_path, info)
        }
    }
}

enum Reply {
    Ok,
    Starting(u64),
    /// 对端活着但拒绝(读错/协议不符)。与「无应答」严格区分:Refused 绝不
    /// 触发接管 kill。
    Refused,
}

fn handshake() -> Result<Reply, String> {
    let stream = UnixStream::connect(sock_path()).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    stream
        .set_write_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|e| format!("set_write_timeout: {e}"))?;

    let mut req = String::from("ACTIVATE");
    for arg in std::env::args() {
        req.push('\x1f');
        req.push_str(&arg);
    }
    let mut s = &stream;
    s.write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("shutdown: {e}"))?;

    let mut reply = String::new();
    s.read_to_string(&mut reply)
        .map_err(|e| format!("read: {e}"))?;
    parse_reply(&reply)
}

fn parse_reply(reply: &str) -> Result<Reply, String> {
    let line = reply.trim();
    if line == "OK" {
        return Ok(Reply::Ok);
    }
    if line == "ERR" {
        return Ok(Reply::Refused);
    }
    if let Some(ts) = line.strip_prefix("STARTING ") {
        return ts
            .parse::<u64>()
            .map(Reply::Starting)
            .map_err(|e| format!("STARTING ts parse: {e}"));
    }
    Err(format!("unexpected reply: {line:?}"))
}

/// 僵尸接管:校验 → SIGKILL → 重试 flock → 接管为主实例。
///
/// **此路径不得删 socket 文件**(F3/edge-case 双确认):未持有 flock 时删除,
/// 在双 takeover 错开竞速下会把赢家刚 bind 的 live socket 路径删掉,使健康
/// 新主实例永久不可达(后续启动全部 connect ENOENT → 又走接管 kill)。唯一
/// 安全删除点在 become_primary(持锁后)。
fn takeover(lock_path: &PathBuf, info: Option<LockInfo>) -> Option<InstanceLock> {
    if let Some(info) = &info {
        if pid_matches_lock_exe(info.pid, &info.exe) {
            tracing::warn!(
                "[single-instance] SIGKILL 僵尸主实例 pid={}(started_at={})",
                info.pid,
                info.started_at_unix
            );
            match std::process::Command::new("kill")
                .args(["-9", &info.pid.to_string()])
                .output()
            {
                Ok(o) if !o.status.success() => tracing::warn!(
                    "[single-instance] kill 退出非零({}): {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => tracing::warn!("[single-instance] kill 执行失败: {e}"),
                _ => {}
            }
        } else {
            // PID 复用 / 锁文件信息陈旧:持锁者不是 transfer,绝不误杀。
            // flock 大概率拿不到,走下面的 alert 退出路径。
            tracing::warn!(
                "[single-instance] 锁文件 pid={} 与记录的 exe 不匹配,跳过 kill(防 PID 复用误杀)",
                info.pid
            );
        }
    }

    for _ in 0..TAKEOVER_RETRIES {
        std::thread::sleep(TAKEOVER_RETRY_INTERVAL);
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
        else {
            continue;
        };
        if file.try_lock_exclusive().is_ok() {
            tracing::info!("[single-instance] 接管成功,以主实例继续启动");
            return Some(become_primary(file));
        }
    }
    // 走到这里:持锁者还活着(可能是并发接管的赢家=健康新主实例,也可能是
    // 杀不掉的 D 状态僵尸)。文案不单一归因,避免引导用户误杀健康实例(F5)。
    alert(
        "Codex App Transfer 检测到另一个实例且无法自动接管。它可能正在运行 —— 请先从 Dock 切换查看;若确认无响应,在「活动监视器」结束 codex-app-transfer 进程后重开;仍无法结束时重启电脑即可恢复。",
    );
    std::process::exit(1);
}

/// 接管 kill 前的持锁者身份校验:`ps -o command=` 与锁文件记录的 exe 比对。
/// exe 非空 → 要求命令行以该完整路径开头;exe 缺失(旧锁文件/写入失败)→
/// 退化为路径段匹配 `/codex-app-transfer`(`.codex-app-transfer` 目录引用
/// 不会命中,因前缀是 `.` 非 `/`)。查询失败/进程不存在 → false → 不杀。
fn pid_matches_lock_exe(pid: u32, exe: &str) -> bool {
    let Some(cmdline) = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    else {
        return false;
    };
    command_matches_exe(&cmdline, exe)
}

/// 纯函数:命令行与期望 exe 的匹配判定(便于单测)。
fn command_matches_exe(cmdline: &str, exe: &str) -> bool {
    if cmdline.is_empty() {
        return false;
    }
    if !exe.is_empty() {
        return cmdline.starts_with(exe);
    }
    cmdline.contains("/codex-app-transfer")
}

/// 原生 alert(osascript):Builder 尚未运行,tauri dialog 不可用;绝不静默退出。
/// spawn 不等待(alert 由独立 osascript 进程展示,本进程按流程退出);spawn
/// 失败(MDM 禁用等)兜底 eprintln —— 「永不静默」的最后一层(F8)。
fn alert(msg: &str) {
    let script = format!(
        "display alert \"Codex App Transfer\" message \"{}\"",
        msg.replace('\\', "\\\\").replace('"', "\\\"")
    );
    if let Err(e) = std::process::Command::new("osascript")
        .args(["-e", &script])
        .spawn()
    {
        eprintln!("[single-instance] alert 弹窗失败({e}),原文: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reply_ok() {
        assert!(matches!(parse_reply("OK\n"), Ok(Reply::Ok)));
        assert!(matches!(parse_reply("OK"), Ok(Reply::Ok)));
    }

    #[test]
    fn parse_reply_err_is_refused_not_zombie() {
        // ERR = 对端活着但拒绝,必须解析为 Refused(走提示退出),不能落进
        // Err(僵尸判定 → kill)——F2 链 A 防回归。
        assert!(matches!(parse_reply("ERR\n"), Ok(Reply::Refused)));
    }

    #[test]
    fn parse_reply_starting_with_ts() {
        match parse_reply("STARTING 1760000000\n") {
            Ok(Reply::Starting(ts)) => assert_eq!(ts, 1760000000),
            other => panic!("wrong: {:?}", other.err()),
        }
    }

    #[test]
    fn parse_reply_rejects_garbage() {
        assert!(parse_reply("").is_err());
        assert!(parse_reply("HELLO").is_err());
        assert!(parse_reply("STARTING abc").is_err());
    }

    #[test]
    fn lock_info_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("instance.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        let info = LockInfo {
            pid: 4242,
            exe: "/Applications/Codex App Transfer.app/Contents/MacOS/codex-app-transfer".into(),
            started_at_unix: 1760000000,
            socket_ok: true,
        };
        write_lock_info(&file, &info).unwrap();
        let read = read_lock_info(&path).unwrap();
        assert_eq!(read.pid, 4242);
        assert_eq!(read.started_at_unix, 1760000000);
        assert!(read.socket_ok);
    }

    #[test]
    fn command_match_requires_exe_prefix_when_exe_known() {
        let exe = "/Applications/Codex App Transfer.app/Contents/MacOS/codex-app-transfer";
        assert!(command_matches_exe(exe, exe));
        assert!(command_matches_exe(&format!("{exe} --some-flag"), exe));
        // F4 防回归:命令行含 ".codex-app-transfer" 路径的无关进程不得命中
        assert!(!command_matches_exe(
            "tail -f /Users/alice/.codex-app-transfer/tray.log",
            exe
        ));
        assert!(!command_matches_exe("/usr/bin/nano notes.txt", exe));
    }

    #[test]
    fn command_match_fallback_when_exe_unknown() {
        // exe 缺失时的退化匹配:要求路径段 `/codex-app-transfer`
        assert!(command_matches_exe(
            "/Applications/Codex App Transfer.app/Contents/MacOS/codex-app-transfer",
            ""
        ));
        // `.codex-app-transfer`(数据目录)前缀是 `.` 不是 `/`,不命中
        assert!(!command_matches_exe(
            "tail -f /Users/alice/.codex-app-transfer/tray.log",
            ""
        ));
        assert!(!command_matches_exe("", ""));
    }

    #[test]
    fn pid_match_rejects_non_transfer_pid() {
        // pid 1 = launchd → 不匹配 → 不杀(防误杀方向)
        assert!(!pid_matches_lock_exe(
            1,
            "/Applications/x/codex-app-transfer"
        ));
        // 不存在的 pid → ps 空输出 → false
        assert!(!pid_matches_lock_exe(4_000_000, ""));
    }

    #[test]
    fn legacy_notify_parse_extracts_args() {
        let req = "/Users/alice\0\0/Applications/old.app/Contents/MacOS/codex-app-transfer\0codex-app-transfer://open";
        let args = parse_legacy_notify(req);
        assert_eq!(args, vec!["codex-app-transfer://open"]);
        // 垃圾输入安全返回空
        assert!(parse_legacy_notify("").is_empty());
        assert!(parse_legacy_notify("no-separator").is_empty());
    }

    /// 握手协议端到端:模拟主实例 listener(STARTING 态)与第二实例 handshake。
    #[test]
    fn handshake_protocol_end_to_end_starting() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("t.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = String::new();
            stream.read_to_string(&mut req).unwrap();
            assert!(req.starts_with("ACTIVATE"), "req={req:?}");
            stream.write_all(b"STARTING 123\n").unwrap();
        });

        let stream = UnixStream::connect(&sock).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut s = &stream;
        s.write_all(b"ACTIVATE\x1f/path/to/app").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).unwrap();
        match parse_reply(&reply) {
            Ok(Reply::Starting(123)) => {}
            other => panic!("wrong: {:?}", other.err()),
        }
        server.join().unwrap();
    }
}
