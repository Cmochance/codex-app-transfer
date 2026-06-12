//! MOC-219: preamble fallback 注入的纯函数 + 跨轮工具记忆。
//!
//! Codex Desktop 26.609 把完成态 reasoning 从对话流渲染中产品级移除(解包实证:
//! exploration 组内 `Lv` 对非 exec item 返 null + 独立 entry `B=null` 双路拦截,
//! 不分 provider、无设置可恢复),工具轮之间唯一持久可见的文本通道是 assistant
//! message。第三方模型连续工具轮常不吐 message(MiMo 真机 trace:10 轮连续
//! `reasoning + function_call` 零 message)→ UI 全折叠成一条 toolActivitySummary。
//!
//! 本模块为 chat 路径 converter 提供三个纯构件:
//! 1. [`tool_family`] — 工具名 → 折叠族。同族连续工具轮折叠(不注入、丢弃模型
//!    偶发的碎 message),异族边界注入 reasoning 转述,UI 形态对齐 gemini 系
//!    模型「每个工具组之间一句 preamble」的自然行为。
//! 2. [`PreambleToolMemory`] — `response_id → 本轮工具族` 跨轮记忆。Codex 是
//!    stateful 增量请求模式(工具轮 input 只有 `*_output`,历史靠
//!    `previous_response_id` 链),「上一轮调了什么工具」只能靠跨轮内存,从
//!    input 读不到(MOC-219 真机 trace 实证,PR #452 第一版因此每轮误注入)。
//! 3. [`select_preamble_text`] — 注入文本截取:短全取,长按段落累积,上限内
//!    char 边界截断。
//!
//! 纯内存、不持久化:记忆丢失(重启/容量逐出)的最坏后果 = 异族判定多注入一条
//! 思考转述,无害降级,不值得动 sessions.db schema。

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

/// 注入文本上限(chars)。用户拍板:短全取;长按段落累积 ≤300;单段超限截断加 `…`。
const PREAMBLE_MAX_CHARS: usize = 300;

/// 跨轮记忆容量(响应条数)。FIFO 逐出;512 轮远超单会话工具轮跨度,
/// 逐出只可能发生在多会话长时间并行后,且后果仅为多注入一条(无害)。
const MEMORY_CAPACITY: usize = 512;

/// 工具名 → 折叠族。同族 = 连续调用视为同一段工作、折叠不注入。
///
/// 归族宽松(用户拍板:`run`/`exec_command`/`shell` 一族、search 一族),但只收
/// Codex 生态已知 name —— MCP/namespace 工具名形态自由,误归族会把异类工具错误
/// 折叠(丢可见文本),按 name 原样自成一族最保守。
pub fn tool_family(name: &str) -> &str {
    match name {
        // 首帧无 name(罕见 provider chunking):空族永不匹配 → 保守走注入分支
        "" => "",
        "exec_command" | "shell" | "shell_command" | "run" | "run_command" | "bash"
        | "execute_command" => "exec",
        "tool_search" => "tool_search",
        // redirect 前的 legacy MCP discovery 名与 tool_search 同族(converter 在
        // open 时已把它们 redirect 成 tool_search,这里冗余覆盖防调用方传 raw name;
        // 共享 converter 的 const,避免两处字面量漂移)
        n if super::converter::REDIRECT_TO_TOOL_SEARCH_NAMES.contains(&n) => "tool_search",
        // web_search 与 web_fetch 同族:同属网络调研,Codex 自身的
        // toolActivitySummary 折叠条也把两者混合统计
        "web_search" | "web_fetch" => "web",
        "apply_patch" => "apply_patch",
        other => other,
    }
}

/// stateless 客户端 fallback(MOC-219 review I1):无 `previous_response_id`(或
/// recall miss)时,从请求 `input` 尾部往前扫「最近一段连续工具 item」推上一轮
/// 工具族。stateless 形态完整 transcript 在 input 里,信息可得;stateful 增量轮
/// input 只有 `*_output`(无 name)→ 扫不出 → None,回到「无记录 → 异族」分支,
/// 与跨轮记忆 miss 的行为一致。任何 message(user 边界 / assistant 可见文本)
/// 截断 —— 工具段定义是「自上次可见文本以来」。
pub fn families_from_input_tail(input: &Value) -> Option<Vec<String>> {
    let items = input.as_array()?;
    let mut families: Vec<String> = Vec::new();
    for item in items.iter().rev() {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        match item_type {
            "function_call" | "custom_tool_call" => {
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    let family = tool_family(name);
                    if !family.is_empty() && !families.iter().any(|f| f == family) {
                        families.push(family.to_owned());
                    }
                }
            }
            // tool_search_call 结构性无 name(arguments object 形态)
            "tool_search_call" => {
                if !families.iter().any(|f| f == "tool_search") {
                    families.push("tool_search".to_owned());
                }
            }
            // 工具输出 / reasoning 不断段,继续往前扫
            "function_call_output"
            | "custom_tool_call_output"
            | "tool_search_output"
            | "reasoning" => {}
            _ => break,
        }
    }
    if families.is_empty() {
        None
    } else {
        Some(families)
    }
}

#[derive(Debug, Default)]
struct MemoryInner {
    map: HashMap<String, Vec<String>>,
    /// FIFO 逐出顺序(插入序)。recall 不 bump —— 容量上限只为防无界增长,
    /// 不需要真 LRU 精度。
    order: VecDeque<String>,
}

/// `response_id → 本轮调用过的工具族(去重)` 的进程内记忆。
#[derive(Debug, Default)]
pub struct PreambleToolMemory {
    inner: Mutex<MemoryInner>,
}

impl PreambleToolMemory {
    /// 流结束时记忆本轮工具族。`families` 空时不记(recall None 与 Some(空) 在
    /// 判定上等价,都走「异族」分支)。
    pub fn remember(&self, response_id: &str, families: Vec<String>) {
        if response_id.trim().is_empty() || families.is_empty() {
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
        inner.map.insert(response_id.to_owned(), families);
    }

    /// 下一轮流内用请求的 `previous_response_id` 取回上一轮工具族。
    pub fn recall(&self, response_id: &str) -> Option<Vec<String>> {
        if response_id.trim().is_empty() {
            return None;
        }
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .get(response_id)
            .cloned()
    }
}

pub fn global_preamble_tool_memory() -> &'static PreambleToolMemory {
    static MEMORY: OnceLock<PreambleToolMemory> = OnceLock::new();
    MEMORY.get_or_init(PreambleToolMemory::default)
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

    #[test]
    fn tool_family_groups_exec_aliases() {
        assert_eq!(tool_family("exec_command"), "exec");
        assert_eq!(tool_family("shell"), "exec");
        assert_eq!(tool_family("run_command"), "exec");
    }

    #[test]
    fn tool_family_groups_web_search_and_fetch() {
        assert_eq!(tool_family("web_search"), "web");
        assert_eq!(tool_family("web_fetch"), "web");
    }

    #[test]
    fn tool_family_unknown_name_is_its_own_family() {
        assert_eq!(tool_family("my_mcp_tool"), "my_mcp_tool");
    }

    #[test]
    fn tool_family_empty_name_never_matches() {
        assert_eq!(tool_family(""), "");
        // 空族与任何记忆值都不相等(记忆侧 families 非空才记)
    }

    #[test]
    fn memory_remember_recall_roundtrip() {
        let m = PreambleToolMemory::default();
        m.remember("resp_a", vec!["exec".into()]);
        assert_eq!(m.recall("resp_a"), Some(vec!["exec".to_owned()]));
        assert_eq!(m.recall("resp_b"), None);
    }

    #[test]
    fn memory_skips_empty_families_and_blank_id() {
        let m = PreambleToolMemory::default();
        m.remember("resp_a", vec![]);
        m.remember("  ", vec!["exec".into()]);
        assert_eq!(m.recall("resp_a"), None);
        assert_eq!(m.recall("  "), None);
    }

    #[test]
    fn memory_evicts_oldest_beyond_capacity() {
        let m = PreambleToolMemory::default();
        for i in 0..(MEMORY_CAPACITY + 10) {
            m.remember(&format!("resp_{i}"), vec!["exec".into()]);
        }
        assert_eq!(m.recall("resp_0"), None);
        assert!(m.recall(&format!("resp_{}", MEMORY_CAPACITY + 9)).is_some());
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
