use std::{collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Serialize};

// upstream: rust/crates/ccusage/src/types.rs
use super::date_utils::TimestampMs;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEntry {
    pub session_id: Option<String>,
    pub timestamp: String,
    pub version: Option<String>,
    pub message: UsageMessage,
    #[serde(rename = "costUSD")]
    pub cost_usd: Option<f64>,
    pub request_id: Option<String>,
    pub is_api_error_message: Option<bool>,
    pub is_sidechain: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsageMessage {
    pub usage: TokenUsageRaw,
    pub model: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct TokenUsageRaw {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    pub speed: Option<Speed>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    Standard,
    Fast,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCounts {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub extra_total_tokens: u64,
}

impl TokenCounts {
    pub fn add_usage(&mut self, usage: TokenUsageRaw) {
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_creation_tokens += usage.cache_creation_input_tokens;
        self.cache_read_tokens += usage.cache_read_input_tokens;
    }

    pub fn total(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_tokens
            + self.cache_read_tokens
            + self.extra_total_tokens
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelBreakdown {
    pub model_name: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    #[serde(skip_serializing)]
    pub extra_total_tokens: u64,
    pub cost: f64,
}

#[derive(Debug, Clone)]
pub struct LoadedEntry {
    pub data: UsageEntry,
    pub timestamp: TimestampMs,
    pub date: String,
    pub project: Arc<str>,
    pub session_id: Arc<str>,
    pub project_path: Arc<str>,
    pub cost: f64,
    pub extra_total_tokens: u64,
    pub credits: Option<f64>,
    pub message_count: Option<u64>,
    pub model: Option<String>,
    pub usage_limit_reset_time: Option<TimestampMs>,
}

#[derive(Debug)]
pub struct LoadedFile {
    pub timestamp: Option<TimestampMs>,
    pub entries: Vec<LoadedEntry>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodexRawUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTokenUsageEvent {
    pub session_id: String,
    pub timestamp: String,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    pub is_fallback_model: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CodexModelUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    pub is_fallback: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CodexGroup {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    pub models: BTreeMap<String, CodexModelUsage>,
    pub last_activity: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub month: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub week: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    #[serde(skip_serializing)]
    pub extra_total_tokens: u64,
    pub total_cost: f64,
    pub credits: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<u64>,
    pub models_used: Vec<String>,
    pub model_breakdowns: Vec<ModelBreakdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versions: Option<Vec<String>>,
}

impl UsageSummary {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_tokens
            + self.cache_read_tokens
            + self.extra_total_tokens
    }
}

#[derive(Debug, Clone)]
pub struct SessionBlock {
    pub id: String,
    pub start_time: TimestampMs,
    pub end_time: TimestampMs,
    pub actual_end_time: Option<TimestampMs>,
    pub is_active: bool,
    pub is_gap: bool,
    pub entries: Vec<LoadedEntry>,
    pub token_counts: TokenCounts,
    pub cost_usd: f64,
    pub models: Vec<String>,
    pub usage_limit_reset_time: Option<TimestampMs>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnRate {
    pub tokens_per_minute: f64,
    pub tokens_per_minute_for_indicator: f64,
    pub cost_per_hour: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Projection {
    pub total_tokens: u64,
    pub total_cost: f64,
    pub remaining_minutes: u64,
}
