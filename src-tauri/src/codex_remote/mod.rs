//! Codex 移动端远程控制 —— Telegram Bot Channel daemon(MOC-249 M1)。
//!
//! 形态:transfer Rust 端跑一个 Telegram bot(纯 HTTPS long-poll,无 relay / 公网回调,
//! 绕开 Codex renderer CSP),收到授权用户的消息后用 [`driver`] 经 CDP 驱动 Codex 跑一轮,
//! 流式把 assistant 输出回编到 Telegram 消息。手机端 = 用户已有的 Telegram app。
//!
//! **M1 范围**:对话问答式(纯文本→prompt),命令 `/help /status /new /stop`;鉴权走
//! settings 白名单(`/bind` 码流程 + 工具批准转发留 M2)。**暂不支持工具执行**:prompt
//! 直接进 Codex,若 Codex 要跑命令会卡在桌面批准 UI(M2 做批准转发)。
//!
//! 开关:settings `codexRemoteControlEnabled`(默认关)+ `codexRemoteControlBotToken`。
//! 见 [`crate::admin::services::desktop::process::should_attach_debug_port`](远程控制开
//! 启时让 Codex 带 `--remote-debugging-port` 起,与 quota/theme 共用同一 CDP 端口)。

pub mod driver;
pub mod telegram;

use serde_json::Value;
use telegram::{Message, TelegramClient};
use tokio::time::Duration;

/// getUpdates long-poll 服务端挂起秒数。
const LONG_POLL_SECS: u64 = 25;
/// 一轮对话流式轮询间隔(Telegram editMessageText 限速,不宜更密)。
const STREAM_POLL: Duration = Duration::from_millis(1200);
/// 一轮对话最长等待轮数(≈ MAX_POLLS × STREAM_POLL,超时收尾)。
const MAX_POLLS: u32 = 180; // ≈ 3.6 min
/// Telegram 单条消息字符上限(留余量)。
const TG_MAX_CHARS: usize = 3800;

/// 全局轮次锁:一台 Codex 同一时刻只驱动一轮(第二个 prompt 排队等)。
/// `/stop` `/status` 等快命令**不**取此锁,故能在长轮次进行中插队。
static TURN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn settings() -> Option<Value> {
    crate::admin::registry_io::load()
        .ok()
        .and_then(|c| c.get("settings").cloned())
}

fn enabled() -> bool {
    settings()
        .as_ref()
        .and_then(|s| s.get("codexRemoteControlEnabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn bot_token() -> Option<String> {
    settings()
        .as_ref()
        .and_then(|s| s.get("codexRemoteControlBotToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// 授权用户白名单(numeric id 或 `@username`,大小写不敏感)。
fn allowed_users() -> Vec<String> {
    settings()
        .as_ref()
        .and_then(|s| s.get("codexRemoteControlAllowedUsers"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().trim_start_matches('@').to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn is_authorized(msg: &Message, allow: &[String]) -> bool {
    let Some(from) = &msg.from else {
        return false;
    };
    let id = from.id.to_string();
    allow.iter().any(|a| {
        a == &id
            || from
                .username
                .as_deref()
                .is_some_and(|u| u.eq_ignore_ascii_case(a))
    })
}

/// Telegram bot daemon 主循环。开关关 / 无 token 时空转;开启后长轮询并分发。
pub async fn run_remote_control_daemon() {
    loop {
        if !enabled() {
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        }
        let Some(token) = bot_token() else {
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };
        let client = match TelegramClient::new(token.clone(), LONG_POLL_SECS) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[RemoteControl] Telegram client 构建失败: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        // offset 每个 bot 会话独立重置:Telegram update_id 是 per-bot 的,换 token 后
        // 复用旧 offset 会让 getUpdates 跳过新 bot 的(可能更小的)update_id → 新 token
        // 看着像死的(bot-review P2)。Telegram 不会重投已确认 update,重置到 0 安全。
        let mut offset: i64 = 0;
        // **丢弃积压**(bot-review P2):功能关闭/无 token 期间 daemon 不 getUpdates,
        // Telegram 会留存这些消息(最长 24h);offset=0 起会把「关闭期间 / 发信人尚未进
        // 白名单时」发的指令当 live 命令立即执行(重放)。会话启动先用 offset=-1(取队尾
        // 最后一条、遗忘之前所有)把 offset 推到最新之后,只接受**启用之后**到达的消息。
        // drain **必须成功**才进 live loop —— 失败(如 Telegram 短暂不可达)就退避重试,
        // 绝不以 offset=0 进 live loop,否则下一次成功 poll 会重放积压(bot-review P2)。
        let mut drained = false;
        for attempt in 0..6 {
            if !enabled() || bot_token().as_deref() != Some(token.as_str()) {
                break;
            }
            match client.get_updates(-1, 0).await {
                Ok(updates) => {
                    if let Some(max_id) = updates.iter().map(|u| u.update_id).max() {
                        offset = max_id + 1;
                        tracing::info!("[RemoteControl] 丢弃启用前积压,offset 起于 {offset}");
                    }
                    drained = true;
                    break;
                }
                Err(e) => {
                    let wait = (3u64 << attempt.min(4)).min(60);
                    tracing::warn!(
                        "[RemoteControl] 初始 drain 失败(第 {} 次,{wait}s 后重试): {e}",
                        attempt + 1
                    );
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
            }
        }
        if !drained {
            // drain 始终失败:不进 live loop(避免 offset=0 重放积压),回外层整体重建。
            tracing::warn!("[RemoteControl] 初始 drain 持续失败,本次不进 live loop,稍后重建会话");
            continue;
        }
        tracing::info!(
            "[RemoteControl] Telegram bot daemon 已启动 (driver schema v{})",
            driver::DRIVER_SCHEMA_VERSION
        );
        // 内层长轮询循环:开关关闭 / token 变更时退出回外层重建。
        // 连续错误用指数退避(3→6→…→封顶 60s)避免永久错误(如 token 失效)下
        // 3s 一轮刷屏空转;日志降频(首次 warn,后续 debug)。
        let mut err_backoff: u64 = 0;
        loop {
            if !enabled() || bot_token().as_deref() != Some(token.as_str()) {
                tracing::info!("[RemoteControl] 配置变更,重建 bot 会话");
                break;
            }
            match client.get_updates(offset, LONG_POLL_SECS).await {
                Ok(updates) => {
                    err_backoff = 0;
                    for u in updates {
                        offset = offset.max(u.update_id + 1);
                        if let Some(msg) = u.message {
                            // spawn:长轮次不阻塞长轮询(否则 /stop 在轮次中收不到)
                            let client2 = client.clone();
                            tokio::spawn(async move { handle_message(&client2, msg).await });
                        }
                    }
                }
                Err(e) => {
                    if err_backoff == 0 {
                        tracing::warn!("[RemoteControl] getUpdates 错误(将退避重试): {e}");
                        err_backoff = 3;
                    } else {
                        err_backoff = (err_backoff * 2).min(60);
                        tracing::debug!(
                            "[RemoteControl] getUpdates 持续错误,退避 {err_backoff}s: {e}"
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(err_backoff)).await;
                }
            }
        }
    }
}

async fn handle_message(client: &TelegramClient, msg: Message) {
    let chat_id = msg.chat.id;
    if !is_authorized(&msg, &allowed_users()) {
        let (id, uname) = msg
            .from
            .as_ref()
            .map(|f| {
                (
                    f.id.to_string(),
                    f.username.clone().unwrap_or_else(|| "（无）".into()),
                )
            })
            .unwrap_or_else(|| ("?".into(), "?".into()));
        notify(
            client,
            chat_id,
            &format!(
                "⛔ 未授权。请在 transfer 设置的「远程控制白名单」加入以下任一,再重试:\n\
                 • user id = {id}\n• username = @{uname}"
            ),
        )
        .await;
        return;
    }

    let text = msg.text.unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if let Some(cmd) = text.strip_prefix('/') {
        handle_command(client, chat_id, cmd).await;
    } else {
        run_turn(client, chat_id, text).await;
    }
}

async fn handle_command(client: &TelegramClient, chat_id: i64, cmd: &str) {
    // 去掉 Telegram 的 @botname 后缀,取首 token 作命令名
    let name = cmd.split_whitespace().next().unwrap_or("");
    let name = name.split('@').next().unwrap_or(name).to_lowercase();
    let reply = match name.as_str() {
        "start" | "help" | "帮助" => HELP_TEXT.to_string(),
        "status" | "状态" => match driver::snapshot().await {
            Ok(s) => format!(
                "📟 Codex 状态\n• composer: {}\n• 运行中: {}",
                if s.composer_present {
                    "在场"
                } else {
                    "不在场"
                },
                match s.submitting {
                    Some(true) => "是",
                    Some(false) => "否",
                    None => "未知",
                }
            ),
            Err(e) => format!("⚠️ 读取状态失败:{e}\n(确认 Codex.app 正在运行)"),
        },
        // /new 取 TURN_LOCK(try_lock):有轮次进行中则拒绝,避免并发点新建把活动对话
        // 从正在跑的 run_turn 下面换掉(bot-review P2)。/status /stop 仍 bypass(正确)。
        "new" | "新建" => match TURN_LOCK.try_lock() {
            Ok(_guard) => match driver::new_chat().await {
                Ok(true) => "🆕 已新建对话".to_string(),
                Ok(false) => "未找到新建按钮(当前视图可能不支持)".to_string(),
                Err(e) => format!("⚠️ 新建失败:{e}"),
            },
            Err(_) => "⏳ 有一轮对话正在进行,请先 /stop 或等它结束再 /new。".to_string(),
        },
        "stop" | "停止" => match driver::stop().await {
            Ok(via) if via == "none" => "ℹ️ 当前没有正在进行的轮次,无需停止。".to_string(),
            Ok(via) => format!("🛑 已发送停止({via})"),
            Err(e) => format!("⚠️ 停止失败:{e}"),
        },
        other => format!("未知命令 /{other}。发送 /help 看可用命令。"),
    };
    notify(client, chat_id, &reply).await;
}

/// 给用户回消息(best-effort);失败仅 log(回消息通道都断了无更优雅补救)。
/// 区别于 `let _ =` 静默吞咽:至少留痕,便于区分「用户没收到」vs「我没发出去」。
async fn notify(client: &TelegramClient, chat_id: i64, text: &str) {
    if let Err(e) = client.send_message(chat_id, text).await {
        tracing::warn!(chat_id, "[RemoteControl] 回复发送失败: {e}");
    }
}

const HELP_TEXT: &str = "🤖 Codex 远程控制\n\
    直接发消息 = 给 Codex 当前对话发一轮 prompt。\n\n\
    命令:\n\
    /new 新建对话\n\
    /stop 停止当前轮\n\
    /status 查看 Codex 状态\n\
    /help 帮助\n\n\
    注:M1 为对话问答式;若 Codex 需要批准命令/工具,请到桌面端确认(批准转发到手机为 M2)。";

/// 驱动 Codex 跑一轮并流式回发。取 [`TURN_LOCK`] 串行化(一台 Codex 一次一轮)。
async fn run_turn(client: &TelegramClient, chat_id: i64, prompt: &str) {
    let _guard = TURN_LOCK.lock().await;

    // 1) 确保 composer 在场(不在则新建对话)。new_chat 失败/没找到按钮必须显式
    //    告知并 return —— 否则下游 set_input 退化成误导性的「输入失败」。
    match driver::snapshot().await {
        // 初始已在跑(桌面端有一轮进行中)→ 拒绝注入(bot-review P2):此时发送钮是停止钮,
        // set_input/submit 会覆盖草稿并停掉桌面轮次。此处未驱动任何东西,释放锁安全。
        Ok(s) if s.submitting == Some(true) => {
            notify(
                client,
                chat_id,
                "⏳ Codex 桌面端有一轮正在进行,请等它结束、或在桌面停止后再发。",
            )
            .await;
            return;
        }
        Ok(s) if !s.composer_present => match driver::new_chat().await {
            Ok(true) => tokio::time::sleep(Duration::from_millis(1500)).await,
            Ok(false) => {
                notify(
                    client,
                    chat_id,
                    "⚠️ 当前 Codex 视图没有输入框,也找不到「新建对话」按钮。请在桌面端打开一个对话后重试。",
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::warn!("[RemoteControl] new_chat 失败: {e}");
                notify(
                    client,
                    chat_id,
                    &format!("⚠️ 无法新建对话:{e}\n(确认 Codex.app 正在运行)"),
                )
                .await;
                return;
            }
        },
        Ok(_) => {}
        Err(e) => {
            notify(
                client,
                chat_id,
                &format!("⚠️ Codex 未就绪:{e}\n(确认 Codex.app 正在运行)"),
            )
            .await;
            return;
        }
    }

    // 2) 灌 prompt + 提交
    if let Err(e) = driver::set_input(prompt).await {
        notify(client, chat_id, &format!("⚠️ 输入失败:{e}")).await;
        return;
    }
    tokio::time::sleep(Duration::from_millis(350)).await;
    if let Err(e) = driver::submit().await {
        notify(client, chat_id, &format!("⚠️ 提交失败:{e}")).await;
        return;
    }

    // 3) 占位消息 + 流式编辑。占位发送失败**不 return**(submit 已让 Codex 起跑,return
    //    会放锁让下条 prompt 驱动同页,bot-review P2):msg_id 置 None,无流式但仍持锁轮询
    //    到 Codex 空闲。
    let msg_id: Option<i64> = match client.send_message(chat_id, "▍ 正在处理…").await {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!("[RemoteControl] 占位消息发送失败,改为静默持锁轮询到 Codex 空闲: {e}");
            None
        }
    };
    // 不变量(bot-review P2):TURN_LOCK 必须持到 Codex 真正空闲(submitting==false)/
    // CDP 不可达 / MAX_POLLS 才释放 —— 绝不在本地轮次仍在跑时 return/break 放锁,否则
    // 下一条 prompt 会驱动同一页面、破坏 one-turn-at-a-time。故所有「无文本/编辑失败」
    // 情形只发一次提示,继续持锁轮询,不提前 return。
    let mut last_sent = String::new(); // 已成功编辑进 Telegram 的内容
    let mut last_seen = String::new(); // Codex 上一轮观察到的内容(稳定性判定,与编辑解耦)
    let mut stable: u32 = 0;
    let mut read_err_streak: u32 = 0;
    let mut edit_err_streak: u32 = 0;
    let mut give_up_edit = false; // 编辑连续失败 → 停止编辑但继续持锁轮询
    let mut saw_running = false; // submitting 出现过 Some(true)(模型确实起跑)
    let mut warned_no_text = false; // 「无文本」提示只发一次
    let mut completed = false; // 是否经「真实 idle 证据」正常完成(区别于超时/CDP 断)
    for i in 0..MAX_POLLS {
        tokio::time::sleep(STREAM_POLL).await;
        let snap = match driver::snapshot().await {
            Ok(s) => {
                read_err_streak = 0;
                s
            }
            Err(_) => {
                read_err_streak += 1;
                if read_err_streak >= 5 {
                    break; // Codex 退出/CDP 断 → 已不可驱动,释放锁安全
                }
                continue;
            }
        };
        if snap.submitting == Some(true) {
            saw_running = true;
        }
        let trimmed = truncate_tg(&snap.reply.unwrap_or_default());
        // 稳定性按「Codex 观察到的内容是否变化」算,与能否成功编辑解耦
        if trimmed == last_seen {
            stable += 1;
        } else {
            stable = 0;
            last_seen = trimmed.clone();
        }
        // 流式编辑(占位消息存在且未放弃时):C1 —— 仅成功才推进 last_sent;连续失败熔断
        // 但**不 return**,改为停止编辑、继续持锁轮询到 Codex 空闲。
        if let Some(mid) = msg_id {
            if !give_up_edit && !trimmed.is_empty() && trimmed != last_sent {
                match client.edit_message_text(chat_id, mid, &trimmed).await {
                    Ok(()) => {
                        last_sent = trimmed;
                        edit_err_streak = 0;
                    }
                    Err(e) => {
                        edit_err_streak += 1;
                        tracing::warn!(
                            chat_id,
                            fail = edit_err_streak,
                            "[RemoteControl] editMessageText 失败: {e}"
                        );
                        if edit_err_streak >= 3 {
                            give_up_edit = true;
                            let _ = client
                                .send_message(
                                    chat_id,
                                    "⚠️ 流式更新中断(bot 可能被限流/踢出/token 失效)。Codex 仍在本地继续,完成前不接受新指令;可发 /stop 中止。",
                                )
                                .await;
                        }
                    }
                }
            }
        }
        // H1:长时间无文本时给一次提示(不再 return —— 必须持锁到 Codex 空闲)
        if last_sent.is_empty() && !warned_no_text && ((i >= 15 && !saw_running) || i >= 40) {
            warned_no_text = true;
            let msg = if !saw_running {
                "⏳ Codex 还没开始响应,可能在桌面端等你批准命令/工具(M1 不支持远程批准)。完成前不接受新指令;到桌面确认,或发 /stop 中止。"
            } else {
                "⏳ Codex 已开始但暂无文本输出(可能在跑工具或等批准)。完成前不接受新指令;发 /status 查看或 /stop 中止。"
            };
            let _ = client.send_message(chat_id, msg).await;
        }
        // 完成判定(bot-review P2):必须有**真实 idle 证据**,不靠纯 stable-text 的 drift。
        // ① done_idle:submitting 明确读到 false(显式空闲)+ 内容稳定;
        // ② done_final:Codex 给最终 assistant 答案打了 final-assistant 标记(streaming /
        //    Thinking / 跑工具 / 等批准期间**不在**)+ 内容稳定 —— 比 isSubmitting 可靠,
        //    且覆盖 fiber 漂移读不到 submitting 的 Codex 版本(codex-e2e-test skill 实证)。
        // 两者都拿不到(submitting 漂移 + 无 final 标记,如纯工具轮)→ 不提前完成,持锁
        // 等到 MAX_POLLS,由循环后的「超时仍在跑则 stop」兜底,绝不在仍在跑时放锁。
        let done_idle = snap.submitting == Some(false) && stable >= 2;
        let done_final = snap.final_ready && stable >= 2;
        if done_idle || done_final {
            completed = true;
            break;
        }
    }
    // 循环退出三种:completed(正常完成)/ read_err_streak(Codex 不可达,释放安全)/
    // MAX_POLLS 耗尽(可能仍在跑)。后两者取末张快照确认:仍 submitting==true → stop 中止
    // 再释放锁(bot-review P2:超时仍在跑就放锁会让下条 prompt 驱动同一页面)。
    if !completed {
        if let Ok(s) = driver::snapshot().await {
            if s.submitting == Some(true) {
                let _ = driver::stop().await;
                let _ = client
                    .send_message(
                        chat_id,
                        "⏱ 轮次超过时长上限仍在运行,已发送停止以释放远程控制(可重发指令)。",
                    )
                    .await;
            }
        }
    }
    if last_sent.is_empty() {
        if let Some(mid) = msg_id {
            if let Err(e) = client
                .edit_message_text(chat_id, mid, "（本轮无文本输出或超时,可发 /status 查看)")
                .await
            {
                tracing::warn!(chat_id, "[RemoteControl] 收尾编辑失败: {e}");
            }
        }
    }
}

/// 截断到 Telegram 单条上限(按字符,UTF-8 安全)。
fn truncate_tg(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= TG_MAX_CHARS {
        return s.to_owned();
    }
    let head: String = s.chars().take(TG_MAX_CHARS).collect();
    format!("{head}\n…（已截断)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use telegram::{Chat, User};

    fn msg_from(id: i64, username: Option<&str>) -> Message {
        Message {
            chat: Chat { id: 100 },
            from: Some(User {
                id,
                username: username.map(str::to_owned),
            }),
            text: Some("hi".into()),
        }
    }

    #[test]
    fn authorized_by_numeric_id() {
        let allow = vec!["12345".to_string()];
        assert!(is_authorized(&msg_from(12345, None), &allow));
        assert!(!is_authorized(&msg_from(99999, None), &allow));
    }

    #[test]
    fn authorized_by_username_case_insensitive() {
        let allow = vec!["Alice".to_string()];
        assert!(is_authorized(&msg_from(1, Some("alice")), &allow));
        assert!(is_authorized(&msg_from(1, Some("ALICE")), &allow));
        assert!(!is_authorized(&msg_from(1, Some("bob")), &allow));
    }

    #[test]
    fn unauthorized_when_no_from() {
        let allow = vec!["1".to_string()];
        let mut m = msg_from(1, None);
        m.from = None;
        assert!(!is_authorized(&m, &allow));
    }

    #[test]
    fn truncate_keeps_short() {
        assert_eq!(truncate_tg("  hi  "), "hi");
    }

    #[test]
    fn truncate_caps_long() {
        let long = "x".repeat(5000);
        let out = truncate_tg(&long);
        assert!(out.chars().count() <= TG_MAX_CHARS + 12);
        assert!(out.ends_with("已截断)"));
    }
}
