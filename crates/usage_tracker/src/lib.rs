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

/// [MOC-204 Phase 2] 当前会话(= 最新 rollout 文件)的累计用量。供 quota injector 即时
/// 显示「累计 token / 缓存命中率」—— 直接读 Codex rollout(ground truth、含**全部历史
/// 轮次**,compact 也已正确计入),不需发新对话。只解析最新一个文件(非全量扫),按
/// (path, mtime) 缓存避免每 tick 重解析。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionTotals {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
}

impl SessionTotals {
    /// 平均缓存命中率 % = `cached_input / input`(对齐前端 `cacheHitPct` / #304);input=0→None。
    pub fn cache_hit_percent(&self) -> Option<f64> {
        if self.input_tokens == 0 {
            None
        } else {
            Some(self.cached_input_tokens as f64 / self.input_tokens as f64 * 100.0)
        }
    }
}

static NEWEST_TOTALS_CACHE: std::sync::OnceLock<
    std::sync::Mutex<Option<(PathBuf, std::time::SystemTime, SessionTotals)>>,
> = std::sync::OnceLock::new();

/// 找最新 rollout 文件(按 mtime)解析得当前会话累计。无 rollout → None。
///
/// **已知限制(review P2)**:按 newest-mtime 选文件,假设「最近写入的 rollout = 当前活动
/// 会话」——常态成立(用户正在用的会话其 rollout 持续被追加,mtime 最新)。但用户切回**旧**
/// 对话且未发新轮就开面板时,旧对话 mtime 可能比另一近期对话旧 → 选错文件,面板累计 / 缓存
/// 短暂显示另一会话的值(在该旧对话发一轮即自愈)。彻底修需把活动会话 id 从 Codex fiber 打通
/// 到本 daemon(当前 daemon→JS 单向推送、无此通道),属设计取舍,留作 followup。
pub fn newest_session_totals() -> Option<SessionTotals> {
    let dirs = codex_usage_paths().ok()?;
    let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
    for dir in &dirs {
        let mut files = Vec::new();
        walk(dir, &mut files);
        for f in files {
            if let Ok(mt) = std::fs::metadata(&f).and_then(|m| m.modified()) {
                if newest.as_ref().is_none_or(|(_, best)| mt > *best) {
                    newest = Some((f, mt));
                }
            }
        }
    }
    let (path, mtime) = newest?;
    // cache key 用完整 SystemTime(保留亚秒精度):同一文件系统秒内 Codex 又追加 usage
    // event 时 mtime 仍变(纳秒级),避免截断到秒后命中旧 cache、漏掉本秒最新一轮累计/
    // 缓存率直到下一秒才有写入(review P2)。
    let cache = NEWEST_TOTALS_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Some((p, mt, totals)) = cache.lock().unwrap().as_ref() {
        if p == &path && *mt == mtime {
            return Some(*totals); // 文件没变,复用解析结果
        }
    }
    // 收集 → dedupe → sum:Codex 会 retry / 重复写同一 token-count event(同 session+ts+
    // tokens),直接累加会双计 total/input/cache → 缓存命中率算错。复用 load_codex_events
    // 同款 dedupe_events(event_key 去重)保持一致(review P2)。
    let mut events: Vec<CodexTokenUsageEvent> = Vec::new();
    let sessions_dir = path.parent().unwrap_or(path.as_path());
    let _ = visit_codex_session_file(sessions_dir, &path, |event| {
        events.push(event);
        Ok(())
    });
    dedupe_events(&mut events);
    let mut totals = SessionTotals::default();
    for event in &events {
        totals.total_tokens = totals.total_tokens.saturating_add(event.total_tokens);
        totals.input_tokens = totals.input_tokens.saturating_add(event.input_tokens);
        totals.cached_input_tokens = totals
            .cached_input_tokens
            .saturating_add(event.cached_input_tokens);
    }
    *cache.lock().unwrap() = Some((path, mtime, totals));
    Some(totals)
}

fn load_dir(sessions_dir: &std::path::Path, events: &mut Vec<CodexTokenUsageEvent>) -> Result<()> {
    let files = list_jsonl_files(sessions_dir);
    for file in files {
        // [MOC-19 ⑤] vendored `parser.rs` 对文件级 open/read 失败吞成 `Ok(())`(保持
        // ccusage 1:1、不改 vendor),caller 因此拿不到 → 用户报「数据少了几天」时无从定位。
        // 在 caller 层先探测 open:失败走 `tracing::warn!` 记 (file, error) 再 skip,跟目录级
        // `walk()` 的 read_dir warn 对称。探测成功后 visit 内仍会再 open 一次(open 成本可
        // 忽略;TOCTOU 窗口极小且 visit 内 `Ok(())` 兜底,非致命)。
        if let Err(err) = std::fs::File::open(&file) {
            tracing::warn!(
                file = %file.display(),
                error = %err,
                "usage_tracker: 打开 rollout 文件失败,跳过该文件(用户报「数据少了」时查此日志)"
            );
            continue;
        }
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
    /// 人类可读对话名(Codex `session_index.jsonl` 的 `thread_name`)。仅
    /// by_conversation 行填充;daily/model 行为 None。前端用它替代 rollout 路径显示。
    pub display_name: Option<String>,
    /// 真实上游模型(proxy 写的 `session-models.jsonl`,见 forward.rs)。仅
    /// by_conversation 行、且本版本之后跑过的对话有;否则 None,前端回退 rollout 模型名。
    pub upstream_model: Option<String>,
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
        // [MOC-19 ②] last_activity 取最晚 event:按解析后的 epoch ms 比较,不用 raw 字符串
        // lex compare。Codex CLI 自身输出全 UTC `Z` 实际不踩,但 RFC3339 mixed offset
        // (如 `+08:00` vs `Z`)的字符串序 != 时间序时会选错最晚活动。parse 失败 → 0(不更新)。
        match (&self.last_activity, &event.timestamp) {
            (None, ts) => self.last_activity = Some(ts.clone()),
            (Some(prev), ts) => {
                let prev_ms = parse_ts_timestamp(prev)
                    .map(TimestampMs::as_millis)
                    .unwrap_or(0);
                let ts_ms = parse_ts_timestamp(ts)
                    .map(TimestampMs::as_millis)
                    .unwrap_or(0);
                if ts_ms > prev_ms {
                    self.last_activity = Some(ts.clone());
                }
            }
        }
    }
}

/// [MOC-19 ④] `last_activity` raw RFC3339(Codex 写 UTC `Z`)→ 用户 tz 的
/// `YYYY-MM-DD HH:MM` 显示串。此前 `last_activity` 全程是 raw UTC 字符串、前端只截取前
/// 16 字符 → 表格显示的是 **UTC** 时间而非用户本地时间(跟 daily date 已按 tz format 不
/// 一致)。统一在后端按用户 tz format(复用 daily date 同款 jiff `to_zoned` 路径),前端直接
/// 显示。parse 失败 → None(调用方保留 raw,不致丢信息)。
fn format_last_activity_tz(raw: &str, tz: Option<&jiff::tz::TimeZone>) -> Option<String> {
    let ms = parse_ts_timestamp(raw)?.as_millis();
    let ts = jiff::Timestamp::from_millisecond(ms).ok()?;
    let tz = tz.cloned().unwrap_or_else(jiff::tz::TimeZone::system);
    let zoned = ts.to_zoned(tz);
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        i32::from(zoned.year()),
        u32::from(zoned.month() as u8),
        u32::from(zoned.day() as u8),
        u32::from(zoned.hour() as u8),
        u32::from(zoned.minute() as u8),
    ))
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
    // by_conversation 行用 session uuid 关联两份本地旁路数据:
    // - session_index.jsonl 的 thread_name → 人类可读对话名(替代 rollout 路径);
    // - session-models.jsonl 的真实上游模型 → 替代 Codex 客户端占位名(gpt-5.x)。
    let mut by_conversation = summarize_by_conversation(&events);
    let titles = read_session_index_titles();
    let upstream_models = read_session_upstream_models();
    if !titles.is_empty() || !upstream_models.is_empty() {
        for row in &mut by_conversation {
            if let Some(uuid) = session_uuid_from_group(&row.group) {
                if let Some(name) = titles.get(&uuid) {
                    row.display_name = Some(name.clone());
                }
                if let Some(model) = upstream_models.get(&uuid) {
                    row.upstream_model = Some(model.clone());
                }
            }
        }
    }
    let mut report = UsageReport {
        daily,
        by_model: summarize_by_model(&events),
        by_conversation,
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
    // [MOC-19 ④] 三视图的 last_activity raw UTC → 用户 tz 显示串(统一一处 format,避免
    // 前端按 UTC 显示)。daily 视图的 date 已按 tz,这里把 last_activity 列也对齐 tz。
    let tz = parse_tz(timezone);
    for row in report
        .daily
        .iter_mut()
        .chain(report.by_model.iter_mut())
        .chain(report.by_conversation.iter_mut())
    {
        if let Some(raw) = row.last_activity.as_deref() {
            if let Some(formatted) = format_last_activity_tz(raw, tz.as_ref()) {
                row.last_activity = Some(formatted);
            }
        }
    }
    Ok(report)
}

/// `session_index.jsonl` 一行(Codex Desktop 写):`id`(uuid)→ `thread_name`(标题)。
#[derive(serde::Deserialize)]
struct SessionIndexLine {
    id: String,
    #[serde(default)]
    thread_name: Option<String>,
}

/// 读各 codex_home 下 `session_index.jsonl`,合并 `{ uuid → thread_name }`。缺文件 /
/// parse 失败 → 跳过(降级)。借鉴 `conversation_export::list`(本 crate 不依赖它避免
/// 循环依赖,故就地小实现)。
fn read_session_index_titles() -> BTreeMap<String, String> {
    use std::io::{BufRead, BufReader};
    let mut out = BTreeMap::new();
    let Ok(dirs) = codex_usage_paths() else {
        return out;
    };
    for sessions_dir in dirs {
        let Some(home) = sessions_dir.parent() else {
            continue;
        };
        let Ok(file) = std::fs::File::open(home.join("session_index.jsonl")) else {
            continue;
        };
        for line in BufReader::new(file)
            .lines()
            .map_while(std::result::Result::ok)
        {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(parsed) = serde_json::from_str::<SessionIndexLine>(trimmed) {
                if let Some(name) = parsed.thread_name.filter(|s| !s.trim().is_empty()) {
                    out.insert(parsed.id, name);
                }
            }
        }
    }
    out
}

/// 从 by_conversation 的 `group`(rollout 相对路径,如
/// `2026/04/28/rollout-2026-04-28T00-11-10-019dcfb5-7925-7a63-8b9d-d6afcaf9212e`)
/// 取末尾 session uuid(末 5 个 `-` 分段),用于查 session_index 标题。
fn session_uuid_from_group(group: &str) -> Option<String> {
    let file = group.rsplit('/').next().unwrap_or(group);
    let parts: Vec<&str> = file.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    Some(parts[parts.len() - 5..].join("-"))
}

/// `session-models.jsonl` 一行(proxy 写,见 forward.rs):`id`(session uuid)→ `model`(真实上游模型)。
#[derive(serde::Deserialize)]
struct SessionModelLine {
    id: String,
    model: String,
}

/// 读 `~/.codex-app-transfer/session-models.jsonl`,返回 `{ session_uuid → 真实上游模型 }`。
/// 同 id 取**最后一条**(append-only,模型若中途变以最新为准)。缺文件 / parse 失败 → 跳过。
fn read_session_upstream_models() -> BTreeMap<String, String> {
    use std::io::{BufRead, BufReader};
    let mut out = BTreeMap::new();
    let Some(home) = vendored_ccusage::home::home_dir() else {
        return out;
    };
    let path = home.join(".codex-app-transfer/session-models.jsonl");
    let Ok(file) = std::fs::File::open(&path) else {
        return out;
    };
    for line in BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<SessionModelLine>(trimmed) {
            if !parsed.model.trim().is_empty() {
                out.insert(parsed.id, parsed.model); // 后写覆盖 → 取最后一条
            }
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// 单对话逐轮缓存命中(#304)— Usage tab 点击命中率数字弹窗用。
// ─────────────────────────────────────────────────────────────────────────────

/// 直方图一根柱:某对话内一段连续轮次(`turn_start..=turn_end`,1-based)的
/// token 加权缓存命中。命中率 = `cached_input_tokens / input_tokens`(前端算)。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheBucket {
    pub turn_start: usize,
    pub turn_end: usize,
    pub cached_input_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// 把逐轮 `(cached_input, input, output)` 序列分成**至多 `max_buckets` 根柱**:
/// - 轮数 ≤ max:一轮一柱;
/// - 轮数 > max:等分成 max 桶,桶大小 `floor(n/max)` 或 `+1`,**余数分给靠后的桶**
///   (从后往前递加,见 #304);每桶 token 加权(cached / input / output 各自求和)。
fn bucket_series(points: &[(u64, u64, u64)], max_buckets: usize) -> Vec<CacheBucket> {
    let n = points.len();
    if n == 0 || max_buckets == 0 {
        return Vec::new();
    }
    let k = n.min(max_buckets);
    let base = n / k;
    let rem = n % k; // 最后 rem 个桶各 +1(余数从后往前递加)
    let mut out = Vec::with_capacity(k);
    let mut idx = 0usize;
    for i in 0..k {
        let size = base + usize::from(i >= k - rem);
        let slice = &points[idx..idx + size];
        out.push(CacheBucket {
            turn_start: idx + 1,
            turn_end: idx + size,
            cached_input_tokens: slice.iter().map(|p| p.0).sum(),
            input_tokens: slice.iter().map(|p| p.1).sum(),
            output_tokens: slice.iter().map(|p| p.2).sum(),
        });
        idx += size;
    }
    out
}

/// 某对话(`session_id`)逐轮缓存命中,分桶成 ≤10 根柱供前端直方图(#304)。
/// 按需调用(点击命中率数字时);复用全量解析后按 session 过滤 + 按 timestamp 升序。
pub fn cache_series_for_conversation(session_id: &str) -> Result<Vec<CacheBucket>> {
    let mut events: Vec<CodexTokenUsageEvent> = load_codex_events()?
        .into_iter()
        .filter(|e| e.session_id == session_id)
        .collect();
    // Codex rollout 同 session 内 ts 单调,字符串比较即时序(对照本文件 last_activity)。
    events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    let points: Vec<(u64, u64, u64)> = events
        .iter()
        .map(|e| {
            (
                e.cached_input_tokens.min(e.input_tokens),
                e.input_tokens,
                e.output_tokens,
            )
        })
        .collect();
    Ok(bucket_series(&points, 10))
}

#[cfg(test)]
mod usage_phase2_tests {
    use super::*;
    use vendored_ccusage::types::CodexTokenUsageEvent;

    fn ev(ts: &str) -> CodexTokenUsageEvent {
        CodexTokenUsageEvent {
            session_id: "s".into(),
            timestamp: ts.into(),
            model: None,
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 0,
            is_fallback_model: false,
        }
    }

    #[test]
    fn last_activity_picks_latest_by_epoch_not_lex() {
        // [MOC-19 ②] mixed offset:`10:00:00+08:00`(=02:00 UTC)vs `05:00:00Z`(=05:00 UTC)。
        // 字符串 lex compare 会误判前者晚("T10:…" > "T05:…");按 epoch 后者才是最晚活动。
        let mut row = UsageRow::default();
        row.add_event(&ev("2026-06-12T10:00:00+08:00"));
        row.add_event(&ev("2026-06-12T05:00:00Z"));
        assert_eq!(
            row.last_activity.as_deref(),
            Some("2026-06-12T05:00:00Z"),
            "应按 epoch 选最晚(05:00Z > 10:00+08=02:00Z),非字符串 lex"
        );
    }

    #[test]
    fn last_activity_order_independent() {
        // 反向加入顺序也对:先加晚的(Z),再加早的(+08)不该覆盖。
        let mut row = UsageRow::default();
        row.add_event(&ev("2026-06-12T05:00:00Z"));
        row.add_event(&ev("2026-06-12T10:00:00+08:00"));
        assert_eq!(row.last_activity.as_deref(), Some("2026-06-12T05:00:00Z"));
    }

    #[test]
    fn format_last_activity_tz_converts_utc_to_user_tz() {
        // [MOC-19 ④] 00:30 UTC → 08:30 Shanghai(+08)
        let tz = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap();
        assert_eq!(
            format_last_activity_tz("2026-06-12T00:30:00Z", Some(&tz)).as_deref(),
            Some("2026-06-12 08:30")
        );
    }

    #[test]
    fn format_last_activity_tz_crosses_date_boundary() {
        // 23:30 UTC → 次日 07:30 Shanghai(跨日,验证不是只截 HH:MM 而是真转 tz)
        let tz = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap();
        assert_eq!(
            format_last_activity_tz("2026-06-12T23:30:00Z", Some(&tz)).as_deref(),
            Some("2026-06-13 07:30")
        );
    }

    #[test]
    fn format_last_activity_tz_invalid_returns_none() {
        let tz = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap();
        assert_eq!(format_last_activity_tz("not-a-timestamp", Some(&tz)), None);
    }
}

#[cfg(test)]
mod session_totals_tests {
    use super::*;

    #[test]
    fn cache_hit_percent_matches_cached_over_input() {
        let t = SessionTotals {
            total_tokens: 3000,
            input_tokens: 2500,
            cached_input_tokens: 1500,
        };
        assert_eq!(t.cache_hit_percent(), Some(60.0)); // 1500/2500
    }

    #[test]
    fn cache_hit_percent_none_when_no_input() {
        assert_eq!(SessionTotals::default().cache_hit_percent(), None);
    }
}

#[cfg(test)]
mod cache_series_tests {
    use super::*;

    #[test]
    fn session_uuid_extracted_from_group_path() {
        assert_eq!(
            session_uuid_from_group(
                "2026/04/28/rollout-2026-04-28T00-11-10-019dcfb5-7925-7a63-8b9d-d6afcaf9212e"
            )
            .as_deref(),
            Some("019dcfb5-7925-7a63-8b9d-d6afcaf9212e")
        );
        assert_eq!(session_uuid_from_group("short").as_deref(), None);
    }

    #[test]
    fn one_bucket_per_turn_when_le_max() {
        let pts = vec![(10, 100, 5), (50, 100, 8), (90, 100, 3)];
        let b = bucket_series(&pts, 10);
        assert_eq!(b.len(), 3);
        assert_eq!(
            b[0],
            CacheBucket {
                turn_start: 1,
                turn_end: 1,
                cached_input_tokens: 10,
                input_tokens: 100,
                output_tokens: 5
            }
        );
        assert_eq!(b[2].turn_start, 3);
        assert_eq!(b[2].turn_end, 3);
    }

    #[test]
    fn even_split_remainder_to_back() {
        // 23 轮 → 10 桶:base=2, rem=3 → 前 7 桶 size2,后 3 桶 size3
        let pts: Vec<(u64, u64, u64)> = (0..23).map(|_| (1u64, 2u64, 1u64)).collect();
        let b = bucket_series(&pts, 10);
        assert_eq!(b.len(), 10);
        let sizes: Vec<usize> = b.iter().map(|x| x.turn_end - x.turn_start + 1).collect();
        assert_eq!(sizes, vec![2, 2, 2, 2, 2, 2, 2, 3, 3, 3]);
        assert_eq!(b[0].turn_start, 1);
        assert_eq!(b.last().unwrap().turn_end, 23);
    }

    #[test]
    fn token_weighted_within_bucket() {
        // turn1 0%(0/100)+ turn2 100%(100/100)合并 → 100/200 = 50%
        let pts = vec![(0, 100, 10), (100, 100, 20)];
        let b = bucket_series(&pts, 1);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].cached_input_tokens, 100);
        assert_eq!(b[0].input_tokens, 200);
        assert_eq!(b[0].output_tokens, 30);
    }

    #[test]
    fn empty_series_no_buckets() {
        assert!(bucket_series(&[], 10).is_empty());
    }

    #[test]
    fn buckets_are_contiguous_and_cover_all() {
        let pts: Vec<(u64, u64, u64)> = (0..47).map(|i| (i, 100, 0)).collect();
        let b = bucket_series(&pts, 10);
        assert_eq!(b[0].turn_start, 1);
        for w in b.windows(2) {
            assert_eq!(w[1].turn_start, w[0].turn_end + 1);
        }
        assert_eq!(b.last().unwrap().turn_end, 47);
    }
}
