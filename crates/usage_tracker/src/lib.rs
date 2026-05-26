//! # codex-app-transfer-usage-tracker (#279)
//!
//! 对话 token 用量统计 — 解析 Codex CLI rollout JSONL,按日 / 模型 / 会话聚合。
//!
//! ## 借鉴自 ryoppippi/ccusage (MIT)
//!
//! - 解析 + 数据类型 + paths: 见 [`vendored_ccusage`] 模块,直接 vendor 自 ccusage
//!   `rust/crates/ccusage/src/adapter/codex/{parser,types,paths}.rs` 与同 crate
//!   `types.rs` / `fast.rs` / `home.rs` / `date_utils.rs` / `utils.rs`。
//! - **本文件 loader + aggregator** 算法 1:1 对照 ccusage
//!   `rust/crates/ccusage/src/adapter/codex/{loader.rs,aggregate.rs}`,但移除 CLI 层
//!   (`SharedArgs` / `progress::track_usage_load`)+ 不做并行(本项目桌面端单 user
//!   ~250 文件串行 <1s 足够)。
//!
//! ## 对外 API
//!
//! - [`load_codex_events`] — 扫所有 `~/.codex/sessions/` 的 rollout 文件,产 events
//! - [`UsageReport`] — daily / by-model / by-conversation 三种聚合视图
//! - [`load_usage_report`] — 一站调用,推荐入口

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

pub mod vendored_ccusage;

use vendored_ccusage::codex::parser::visit_codex_session_file;
use vendored_ccusage::codex::paths::codex_usage_paths;
use vendored_ccusage::date_utils::{format_date_tz, parse_ts_timestamp, parse_tz, TimestampMs};
use vendored_ccusage::error::Result;
use vendored_ccusage::fast::FxHashSet;
use vendored_ccusage::types::CodexTokenUsageEvent;

/// 单次 event dedupe key(对照 ccusage `aggregate.rs:23-33` 的
/// `CodexEventKey = (u64, usize, TimestampMs, u64, usize, u64, u64, u64, u64, u64)`,
/// 用 session_id_hash + model_hash + ts + 5 token 字段 — 同 session 同 ts 同 token
/// counts 视为重复事件,通常是文件重复扫描或者 Codex 自身 retry 写两次)。
type CodexEventKey = (u64, u64, i64, u64, u64, u64, u64, u64);

fn event_key(event: &CodexTokenUsageEvent) -> CodexEventKey {
    use rustc_hash::FxHasher;
    use std::hash::{Hash, Hasher};
    let mut session_hasher = FxHasher::default();
    event.session_id.hash(&mut session_hasher);
    let mut model_hasher = FxHasher::default();
    event.model.hash(&mut model_hasher);
    // ccusage 对 ts 走 `parse_ts_timestamp` 提取 ms (fallback 0);本项目同款,
    // 缺 ts 字段时 dedupe 退化为按 (session, model, tokens) 比较。
    let ts_ms = parse_ts_timestamp(&event.timestamp)
        .map(TimestampMs::as_millis)
        .unwrap_or(0);
    (
        session_hasher.finish(),
        model_hasher.finish(),
        ts_ms,
        event.input_tokens,
        event.cached_input_tokens,
        event.output_tokens,
        event.reasoning_output_tokens,
        event.total_tokens,
    )
}

/// 一行 event 在多次扫描 / 多 codex_home 目录下可能重复,用 [`event_key`] dedupe
/// (对照 ccusage `aggregate.rs:24-100` 的 `seen: FxHashSet<CodexEventKey>` 思路)。
fn dedupe_events(events: &mut Vec<CodexTokenUsageEvent>) {
    let mut seen = FxHashSet::default();
    events.retain(|event| seen.insert(event_key(event)));
}

/// 扫所有 `~/.codex/sessions/` 下 *.jsonl,出全部 [`CodexTokenUsageEvent`]。
///
/// 算法对照 ccusage `loader.rs:15-32`(`load_codex_events_from_directory`):
/// 1. 列目录所有 .jsonl 文件(本 crate 自实现 [`walk`],算法对照
///    `conversation_export::list.rs:65-95` 但不依赖避免循环)
/// 2. 对每个文件用 [`visit_codex_session_file`](vendored_ccusage::codex::parser::visit_codex_session_file)
///    line-by-line memchr fast-path 解析
/// 3. dedupe
pub fn load_codex_events() -> Result<Vec<CodexTokenUsageEvent>> {
    let mut events = Vec::new();
    for sessions_dir in codex_usage_paths()? {
        load_dir(&sessions_dir, &mut events)?;
    }
    dedupe_events(&mut events);
    Ok(events)
}

fn load_dir(sessions_dir: &std::path::Path, events: &mut Vec<CodexTokenUsageEvent>) -> Result<()> {
    let files = list_jsonl_files(sessions_dir);
    for file in files {
        visit_codex_session_file(sessions_dir, &file, |event| {
            events.push(event);
            Ok(())
        })?;
    }
    Ok(())
}

/// 列出 sessions_dir 下所有 .jsonl 文件(递归)。算法对照
/// `crates/conversation_export/src/list.rs:65-95` 的 `collect_rollouts_recursively`,
/// 本 crate 不依赖 conversation_export 避免循环依赖,改用 std::fs 自实现。
fn list_jsonl_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

/// 不返回 Result — 单个子目录读失败(EACCES / 临时 IO)不应阻塞整次扫描;
/// **但**: silent-failure-hunter PR #279 review 指出需 surface 错误,这里走
/// `tracing::warn` 让 admin 日志可见。完全 silent ignore 会让 "为啥少了几天"
/// 的用户报告无法定位(目录被 chmod / 临时 IO 错都看不见)。
fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(
                dir = %dir.display(),
                error = %err,
                "usage_tracker: read_dir 失败,跳过该子目录(用户报「数据少了」时查此日志)"
            );
            return;
        }
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Aggregation — 对照 ccusage `aggregate.rs:90-260`(`aggregate_file` /
// `aggregate_files`)的 group-by 模式,但简化 kind 维度(Daily/Model/Session)
// 分三个独立函数,语义不变。
// ─────────────────────────────────────────────────────────────────────────────

/// 一行聚合后的 token + cost = 0(Phase 1 不计费,Phase 2 加 LiteLLM pricing)。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    /// group 键:date / model_name / session_id 之一
    pub group: String,
    /// 主 model(daily 视图列出本日用到的所有 model;model 视图就是该 model 名;
    /// session 视图是该会话主要 model)
    pub models: Vec<String>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    /// turn 数(events 数)
    pub turn_count: u64,
    /// last activity (RFC3339,ccusage 同款)
    pub last_activity: Option<String>,
}

impl UsageRow {
    fn add_event(&mut self, event: &CodexTokenUsageEvent) {
        self.input_tokens += event.input_tokens;
        self.cached_input_tokens += event.cached_input_tokens;
        self.output_tokens += event.output_tokens;
        self.reasoning_output_tokens += event.reasoning_output_tokens;
        // Codex CLI 5 元组没有 cache_creation/cache_read 分量,Phase 2 加 Claude
        // 时再分;Phase 1 暂存 0。
        self.total_tokens += event.total_tokens;
        self.turn_count += 1;
        if let Some(model) = event.model.as_deref() {
            if !self.models.iter().any(|m| m == model) {
                self.models.push(model.to_string());
            }
        }
        match (&self.last_activity, &event.timestamp) {
            (None, ts) => self.last_activity = Some(ts.clone()),
            (Some(prev), ts) if ts.as_str() > prev.as_str() => {
                self.last_activity = Some(ts.clone())
            }
            _ => {}
        }
    }
}

/// Daily 视图:date(localized) → UsageRow。timezone 同 ccusage `aggregate.rs:97`
/// `parse_tz(shared.timezone.as_deref()).or_else(|| Some(JiffTimeZone::system()))`。
///
/// 返回 (rows, unknown_timestamp_count) — silent-failure-hunter PR #279 修:如果上游
/// ccusage 改 ts 格式或本地 Codex CLI 输出异常,所有 event 解析 None 会全塞 "unknown"
/// 桶,UI 看不出端倪,这里把计数返出去 frontend 可显示 warning。
pub fn summarize_daily(
    events: &[CodexTokenUsageEvent],
    timezone: Option<&str>,
) -> (Vec<UsageRow>, u64) {
    let tz = parse_tz(timezone).or_else(|| Some(jiff::tz::TimeZone::system()));
    let mut groups: BTreeMap<String, UsageRow> = BTreeMap::new();
    let mut unknown_count: u64 = 0;
    for event in events {
        let date =
            match parse_ts_timestamp(&event.timestamp).map(|ts| format_date_tz(ts, tz.as_ref())) {
                Some(d) => d,
                None => {
                    unknown_count += 1;
                    "unknown".to_string()
                }
            };
        let entry = groups.entry(date.clone()).or_insert_with(|| UsageRow {
            group: date,
            ..Default::default()
        });
        entry.add_event(event);
    }
    (groups.into_values().collect(), unknown_count)
}

/// By Model 视图:model_name → UsageRow(全期累计)。
pub fn summarize_by_model(events: &[CodexTokenUsageEvent]) -> Vec<UsageRow> {
    let mut groups: BTreeMap<String, UsageRow> = BTreeMap::new();
    for event in events {
        let model = event.model.clone().unwrap_or_else(|| "unknown".to_string());
        let entry = groups.entry(model.clone()).or_insert_with(|| UsageRow {
            group: model,
            ..Default::default()
        });
        entry.add_event(event);
    }
    groups.into_values().collect()
}

/// By Conversation 视图:session_id → UsageRow。
pub fn summarize_by_conversation(events: &[CodexTokenUsageEvent]) -> Vec<UsageRow> {
    let mut groups: BTreeMap<String, UsageRow> = BTreeMap::new();
    for event in events {
        let entry = groups
            .entry(event.session_id.clone())
            .or_insert_with(|| UsageRow {
                group: event.session_id.clone(),
                ..Default::default()
            });
        entry.add_event(event);
    }
    groups.into_values().collect()
}

/// 三视图同时返回,加 Total KPI(顶部卡片用)。一次扫一次解析,出全部。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_reasoning_tokens: u64,
    pub total_tokens: u64,
    pub total_conversations: u64,
    pub total_turns: u64,
    /// 时间戳 parse 失败的 event 数(silent-failure-hunter #279 修)— 非零时
    /// frontend 应给 warning,定位为"上游 ts 格式 drift / Codex CLI 异常"。
    pub unknown_timestamp_events: u64,
    pub daily: Vec<UsageRow>,
    pub by_model: Vec<UsageRow>,
    pub by_conversation: Vec<UsageRow>,
}

/// 一站调用 — 推荐 admin handler 入口。
pub fn load_usage_report(timezone: Option<&str>) -> Result<UsageReport> {
    let events = load_codex_events()?;
    let (daily, unknown_timestamp_events) = summarize_daily(&events, timezone);
    let mut report = UsageReport {
        daily,
        by_model: summarize_by_model(&events),
        by_conversation: summarize_by_conversation(&events),
        unknown_timestamp_events,
        ..Default::default()
    };
    for event in &events {
        report.total_input_tokens += event.input_tokens;
        report.total_output_tokens += event.output_tokens;
        report.total_reasoning_tokens += event.reasoning_output_tokens;
        report.total_tokens += event.total_tokens;
        report.total_turns += 1;
    }
    report.total_conversations = report.by_conversation.len() as u64;
    Ok(report)
}
