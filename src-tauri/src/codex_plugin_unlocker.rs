//! Codex Desktop Plugins 解锁器 —— 运行时伴侣守护进程
//!
//! 通过 Chrome DevTools Protocol (CDP) 向 Codex Desktop 渲染进程注入 JavaScript,
//! 调用 React state 中的 `setAuthMethod('chatgpt')` 来解锁 Plugins 选项卡。
//!
//! 使用方式:
//! 1. 创建 `PluginUnlockService`
//! 2. 调用 `start()` 启动守护循环
//! 3. 调用 `stop()` 停止
//!
//! 守护循环行为:
//! - 检测 Codex Desktop 进程是否存在
//! - 尝试连接 `http://127.0.0.1:9222/json/list` 获取 CDP endpoint
//! - WebSocket 连接后注入解锁脚本
//! - 监听 `Page.loadEventFired`,刷新后自动重新注入
//! - 断开时指数退避重连

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{Sink, SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 解锁器状态（线程安全共享）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum UnlockStatus {
    /// 未启动或 Codex Desktop 未运行/无调试端口
    Disconnected,
    /// 正在连接 CDP
    Connecting,
    /// 已连接,等待页面就绪
    Connected,
    /// 注入成功,Plugins 已解锁
    Injected,
    /// 注入失败
    Failed { error: String },
}

/// 服务配置
#[derive(Debug, Clone)]
pub struct UnlockConfig {
    /// CDP HTTP 端点（获取 WebSocket URL）
    pub cdp_http_url: String,
    /// 重连退避:初始延迟（毫秒）。第一次失败后等这么久重试,
    /// 每次失败 ×2 直到 `reconnect_max_ms`。1s 起够快,不会让用户感觉卡。
    pub reconnect_base_ms: u64,
    /// 重连退避上限。30s 是经验值:Codex 启动 / 系统休眠唤醒最长 ~30s 内
    /// CDP 必然就绪,再长意义不大。
    pub reconnect_max_ms: u64,
}

impl Default for UnlockConfig {
    fn default() -> Self {
        Self {
            cdp_http_url: "http://127.0.0.1:9222/json/list".into(),
            reconnect_base_ms: 1_000,
            reconnect_max_ms: 30_000,
        }
    }
}

/// CDP Page 信息（来自 `/json/list`）
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CdpPage {
    id: String,
    #[serde(rename = "type")]
    page_type: String,
    url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    ws_url: Option<String>,
}

/// CDP WebSocket 消息
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CdpResponse {
    id: Option<u64>,
    #[serde(rename = "result")]
    result: Option<serde_json::Value>,
    error: Option<CdpError>,
    #[serde(rename = "method")]
    method: Option<String>,
    #[serde(rename = "params")]
    params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CdpError {
    code: i32,
    message: String,
}

/// 解锁服务
pub struct PluginUnlockService {
    config: UnlockConfig,
    status: Arc<RwLock<UnlockStatus>>,
    /// 控制守护循环的通道
    cmd_tx: mpsc::Sender<ServiceCommand>,
    cmd_rx: Arc<Mutex<mpsc::Receiver<ServiceCommand>>>,
    /// CDP 消息 ID 单调递增计数器(无锁,daemon + tests 共享)
    msg_id: Arc<AtomicU64>,
}

#[derive(Debug)]
enum ServiceCommand {
    Stop,
    /// 强制重新注入（前端手动触发）
    Reinject,
}

impl PluginUnlockService {
    pub fn new(config: UnlockConfig) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        Self {
            config,
            status: Arc::new(RwLock::new(UnlockStatus::Disconnected)),
            cmd_tx,
            cmd_rx: Arc::new(Mutex::new(cmd_rx)),
            msg_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 使用默认配置创建。前端 HTTP handler 跟 setup hook 都通过这个共享同
    /// 一份 OnceCell 实例,见 `admin::handlers::plugin_unlock::get_service`。
    pub fn with_defaults() -> Self {
        Self::new(UnlockConfig::default())
    }

    /// 获取当前状态
    pub async fn status(&self) -> UnlockStatus {
        self.status.read().await.clone()
    }

    /// 启动守护循环（非阻塞）
    pub fn start(&self) {
        let config = self.config.clone();
        let status = self.status.clone();
        let cmd_rx = self.cmd_rx.clone();
        let msg_id = self.msg_id.clone();

        tokio::spawn(async move {
            run_daemon(config, status, cmd_rx, msg_id).await;
        });
    }

    /// 停止守护循环
    pub async fn stop(&self) {
        let _ = self.cmd_tx.send(ServiceCommand::Stop).await;
    }

    /// 前端手动触发重新注入
    pub async fn reinject(&self) {
        let _ = self.cmd_tx.send(ServiceCommand::Reinject).await;
    }
}

/// 守护循环主逻辑
async fn run_daemon(
    config: UnlockConfig,
    status: Arc<RwLock<UnlockStatus>>,
    cmd_rx: Arc<Mutex<mpsc::Receiver<ServiceCommand>>>,
    msg_id: Arc<AtomicU64>,
) {
    let mut reconnect_delay = config.reconnect_base_ms;

    loop {
        // 检查是否有外部命令(此时未连 WS — 真正注入态的 Reinject 在
        // `connect_and_monitor` 内 select! 处理)。未连接时收到 Reinject
        // 视作"加速重连请求":reset backoff 让下一次 detect 立即跑,
        // 而不是静默 noop。
        {
            let mut rx = cmd_rx.lock().await;
            if let Ok(cmd) = rx.try_recv() {
                match cmd {
                    ServiceCommand::Stop => {
                        tracing::info!("[PluginUnlock] daemon stopped by command");
                        set_status(&status, UnlockStatus::Disconnected).await;
                        return;
                    }
                    ServiceCommand::Reinject => {
                        tracing::info!(
                            "[PluginUnlock] reinject requested while disconnected, resetting backoff"
                        );
                        reconnect_delay = config.reconnect_base_ms;
                    }
                }
            }
        }

        // 阶段 1: 检测 CDP 端口是否可用
        match detect_cdp(&config.cdp_http_url).await {
            Some(pages) => {
                // Codex Desktop 同时开多个 BrowserWindow:主窗口
                // `app://-/index.html` + 宠物悬浮窗 `app://-/index.html?initialRoute=
                // %2Favatar-overlay` + 可能的 DevTools / extension。我们只想注主
                // 窗口(那里才有 Plugins UI 跟 AuthContext)。
                //
                // 早期版本 `find(|p| p.page_type == "page")` 拿第一个 — 真机
                // 发现宠物窗排第一,导致一直注错地方(log 里 "找不到
                // setAuthMethod hook" 正是因为宠物窗根本没这个 Context)。
                //
                // 筛选规则:type=page + URL 含 `index.html` + 不含 `avatar-overlay`
                // (宠物窗用 query param 路由,主窗口无 query 或别的路由)。
                let (target, all_pages_for_log) = {
                    let snapshot: Vec<String> = pages
                        .iter()
                        .filter(|p| p.page_type == "page")
                        .map(|p| p.url.clone())
                        .collect();
                    let target = pages.into_iter().find(|p| {
                        p.page_type == "page"
                            && p.url.contains("index.html")
                            && !p.url.contains("avatar-overlay")
                    });
                    (target, snapshot)
                };
                if let Some(page) = target {
                    if let Some(ws_url) = page.ws_url {
                        set_status(&status, UnlockStatus::Connecting).await;
                        tracing::info!("[PluginUnlock] connecting to CDP: {}", ws_url);
                        match connect_and_monitor(&ws_url, &cmd_rx, &msg_id, &status).await {
                            Ok(()) => {
                                tracing::info!("[PluginUnlock] connection ended gracefully");
                                reconnect_delay = config.reconnect_base_ms;
                            }
                            Err(e) => {
                                tracing::warn!("[PluginUnlock] connection error: {}", e);
                                set_status(
                                    &status,
                                    UnlockStatus::Failed {
                                        error: e.to_string(),
                                    },
                                )
                                .await;
                            }
                        }
                    }
                } else {
                    // CDP 在跑但没找到主窗口 — 可能 Codex 还在 mount / 只
                    // 开了宠物悬浮窗 / 未来 Codex URL schema 变了。warn 级日志
                    // 列出我们看到的全部 page URLs,方便 support 诊断"我的
                    // Codex 在开但 daemon 一直显示 Disconnected"。状态保持
                    // Disconnected 让 backoff 重试。
                    tracing::warn!(
                        "[PluginUnlock] CDP reachable but no main window matched (need URL containing 'index.html' and not 'avatar-overlay'); visible pages={:?}",
                        all_pages_for_log
                    );
                    set_status(&status, UnlockStatus::Disconnected).await;
                }
            }
            None => {
                // CDP 不可用,保持 Disconnected。set_status 内部已做 != 比对,
                // 无需额外 if_not 包装。
                set_status(&status, UnlockStatus::Disconnected).await;
            }
        }

        // 指数退避:1s → 2s → 4s → ... → 30s 封顶
        sleep(Duration::from_millis(reconnect_delay)).await;
        reconnect_delay = (reconnect_delay * 2).min(config.reconnect_max_ms);
    }
}

/// 检测 CDP HTTP 端点，返回 page 列表
async fn detect_cdp(url: &str) -> Option<Vec<CdpPage>> {
    match reqwest::get(url).await {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.json::<Vec<CdpPage>>().await {
                    Ok(pages) if !pages.is_empty() => Some(pages),
                    _ => None,
                }
            } else {
                None
            }
        }
        Err(e) => {
            tracing::debug!("[PluginUnlock] CDP detect failed: {}", e);
            None
        }
    }
}

/// WebSocket 连接、注入、并持续监控页面刷新
async fn connect_and_monitor(
    ws_url: &str,
    cmd_rx: &Arc<Mutex<mpsc::Receiver<ServiceCommand>>>,
    msg_id_counter: &AtomicU64,
    status: &Arc<RwLock<UnlockStatus>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = connect_async(ws_url).await?;
    let (mut write, mut read) = ws_stream.split();

    // 1. 启用 Runtime domain
    let (runtime_enable, runtime_enable_id) =
        make_cdp_msg(msg_id_counter, "Runtime.enable", json!({}));
    write.send(WsMessage::Text(runtime_enable)).await?;
    let _ = await_cdp_response(&mut read, runtime_enable_id, Duration::from_secs(5)).await;

    // 2. 启用 Page domain（监听刷新事件）
    let (page_enable, page_enable_id) = make_cdp_msg(msg_id_counter, "Page.enable", json!({}));
    write.send(WsMessage::Text(page_enable)).await?;
    let _ = await_cdp_response(&mut read, page_enable_id, Duration::from_secs(5)).await;

    // 3. 首次注入
    inject_unlock_script(&mut write, &mut read, msg_id_counter, status).await?;

    // 4. 持续监控：监听 Page.loadEventFired 和外部命令
    loop {
        tokio::select! {
            // 监听 WebSocket 消息
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Ok(resp) = serde_json::from_str::<CdpResponse>(&text) {
                            // 检测 Page.loadEventFired 事件
                            if resp.method.as_deref() == Some("Page.loadEventFired") {
                                tracing::info!("[PluginUnlock] page refreshed, reinjecting...");
                                inject_unlock_script(&mut write, &mut read, msg_id_counter, status).await?;
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        tracing::info!("[PluginUnlock] WebSocket closed");
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("[PluginUnlock] WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }

            // 监听外部命令
            cmd = async {
                let mut rx = cmd_rx.lock().await;
                rx.recv().await
            } => {
                match cmd {
                    Some(ServiceCommand::Reinject) => {
                        tracing::info!("[PluginUnlock] manual reinject requested");
                        inject_unlock_script(&mut write, &mut read, msg_id_counter, status).await?;
                    }
                    Some(ServiceCommand::Stop) => {
                        tracing::info!("[PluginUnlock] stop requested, closing connection");
                        let _ = write.close().await;
                        return Ok(());
                    }
                    None => break,
                }
            }

            // 心跳检测：如果 30 秒没有收到任何消息，检查连接是否仍然活跃
            _ = sleep(Duration::from_secs(30)) => {
                // 发送一个简单的 Runtime.evaluate 来检测连接;响应会从外层
                // select! 的 read.next() 分支流回,被忽略(不是 Page.loadEventFired)
                let (ping, _ping_id) = make_cdp_msg(msg_id_counter, "Runtime.evaluate", json!({"expression": "1+1"}));
                if let Err(e) = write.send(WsMessage::Text(ping)).await {
                    tracing::warn!("[PluginUnlock] ping failed, connection dead: {}", e);
                    break;
                }
            }
        }
    }

    Ok(())
}

/// 发送注入脚本
async fn inject_unlock_script(
    write: &mut (impl Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    read: &mut (impl Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin),
    msg_id_counter: &AtomicU64,
    status: &Arc<RwLock<UnlockStatus>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    set_status(status, UnlockStatus::Connected).await;

    // 注入脚本 — 解锁 Codex Desktop Plugins 选项卡。
    //
    // 算法借鉴 galaxywk223/codex-plugin-unlocker (MIT, 2026-05-11)
    //   https://github.com/galaxywk223/codex-plugin-unlocker/blob/main/codex_plugin_unlocker/inject/plugin-unlock.js
    //
    // 关键差异 vs. 早期版本(找 useState hook 链上的 setAuthMethod setter):
    // - 新策略走 React Context — 从 plugin 入口 DOM 节点拿 fiber,沿 `fiber.return`
    //   向上爬,检查每层 `memoizedProps.value` / `pendingProps.value`,找带
    //   `setAuthMethod` + `authMethod` 字段的对象(即 `AuthContext.Provider` value)
    // - Codex Desktop 26.513+ 的 React state 结构变了,旧 hook-scan 策略失效;
    //   Context.Provider 是 React 公开 API,比 hook 链表稳定得多
    // - 加 DOM-level enable(清 disabled / __reactProps disabled),即使 setter
    //   找不到也能让按钮可点(strict fallback)
    // - 加 MutationObserver,SPA 路由跳转重渲时自动重跑
    //
    // 异步包装:返回 Promise<bool>,通过 awaitPromise: true 拿到结果。
    // 内部最多 6 次重试(每次 500ms 间隔)等 plugin 按钮 DOM 出现。
    let unlock_script = r#"
(async function() {
    const MARKER = '__codexAppTransferPluginUnlocker';
    window[MARKER] = window[MARKER] || { version: '2.1.10', unlocked: false };

    const selectors = {
        disabledInstallButton: 'button:disabled.w-full.justify-center, [role="button"][aria-disabled="true"].cursor-not-allowed',
        pluginNavButton: 'nav[role="navigation"] button.h-token-nav-row.w-full',
        pluginSvgPath: 'svg path[d^="M7.94562 14.0277"]',
    };

    function reactFiberFrom(element) {
        const key = Object.keys(element).find((k) => k.startsWith('__reactFiber'));
        return key ? element[key] : null;
    }
    function reactPropsKeyFrom(element) {
        return Object.keys(element).find((k) => k.startsWith('__reactProps'));
    }
    function authContextValueFrom(element) {
        for (let fiber = reactFiberFrom(element); fiber; fiber = fiber.return) {
            for (const v of [fiber.memoizedProps?.value, fiber.pendingProps?.value]) {
                if (v && typeof v === 'object'
                    && typeof v.setAuthMethod === 'function'
                    && 'authMethod' in v) {
                    return v;
                }
            }
        }
        return null;
    }
    function spoofChatGPTAuthMethod(element) {
        const auth = authContextValueFrom(element);
        if (!auth) return false;
        if (auth.authMethod === 'chatgpt') { window[MARKER].unlocked = true; return true; }
        auth.setAuthMethod('chatgpt');
        window[MARKER].unlocked = true;
        return true;
    }
    function pluginEntryButton() {
        const byIcon = document.querySelector(
            selectors.pluginNavButton + ' ' + selectors.pluginSvgPath
        )?.closest('button');
        if (byIcon) return byIcon;
        return Array.from(document.querySelectorAll(selectors.pluginNavButton)).find((b) => {
            const t = (b.textContent || '').trim();
            return /^(插件|Plugins)(\s+-\s+.*)?$/i.test(t);
        }) || null;
    }
    function normalizePluginEntryLabel(button) {
        const node = Array.from(button.querySelectorAll('span, div')).reverse()
            .flatMap((n) => Array.from(n.childNodes))
            .find((n) => n.nodeType === 3
                && /^(插件|Plugins)( - 已解锁| - Unlocked)?$/i.test((n.nodeValue || '').trim()));
        if (!node) return;
        const cur = (node.nodeValue || '').trim();
        node.nodeValue = /^Plugins/i.test(cur) ? 'Plugins' : '插件';
    }
    function enablePluginEntry() {
        const btn = pluginEntryButton();
        if (!btn) return false;
        spoofChatGPTAuthMethod(btn);
        btn.disabled = false;
        btn.removeAttribute('disabled');
        btn.style.display = '';
        btn.querySelectorAll('*').forEach((n) => { n.style.display = ''; });
        normalizePluginEntryLabel(btn);
        const propsKey = reactPropsKeyFrom(btn);
        if (propsKey) { btn[propsKey].disabled = false; }
        if (btn.dataset.codexAppTransferPluginUnlocked !== 'true') {
            btn.dataset.codexAppTransferPluginUnlocked = 'true';
            btn.addEventListener('click', () => spoofChatGPTAuthMethod(btn), true);
        }
        return true;
    }
    function unblockButtonElement(button) {
        button.disabled = false;
        button.removeAttribute('disabled');
        button.removeAttribute('aria-disabled');
        button.classList.remove('disabled', 'opacity-50', 'cursor-not-allowed', 'pointer-events-none');
        button.style.pointerEvents = 'auto';
        button.tabIndex = 0;
        const propsKey = reactPropsKeyFrom(button);
        if (propsKey) {
            button[propsKey].disabled = false;
            button[propsKey]['aria-disabled'] = false;
        }
    }
    function labelForcedInstallButton(button) {
        const node = Array.from(button.childNodes).find((n) => {
            const t = (n.nodeValue || '').trim();
            return n.nodeType === 3
                && (/^安装\s/.test(t) || /^Install\s/.test(t) || t === '强制安装');
        });
        if (node) node.nodeValue = '强制安装';
    }
    function unblockPluginInstallButtons() {
        document.querySelectorAll(selectors.disabledInstallButton).forEach((b) => {
            const t = (b.textContent || '').trim();
            if (!/^安装\s/.test(t) && !/^Install\s/.test(t) && t !== '强制安装') return;
            unblockButtonElement(b);
            labelForcedInstallButton(b);
        });
    }
    function runUnlock() {
        try {
            enablePluginEntry();
            unblockPluginInstallButtons();
        } catch (e) {
            window[MARKER].lastError = String(e?.stack || e);
        }
    }
    function scheduleUnlock() {
        if (window[MARKER].scanPending) return;
        window[MARKER].scanPending = true;
        setTimeout(() => {
            window[MARKER].scanPending = false;
            runUnlock();
        }, 200);
    }

    // 重试等 plugin 按钮 DOM 出现(SPA 刚加载完时按钮可能还在 lazy mount)
    for (let i = 0; i < 6; i++) {
        runUnlock();
        if (window[MARKER].unlocked) break;
        await new Promise((r) => setTimeout(r, 500));
    }

    // SPA 路由跳转 / sidebar 重渲会冲掉我们的 DOM mutation,装 observer 持续 enforce
    window[MARKER].observer?.disconnect();
    window[MARKER].observer = new MutationObserver(scheduleUnlock);
    window[MARKER].observer.observe(
        document.body || document.documentElement,
        { childList: true, subtree: true }
    );

    return window[MARKER].unlocked === true;
})()
"#;

    let (evaluate, evaluate_id) = make_cdp_msg(
        msg_id_counter,
        "Runtime.evaluate",
        json!({
            "expression": unlock_script,
            "awaitPromise": true,
            "returnByValue": true
        }),
    );

    write.send(WsMessage::Text(evaluate)).await?;

    // 必须按 CDP message id 匹配响应,而不是简单读"下一帧"。
    // 注入脚本内 `console.log` 会触发 `Runtime.consoleAPICalled` 事件帧,
    // 30 秒心跳的 evaluate 响应也可能排队 — 这些都不带 evaluate_id,
    // 会跟我们的目标响应交错。await_cdp_response 循环丢弃非目标 id 的帧。
    let parsed = match await_cdp_response(read, evaluate_id, Duration::from_secs(8)).await {
        Ok(resp) => resp,
        Err(e) => {
            set_status(
                status,
                UnlockStatus::Failed {
                    error: format!("CDP Runtime.evaluate 响应等待失败: {e}"),
                },
            )
            .await;
            return Err(e.into());
        }
    };

    if let Some(error) = parsed.error {
        let msg = format!("CDP error {}: {}", error.code, error.message);
        set_status(status, UnlockStatus::Failed { error: msg.clone() }).await;
        return Err(msg.into());
    }

    // 脚本只在 React fiber 上确实拿到 setAuthMethod 且调用成功时才返回 true;
    // 返回 false 或无 result.value 都视为注入失败 — 不能 fallback 到"也算
    // 成功"(违反 no-silent-destructive-fallback 规则)。
    if let Some(result) = parsed.result {
        if let Some(val) = result.get("result").and_then(|v| v.get("value")) {
            if val.as_bool() == Some(true) {
                set_status(status, UnlockStatus::Injected).await;
                return Ok(());
            }
        }
    }

    set_status(
        status,
        UnlockStatus::Failed {
            error: "注入脚本未找到 React setAuthMethod hook,Codex Desktop 版本可能不兼容".into(),
        },
    )
    .await;
    Err("inject script returned non-true".into())
}

/// 循环读 WebSocket 帧,丢弃非目标 id 的事件 / 响应,直到拿到 `target_id`
/// 对应的响应或超时。
///
/// CDP 协议在 active session 上会持续推送各种事件帧(`Runtime.consoleAPICalled`
/// / `Page.loadEventFired` / 其他 request 的 response),不能假设"下一帧
/// 就是我刚发的 request 的 reply"— 必须按 `resp.id == Some(target_id)`
/// 精确匹配。
async fn await_cdp_response(
    read: &mut (impl Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin),
    target_id: u64,
    timeout: Duration,
) -> Result<CdpResponse, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let frame = match tokio::time::timeout_at(deadline, read.next()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(e))) => return Err(format!("ws read error: {e}")),
            Ok(None) => return Err("ws closed before response".into()),
            Err(_) => return Err(format!("timed out waiting for id={target_id}")),
        };
        let WsMessage::Text(text) = frame else {
            continue;
        };
        let resp: CdpResponse = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if resp.id == Some(target_id) {
            return Ok(resp);
        }
        tracing::trace!(
            target_id,
            dropped_id = ?resp.id,
            dropped_method = ?resp.method,
            "[PluginUnlock] dropping non-target CDP frame while awaiting response"
        );
    }
}

/// 生成 CDP 消息 JSON,返回 `(序列化后的 text frame, 该消息的 id)`。
/// 调用方需保留 `id` 用于 `await_cdp_response` 匹配响应。
fn make_cdp_msg(counter: &AtomicU64, method: &str, params: serde_json::Value) -> (String, u64) {
    let id = counter.fetch_add(1, Ordering::Relaxed) + 1;
    let json = json!({
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string();
    (json, id)
}

/// 设置状态（带日志）
async fn set_status(status: &Arc<RwLock<UnlockStatus>>, new: UnlockStatus) {
    let mut s = status.write().await;
    if *s != new {
        tracing::info!("[PluginUnlock] status: {:?} → {:?}", *s, new);
        *s = new;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_serialization() {
        let s = UnlockStatus::Failed {
            error: "test".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("failed"));
    }

    #[test]
    fn test_default_config() {
        let c = UnlockConfig::default();
        assert_eq!(c.cdp_http_url, "http://127.0.0.1:9222/json/list");
    }
}
