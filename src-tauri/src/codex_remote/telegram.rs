//! Telegram Bot API 客户端(MOC-249 移动端远程控制 M1)。
//!
//! 纯 HTTPS long-poll —— 不需要 relay / 公网回调 / webhook,绕开 Codex renderer 的
//! CSP(对外连接放 Rust 侧)。只用到三个 method:`getUpdates`(收)、`sendMessage` +
//! `editMessageText`(发 + 流式编辑)。
//!
//! 端点 `https://api.telegram.org/bot<token>/<method>`,响应统一信封
//! `{ok, result?, description?}`。

use serde::Deserialize;

const API_BASE: &str = "https://api.telegram.org";

/// 一条入站更新(只关心普通文本消息;callback_query 等留 M2)。
#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Deserialize)]
struct ApiEnvelope<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct SentMessage {
    message_id: i64,
}

/// Telegram bot 客户端。`http` 的超时必须 > long-poll timeout(否则 getUpdates 自断)。
#[derive(Clone)]
pub struct TelegramClient {
    token: String,
    http: reqwest::Client,
}

impl TelegramClient {
    /// `long_poll_secs` = getUpdates 服务端挂起秒数;http 超时取其 + 10s 余量。
    pub fn new(token: impl Into<String>, long_poll_secs: u64) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(long_poll_secs + 10))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            token: token.into(),
            http,
        })
    }

    fn url(&self, method: &str) -> String {
        format!("{API_BASE}/bot{}/{method}", self.token)
    }

    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        body: serde_json::Value,
    ) -> Result<T, String> {
        let resp = self
            .http
            .post(self.url(method))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("{method} 网络错误: {e}"))?;
        let env: ApiEnvelope<T> = resp
            .json()
            .await
            .map_err(|e| format!("{method} 解析失败: {e}"))?;
        if !env.ok {
            return Err(format!(
                "{method} API 错误: {}",
                env.description.unwrap_or_default()
            ));
        }
        env.result.ok_or_else(|| format!("{method} 响应缺 result"))
    }

    /// long-poll 拉更新。`offset` = 上次最大 update_id + 1(已确认的不再下发)。
    pub async fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Update>, String> {
        self.call(
            "getUpdates",
            serde_json::json!({
                "offset": offset,
                "timeout": timeout_secs,
                "allowed_updates": ["message"],
            }),
        )
        .await
    }

    /// 发文本,返回新消息的 message_id(供后续 [`Self::edit_message_text`] 流式编辑)。
    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<i64, String> {
        let sent: SentMessage = self
            .call(
                "sendMessage",
                serde_json::json!({ "chat_id": chat_id, "text": text }),
            )
            .await?;
        Ok(sent.message_id)
    }

    /// 编辑已发消息的文本(流式回复:占位消息随 assistant 输出增量更新)。
    /// Telegram 对「内容未变」的编辑会回 400,调用方应跳过无变化的编辑。
    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> Result<(), String> {
        let _: serde_json::Value = self
            .call(
                "editMessageText",
                serde_json::json!({ "chat_id": chat_id, "message_id": message_id, "text": text }),
            )
            .await?;
        Ok(())
    }
}
