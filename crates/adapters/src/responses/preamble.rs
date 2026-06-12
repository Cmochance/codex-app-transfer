//! MOC-219: preamble fallback 注入的纯函数 + 跨轮静默轮数记忆。
//!
//! Codex Desktop 26.609 把完成态 reasoning 从对话流渲染中产品级移除(解包实证:
//! exploration 组内 `Lv` 对非 exec item 返 null + 独立 entry `B=null` 双路拦截,
//! 不分 provider、无设置可恢复),工具轮之间唯一持久可见的文本通道是 assistant
//! message。第三方 chat 模型连续工具轮常不吐 message → UI 全折叠无文本。
//!
//! ## 注入节奏:轮数节流(而非工具族切换)
//!
//! 第一版按「工具族切换」判定注入点,真机证伪:Codex chat 路径 wire 工具粒度是
//! **exec_command 包打一切**(读文件 cat/sed、搜索 rg、跑命令全是同一个 name,
//! renderer 靠解析命令内容才分出 Read/Searched/Ran),真实编码会话几乎全程同族
//! → 永不注入;而「同族丢弃」还把模型自己稀缺的 preamble message 吃掉(真机日志
//! dropped_chars 34-126 连续出现)。族切换信号在 Codex 场景不可用;改文件↔跑
//! 测试交替形态下它又退化成每两轮一句(旧 PR #452-455 每轮注入被废弃的同款问题)。
//!
//! 改为**纯轮数节流**:跨轮记忆「自上次可见文本以来的连续静默工具轮数」,
//! 静默满 [`INJECT_EVERY_N_ROUNDS`] 轮才注入一句 reasoning 转述;模型自己吐的
//! message 永远原样下发(proxy 不替模型删话,工具折叠是 Codex renderer 自己的
//! 职责)。任何会话形态下 UI 严格保持「最多每 N 个静默工具轮一句话」。
//!
//! Codex 是 stateful 增量请求模式(工具轮 input 只有 `*_output`,历史靠
//! `previous_response_id` 链),静默轮数只能靠跨轮内存;stateless 客户端(完整
//! transcript 进 input)用 [`silent_rounds_from_input_tail`] 从 input 尾部近似。
//!
//! 纯内存、不持久化:记忆丢失(重启/容量逐出)的最坏后果 = 当作新 task 多注入
//! 一条思考转述,无害降级,不值得动 sessions.db schema。

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

/// 注入文本上限(chars)。用户拍板:短全取;长按段落累积 ≤300;单段超限截断加 `…`。
const PREAMBLE_MAX_CHARS: usize = 300;

/// 连续静默(无可见 message)工具轮满该数注入一句。3 = 在「每轮都注入太吵」
/// (旧 PR 废弃原因)与「全程空白」之间的平衡起步值,真机体感不合适可调。
pub const INJECT_EVERY_N_ROUNDS: u32 = 3;

/// 跨轮记忆容量(响应条数)。FIFO 逐出;512 轮远超单会话工具轮跨度,
/// 逐出只可能发生在多会话长时间并行后,且后果仅为多注入一条(无害)。
const MEMORY_CAPACITY: usize = 512;

#[derive(Debug, Default)]
struct MemoryInner {
    /// response_id → 该响应结束时「自上次可见文本以来的连续静默轮数」
    /// (0 = 该轮自身有可见 message:模型原文 flush 或注入)。
    map: HashMap<String, u32>,
    /// FIFO 逐出顺序(插入序)。recall 不 bump —— 容量上限只为防无界增长,
    /// 不需要真 LRU 精度。
    order: VecDeque<String>,
}

/// `response_id → 静默轮数` 的进程内记忆。
#[derive(Debug, Default)]
pub struct PreambleToolMemory {
    inner: Mutex<MemoryInner>,
}

impl PreambleToolMemory {
    /// 流结束时记忆本轮结束后的静默轮数(本轮有可见 message 记 0,否则上轮 +1)。
    pub fn remember(&self, response_id: &str, silent_rounds: u32) {
        if response_id.trim().is_empty() {
            return;
        }
        // poisoned 不 panic 放大:记忆是无害降级数据,接着用比拒绝服务好
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.map.contains_key(response_id) {
            inner.order.push_back(response_id.to_owned());
            while inner.order.len() > MEMORY_CAPACITY {
                if let Some(evicted) = inner.order.pop_front() {
                    inner.map.remove(&evicted);
                }
            }
        }
        inner.map.insert(response_id.to_owned(), silent_rounds);
    }

    /// 下一轮流内用请求的 `previous_response_id` 取回上一轮静默轮数。
    pub fn recall(&self, response_id: &str) -> Option<u32> {
        if response_id.trim().is_empty() {
            return None;
        }
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .get(response_id)
            .copied()
    }
}

pub fn global_preamble_tool_memory() -> &'static PreambleToolMemory {
    static MEMORY: OnceLock<PreambleToolMemory> = OnceLock::new();
    MEMORY.get_or_init(PreambleToolMemory::default)
}

/// stateless 客户端 fallback:无 `previous_response_id`(或 recall miss)时,从
/// 请求 `input` 尾部往前数「最近一段连续工具 item」里的工具调用数,近似静默轮数
/// (一轮多 fc 会高估 → 提前注入,无害方向)。任何 message(user 边界 /
/// assistant 可见文本)截断 —— 静默段定义是「自上次可见文本以来」。
///
/// stateful 增量轮 input 只有 `*_output`(不计数)→ 返 None,回到「无记录」
/// 分支,与跨轮记忆 miss 的行为一致。
pub fn silent_rounds_from_input_tail(input: &Value) -> Option<u32> {
    let items = input.as_array()?;
    let mut rounds = 0u32;
    for item in items.iter().rev() {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        match item_type {
            "function_call" | "custom_tool_call" | "tool_search_call" => {
                rounds = rounds.saturating_add(1);
            }
            // 工具输出 / reasoning 不断段,继续往前扫
            "function_call_output"
            | "custom_tool_call_output"
            | "tool_search_output"
            | "reasoning" => {}
            _ => break,
        }
    }
    if rounds == 0 {
        None
    } else {
        Some(rounds)
    }
}

/// 从 reasoning 文本截取注入用 preamble:整体 ≤ [`PREAMBLE_MAX_CHARS`] 全取;
/// 超限按段落(`\n\n`)从头累积到上限;首段自身超限则 char 边界截断加 `…`。
pub fn select_preamble_text(reasoning: &str) -> String {
    let trimmed = reasoning.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // 整体短文本早退是 load-bearing 的:保留原文段落格式(下方循环路径会
    // 规范化段间空白),不要"顺手简化"掉。
    if trimmed.chars().count() <= PREAMBLE_MAX_CHARS {
        return trimmed.to_owned();
    }
    let mut out = String::new();
    let mut count = 0usize;
    for para in trimmed.split("\n\n") {
        let p = para.trim();
        if p.is_empty() {
            continue;
        }
        let pc = p.chars().count();
        if out.is_empty() {
            if pc > PREAMBLE_MAX_CHARS {
                let cut: String = p.chars().take(PREAMBLE_MAX_CHARS).collect();
                return format!("{cut}…");
            }
            out.push_str(p);
            count = pc;
            continue;
        }
        // 段间分隔按 2 chars 计
        if count + 2 + pc > PREAMBLE_MAX_CHARS {
            break;
        }
        out.push_str("\n\n");
        out.push_str(p);
        count += 2 + pc;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn memory_remember_recall_roundtrip() {
        let m = PreambleToolMemory::default();
        m.remember("resp_a", 2);
        assert_eq!(m.recall("resp_a"), Some(2));
        assert_eq!(m.recall("resp_b"), None);
    }

    #[test]
    fn memory_skips_blank_id() {
        let m = PreambleToolMemory::default();
        m.remember("  ", 1);
        assert_eq!(m.recall("  "), None);
    }

    #[test]
    fn memory_evicts_oldest_beyond_capacity() {
        let m = PreambleToolMemory::default();
        for i in 0..(MEMORY_CAPACITY + 10) {
            m.remember(&format!("resp_{i}"), 1);
        }
        assert_eq!(m.recall("resp_0"), None);
        assert!(m.recall(&format!("resp_{}", MEMORY_CAPACITY + 9)).is_some());
    }

    #[test]
    fn silent_rounds_counts_tail_tool_calls() {
        let input = json!([
            {"type": "message", "role": "assistant", "content": "visible"},
            {"type": "function_call", "name": "exec_command"},
            {"type": "function_call_output"},
            {"type": "reasoning", "summary": []},
            {"type": "function_call", "name": "exec_command"},
            {"type": "function_call_output"},
        ]);
        assert_eq!(silent_rounds_from_input_tail(&input), Some(2));
    }

    #[test]
    fn silent_rounds_none_when_tail_is_message() {
        let input = json!([
            {"type": "function_call", "name": "exec_command"},
            {"type": "function_call_output"},
            {"type": "message", "role": "user", "content": "next task"},
        ]);
        assert_eq!(silent_rounds_from_input_tail(&input), None);
    }

    #[test]
    fn silent_rounds_none_for_stateful_incremental_input() {
        // stateful 增量轮:input 只有 *_output(无 name 可数)
        let input = json!([
            {"type": "function_call_output", "call_id": "c1", "output": "ok"},
        ]);
        assert_eq!(silent_rounds_from_input_tail(&input), None);
    }

    #[test]
    fn select_short_reasoning_taken_whole() {
        assert_eq!(select_preamble_text("  short thought  "), "short thought");
    }

    #[test]
    fn select_accumulates_paragraphs_up_to_limit() {
        let p1 = "a".repeat(100);
        let p2 = "b".repeat(100);
        let p3 = "c".repeat(150); // 100+2+100+2+150 > 300 → p3 不进
        let input = format!("{p1}\n\n{p2}\n\n{p3}");
        let got = select_preamble_text(&input);
        assert_eq!(got, format!("{p1}\n\n{p2}"));
    }

    #[test]
    fn select_truncates_oversized_first_paragraph_at_char_boundary() {
        // 多字节字符验证 char 边界(不能 byte 截断 panic)
        let input = "思".repeat(400);
        let got = select_preamble_text(&input);
        assert_eq!(got.chars().count(), 301); // 300 + '…'
        assert!(got.ends_with('…'));
    }

    #[test]
    fn select_empty_input_returns_empty() {
        assert_eq!(select_preamble_text("   "), "");
    }
}
