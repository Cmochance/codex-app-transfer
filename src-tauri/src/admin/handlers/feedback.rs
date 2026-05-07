//! `/api/feedback` —— 反馈提交 + 节流 + 附件打包.

use std::fs;
use std::path::Path as FsPath;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use codex_app_transfer_proxy::proxy_log_dir;
use reqwest::{header::CONTENT_TYPE, multipart};
use serde_json::{json, Value};

use super::super::registry_io::load as load_registry;
use super::common::{active_provider_name, current_epoch_secs, err, APP_VERSION};

pub(super) const FEEDBACK_WORKER_URL: &str =
    "https://codex-app-transfer-feedback.alysechencn.workers.dev";

pub(super) struct FeedbackThrottleState {
    pub(super) last_success: Option<Instant>,
    pub(super) failure_ts: Vec<Instant>,
    pub(super) failure_cooldown_until: Option<Instant>,
}

pub(super) struct FeedbackThrottle {
    pub(super) inner: Mutex<FeedbackThrottleState>,
}

impl FeedbackThrottle {
    pub(super) const SUCCESS_COOLDOWN: Duration = Duration::from_secs(60);
    pub(super) const FAILURE_WINDOW: Duration = Duration::from_secs(300);
    pub(super) const FAILURE_LIMIT: usize = 5;
    pub(super) const FAILURE_COOLDOWN: Duration = Duration::from_secs(60);

    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(FeedbackThrottleState {
                last_success: None,
                failure_ts: Vec::new(),
                failure_cooldown_until: None,
            }),
        }
    }

    pub(super) fn acquire(&self) -> Result<(), String> {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();

        if let Some(last_success) = inner.last_success {
            let elapsed = now.saturating_duration_since(last_success);
            if elapsed < Self::SUCCESS_COOLDOWN {
                let wait = Self::SUCCESS_COOLDOWN.saturating_sub(elapsed).as_secs();
                return Err(format!("刚提交成功,请等 {wait} 秒后再发新反馈"));
            }
        }

        if let Some(until) = inner.failure_cooldown_until {
            if now < until {
                let wait = until.saturating_duration_since(now).as_secs();
                return Err(format!("连续提交失败次数过多,请等 {wait} 秒后再试"));
            }
        }

        inner
            .failure_ts
            .retain(|ts| now.saturating_duration_since(*ts) < Self::FAILURE_WINDOW);
        Ok(())
    }

    pub(super) fn record_success(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.last_success = Some(Instant::now());
        inner.failure_ts.clear();
        inner.failure_cooldown_until = None;
    }

    pub(super) fn record_failure(&self) {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        inner
            .failure_ts
            .retain(|ts| now.saturating_duration_since(*ts) < Self::FAILURE_WINDOW);
        inner.failure_ts.push(now);
        if inner.failure_ts.len() >= Self::FAILURE_LIMIT {
            inner.failure_cooldown_until = Some(now + Self::FAILURE_COOLDOWN);
        }
    }
}

static FEEDBACK_THROTTLE: OnceLock<FeedbackThrottle> = OnceLock::new();

pub(super) fn feedback_throttle() -> &'static FeedbackThrottle {
    FEEDBACK_THROTTLE.get_or_init(FeedbackThrottle::new)
}

pub(super) fn feedback_worker_url(raw: &str) -> Result<&str, String> {
    let url = raw.trim();
    if url.is_empty() {
        Err("反馈服务未配置".to_owned())
    } else {
        Ok(url)
    }
}

pub(super) fn multipart_text_part(text: String, mime: &str) -> multipart::Part {
    multipart::Part::text(text.clone())
        .mime_str(mime)
        .unwrap_or_else(|_| multipart::Part::text(text))
}

pub(super) fn feedback_proxy_tail_content(path: &FsPath) -> Option<String> {
    let content = fs::read(path).ok()?;
    let content = String::from_utf8_lossy(&content);
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(200);
    let tail = lines[start..].join("\n");
    if tail.trim().is_empty() {
        return None;
    }
    Some(tail)
}

pub(super) fn feedback_proxy_tail_part() -> Option<multipart::Part> {
    let log_dir = proxy_log_dir()?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let path = log_dir.join(format!("proxy-{today}.log"));
    let tail = feedback_proxy_tail_content(&path)?;
    let part =
        multipart::Part::bytes(tail.into_bytes()).file_name(format!("proxy-tail-{today}.log"));
    Some(
        part.mime_str("text/plain")
            .unwrap_or_else(|_| multipart::Part::text("")),
    )
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct FeedbackAttachment {
    pub(super) field: String,
    pub(super) name: String,
    pub(super) content_type: String,
    pub(super) raw: Vec<u8>,
}

pub(super) fn feedback_attachments(input: &Value, timestamp_secs: u64) -> Vec<FeedbackAttachment> {
    let mut shot_idx = 0usize;
    let mut log_idx = 0usize;
    let mut parts = Vec::new();

    if let Some(attachments) = input.get("attachments").and_then(|v| v.as_array()) {
        for attachment in attachments {
            let Some(obj) = attachment.as_object() else {
                continue;
            };
            let content_b64 = obj
                .get("content_b64")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let Ok(raw) = STANDARD.decode(content_b64.as_bytes()) else {
                continue;
            };
            if raw.is_empty() || raw.len() > 5 * 1024 * 1024 {
                continue;
            }
            let kind = obj.get("kind").and_then(|v| v.as_str()).unwrap_or("log");
            let fallback_name = format!("{kind}-{timestamp_secs}.bin");
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .unwrap_or(&fallback_name)
                .to_owned();
            let content_type = obj
                .get("content_type")
                .and_then(|v| v.as_str())
                .filter(|v| v.contains('/'))
                .unwrap_or("application/octet-stream")
                .to_owned();
            let field = if kind == "screenshot" {
                let field = format!("screenshot{shot_idx}");
                shot_idx += 1;
                field
            } else {
                let field = format!("log{log_idx}");
                log_idx += 1;
                field
            };
            parts.push(FeedbackAttachment {
                field,
                name,
                content_type,
                raw,
            });
        }
    }

    parts
}

// ── /api/feedback ────────────────────────────────────────────────────

pub async fn submit_feedback(body: Bytes) -> Response {
    submit_feedback_with_body(body, FEEDBACK_WORKER_URL, feedback_throttle()).await
}

pub(super) async fn submit_feedback_with_body(
    body: Bytes,
    worker_url: &str,
    throttle: &FeedbackThrottle,
) -> Response {
    if let Err(reason) = throttle.acquire() {
        return err(StatusCode::TOO_MANY_REQUESTS, reason).into_response();
    }

    let input = match serde_json::from_slice::<Value>(&body) {
        Ok(input) => input,
        Err(_) => return err(StatusCode::BAD_REQUEST, "请求体非 JSON").into_response(),
    };

    let title = input
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let body_text = input
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let include_diag = input
        .get("include_diagnostics")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if body_text.is_empty() {
        return err(StatusCode::BAD_REQUEST, "请填写描述").into_response();
    }

    let worker_url = match feedback_worker_url(worker_url) {
        Ok(url) => url,
        Err(e) => {
            throttle.record_failure();
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    };

    let mut meta = json!({"app_version": APP_VERSION});
    if include_diag {
        let active_name = load_registry()
            .ok()
            .map(|cfg| active_provider_name(&cfg))
            .unwrap_or_default();
        if let Some(obj) = meta.as_object_mut() {
            obj.insert(
                "os".to_owned(),
                Value::String(std::env::consts::OS.to_owned()),
            );
            obj.insert(
                "arch".to_owned(),
                Value::String(std::env::consts::ARCH.to_owned()),
            );
            obj.insert(
                "active_provider_name".to_owned(),
                Value::String(active_name),
            );
            obj.insert("include_diagnostics".to_owned(), Value::Bool(true));
        }
    }

    let mut form = multipart::Form::new()
        .part(
            "meta",
            multipart_text_part(meta.to_string(), "application/json"),
        )
        .part("title", multipart_text_part(title, "text/plain"))
        .part("body", multipart_text_part(body_text, "text/plain"));

    for attachment in feedback_attachments(&input, current_epoch_secs()) {
        let FeedbackAttachment {
            field,
            name,
            content_type,
            raw,
        } = attachment;
        let part = multipart::Part::bytes(raw.clone()).file_name(name.clone());
        let part = part
            .mime_str(&content_type)
            .unwrap_or_else(|_| multipart::Part::bytes(raw).file_name(name));
        form = form.part(field, part);
    }

    if include_diag {
        if let Some(part) = feedback_proxy_tail_part() {
            form = form.part("log_proxy_tail", part);
        }
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            throttle.record_failure();
            return err(StatusCode::BAD_GATEWAY, format!("反馈服务暂不可用:{e}")).into_response();
        }
    };

    let response = match client.post(worker_url).multipart(form).send().await {
        Ok(response) => response,
        Err(e) => {
            throttle.record_failure();
            return err(StatusCode::BAD_GATEWAY, format!("反馈服务暂不可用:{e}")).into_response();
        }
    };
    let status = response.status();
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("application/json"))
        .unwrap_or(false);
    let data = if is_json {
        response.json::<Value>().await.unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    if !status.is_success() || data.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        throttle.record_failure();
        let status_code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let status_code = if status_code.is_client_error() || status_code.is_server_error() {
            status_code
        } else {
            StatusCode::BAD_GATEWAY
        };
        let message = data
            .get("error")
            .or_else(|| data.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("上游错误");
        return err(status_code, message).into_response();
    }

    throttle.record_success();
    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");
    Json(json!({
        "success": true,
        "id": id,
        "message": format!("反馈已收到 (ID: {id})"),
        "email_sent": data.get("email_sent").and_then(|v| v.as_bool()).unwrap_or(false),
    }))
    .into_response()
}
