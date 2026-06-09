//! apply_patch 转换决策埋点(诊断流量查看器「apply-patch」页的数据源)。
//!
//! ## 为什么需要它(与 forward-trace 的区别)
//!
//! forward-trace 抓的是 **raw 协议体**(Codex 原始请求 / 转换后发上游 / 上游回包),
//! 看不到 adapter 内部把上游 `apply_patch` 工具调用重打包成 Codex `custom_tool_call`
//! wire 时的**中间决策**:原始 function args 长啥样、提取出的 V4A 文本、信封修复改了啥、
//! JSON/V4A 截断检测结果、V4A 后验语法校验 verdict、最终 completed/incomplete 决策。
//! 这些恰是精修 apply_patch 模块(extract / repair / validate 反复迭代)最需要盯的环节,
//! 故单列一个 [`crate::responses`] / [`crate::gemini_native`] 共用的埋点出口。
//!
//! ## 为什么用 sink 注入而非直接调 trace_store
//!
//! `trace_store` 在 `crates/proxy`,而本 crate(`adapters`)被 proxy 依赖 —— 反向 `use`
//! 会造成**循环依赖**。故这里只定义一个进程级 sink hook:proxy 启动时(`build_router`)
//! 注册一个闭包,把本模块构造的诊断 `Value` 补 `seq`/`captured_at` 后 push 进
//! `trace_store`(`TraceKind::ApplyPatch`)。沿用 cat-webfetch 子进程 `POST /api/ingest`
//! 的「外层补 seq」思路,只是这里是进程内闭包、无需跨进程。
//!
//! ## 开销 / 默认关
//!
//! gate 指向 `proxy::diagnostics::forward_trace_enabled`(env `CAS_DIAG_TRACE` 或 app 内
//! 「诊断模式」开关,默认关)。未注册 / 关时 [`emit`] 是一次 `OnceLock` load + 一次原子读,
//! **不构造任何 Value**(闭包 `build` 仅在开启时调用),与 forward-trace 同「关时零开销」契约。
//! 与 forward-trace 同定位:开发者本地诊断,patch 正文(代码)按原文记录、不脱敏,仅 loopback、
//! 默认关,绝不随 release 给终端用户开。

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

/// 单条 args/input 文本落盘上限(防个别巨型 patch 把一条诊断撑爆)。超出截断 + 标注 `truncated_bytes`。
const MAX_FIELD_BYTES: usize = 256 * 1024;

/// 「已发出、等结果回灌」的 apply_patch call_id 上限(防泄漏:若某 call 永远等不到结果,
/// 超额淘汰最旧)。稳态在飞 apply_patch 很少,512 远够。
const PENDING_CAP: usize = 512;

/// 进程级埋点 hook。`gate` 决定是否采集(指向 proxy 的诊断总开关),`sink` 把构造好的
/// 诊断 `Value` 落到 trace_store(由 proxy 注册的闭包补 seq 再 push)。
struct Hook {
    gate: fn() -> bool,
    sink: Box<dyn Fn(Value) + Send + Sync>,
}

static HOOK: OnceLock<Hook> = OnceLock::new();

/// proxy 启动时注册一次(`OnceLock`,二次调用静默忽略 —— 进程级单例)。
/// - `gate`:返回「当前是否采集诊断」,传 `proxy::diagnostics::forward_trace_enabled`。
/// - `sink`:收一条已构造的 apply_patch 诊断 `Value`(尚无 `seq`/`captured_at`),由 proxy
///   补全后 push 进 `trace_store`(`TraceKind::ApplyPatch`)。
pub fn install(gate: fn() -> bool, sink: Box<dyn Fn(Value) + Send + Sync>) {
    let _ = HOOK.set(Hook { gate, sink });
}

/// 当前是否采集 apply_patch 埋点(未注册 → false)。调用方可在构造昂贵字段前先 gate。
pub fn enabled() -> bool {
    HOOK.get().map(|h| (h.gate)()).unwrap_or(false)
}

/// 一条 apply_patch 转换决策的输入(全引用,仅在采集开启时才序列化成 `Value`)。
pub struct ApplyPatchTrace<'a> {
    /// 转换来源路径:`"chat"`(responses/converter.rs)/ `"gemini_native"`。
    pub source: &'a str,
    /// 上游模型名(converter `self.model` / gemini `self.model`),apply_patch 行为按模型分布。
    pub model: &'a str,
    /// Codex wire 的 `call_id`(关联工具结果回灌)。
    pub call_id: &'a str,
    /// Codex wire 的 item id(`fc_*`)。
    pub fc_id: &'a str,
    /// 上游回的**原始** function arguments(标准形态 `{"input":"*** Begin Patch…"}`,
    /// 也可能是裸 V4A / 别名 key / 截断残片)。
    pub args_raw: &'a str,
    /// `extract_apply_patch_input` 提取 + `repair_v4a_envelope` 修复后、真正发给 Codex 的 V4A 文本。
    pub input: &'a str,
    /// 流是否中断(chat:无 finish_reason 且非 `[DONE]`;gemini 不增量,恒 false)。
    pub interrupted: bool,
    /// JSON 结构截断检测结果(`detect_json_truncation`;gemini 路径不适用,传 None)。
    pub json_truncation: Option<&'a str>,
    /// V4A 信封截断检测结果(`detect_v4a_truncation`;gemini 路径不适用,传 None)。
    pub v4a_truncation: Option<&'a str>,
    /// V4A 后验语法校验失败(`validate_v4a_syntax`):`(行号, 人类可读消息)`。
    pub v4a_validation: Option<(usize, &'a str)>,
    /// 最终决策:`"completed"`(emit input.delta+done,写 cache)或 `"incomplete"`
    /// (emit status=incomplete,跳过 input.done,不写 cache,防破坏性半应用)。
    pub decision: &'a str,
    /// pre-flight 自动修复记录(`apply_patch_preflight::repairs_to_value` 的产物):每个
    /// `Update File` 读盘比对的结果(repaired / clean / skipped)。无修复时传 `None`。
    pub repairs: Option<&'a Value>,
}

/// 采集开启时构造诊断 `Value`(phase=`call`)并经 sink 落库;关时零开销返回。
/// completed 的 call 会**登记 call_id 到 pending**,等下一轮请求回灌结果时由 [`emit_result`]
/// 配对发射(incomplete 的 call Codex 不会执行、不会有结果,不登记)。
pub fn emit(trace: &ApplyPatchTrace) {
    let Some(hook) = HOOK.get() else { return };
    if !(hook.gate)() {
        return;
    }
    (hook.sink)(build_value(trace));
    if trace.decision == "completed" {
        register_pending(trace.call_id);
    }
}

/// 采集开启时,为一条 apply_patch **结果回灌**(Codex apply 后塞回模型的 `custom_tool_call_output`)
/// 发射 phase=`result` 诊断。`output` 是回灌原值(string 或 content_items array)。
///
/// **去重 + 精准**:请求侧每轮都重放完整历史(同一 call_id 的结果会在后续每轮请求里再次出现),
/// 故只在 call_id **首次**命中 pending(= 我们发过的 completed apply_patch call)时发射并移除;
/// 历史重放的重复结果、以及非 apply_patch 的 custom 工具结果都不会命中 → 跳过。重试是新 call_id,
/// 各自独立配对。关 / 未注册 sink 时零开销(先 gate 再查 pending)。
pub fn emit_result(call_id: &str, output: &Value) {
    let Some(hook) = HOOK.get() else { return };
    if !(hook.gate)() {
        return;
    }
    if !take_pending(call_id) {
        return;
    }
    let text = match output {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    (hook.sink)(build_result_value(call_id, &text));
}

/// 把一条 [`ApplyPatchTrace`] 构造成诊断 `Value`(viewer / jsonl 用)。`seq`/`captured_at`/
/// `proxy_version` 由 proxy 注册的 sink 补(那里能拿到 `next_seq` + 版本号)。`pub(crate)` 供测试。
pub(crate) fn build_value(t: &ApplyPatchTrace) -> Value {
    let (args_text, args_trunc) = cap_field(t.args_raw);
    let (input_text, input_trunc) = cap_field(t.input);
    let mut reasons: Vec<&str> = Vec::new();
    if t.interrupted {
        reasons.push("interrupted");
    }
    if t.json_truncation.is_some() {
        reasons.push("json_truncated");
    }
    if t.v4a_truncation.is_some() {
        reasons.push("v4a_truncated");
    }
    if t.v4a_validation.is_some() {
        reasons.push("v4a_invalid");
    }
    json!({
        "trace_kind": "apply_patch",
        "phase": "call",
        "source": t.source,
        "model": t.model,
        "call_id": t.call_id,
        "fc_id": t.fc_id,
        "decision": t.decision,
        "extraction": classify_extraction(t.args_raw, t.input),
        "incomplete_reasons": reasons,
        "repairs": t.repairs.cloned().unwrap_or(Value::Null),
        "args": {
            "len": t.args_raw.len(),
            "truncated_bytes": args_trunc,
            "raw": args_text,
        },
        "input": {
            "len": t.input.len(),
            "truncated_bytes": input_trunc,
            "v4a": input_text,
        },
        "checks": {
            "interrupted": t.interrupted,
            "json_truncation": t.json_truncation,
            "v4a_truncation": t.v4a_truncation,
            "v4a_validation": t.v4a_validation.map(|(line, message)| json!({
                "line": line,
                "message": message,
            })),
        },
    })
}

/// 把一条 apply_patch **结果回灌**构造成诊断 `Value`(phase=`result`)。`pub(crate)` 供测试。
pub(crate) fn build_result_value(call_id: &str, output: &str) -> Value {
    let (text, trunc) = cap_field(output);
    json!({
        "trace_kind": "apply_patch",
        "phase": "result",
        "call_id": call_id,
        "is_error": looks_like_error(output),
        "output": {
            "len": output.len(),
            "truncated_bytes": trunc,
            "text": text,
        },
    })
}

/// apply_patch 结果是否像失败(advisory —— viewer 仍展示全文供人判断)。匹配 Codex apply_patch
/// handler / parse_patch 常见失败措辞;成功输出通常是变更文件清单或简短 "Success"。
/// 判断 apply_patch 结果是否失败。**不能**用 `"error"`/`"context"` 等松散子串 —— 会命中
/// 文件名(`ErrorBoundary.tsx`)、代码(`asynccontextmanager`)而误报(MOC-194 真机 seq977:
/// `Exit code: 0 … Success … A …ErrorBoundary.tsx` 被误判 is_error=true)。信号优先级:
/// ① 明确失败短语(apply_patch 校验失败直接报、不带 Exit code 包装)→ ② exec 包装的
/// `Exit code: N`(非 0 = 失败)→ ③ 默认非错。
fn looks_like_error(output: &str) -> bool {
    let l = output.to_ascii_lowercase();
    const FAIL_PHRASES: [&str; 9] = [
        "apply_patch verification failed",
        "failed to find",
        "did not apply",
        "does not match",
        "invalid patch",
        "no such file or directory",
        "is not a valid hunk header",
        "update file hunk for path",
        "cannot operate on a completely empty file",
    ];
    if FAIL_PHRASES.iter().any(|m| l.contains(m)) {
        return true;
    }
    // exec 包装的 `Exit code: N` 是权威信号(成功 = 0)。
    if let Some(code) = parse_exit_code(output) {
        return code != 0;
    }
    false
}

/// 从 exec 包装的 `Exit code: N` 抽退出码(apply_patch 经 shell exec 时带此前缀)。
fn parse_exit_code(output: &str) -> Option<i32> {
    let idx = output.find("Exit code:")?;
    output[idx + "Exit code:".len()..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

// ── pending apply_patch call_id 注册表(call ↔ result 配对 + 历史重放去重)──────────
//
// completed 的 apply_patch call 登记 call_id;结果回灌首次命中即发射并移除。只用
// `Mutex<VecDeque<String>>`(每次 apply_patch 才动一次,512 内线性扫可忽略),超额淘汰最旧。

static PENDING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

fn pending() -> &'static Mutex<VecDeque<String>> {
    PENDING.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// 登记一个「等结果」的 call_id(超 [`PENDING_CAP`] 淘汰最旧)。空 id 忽略。
fn register_pending(call_id: &str) {
    if call_id.is_empty() {
        return;
    }
    if let Ok(mut q) = pending().lock() {
        // 去重:同 call_id 不重复登记(理论上 call_id 唯一,防御)。
        if q.iter().any(|x| x == call_id) {
            return;
        }
        q.push_back(call_id.to_owned());
        while q.len() > PENDING_CAP {
            q.pop_front();
        }
    }
}

/// 若 call_id 在 pending 中则移除并返回 true(= 这是我们发过的 apply_patch call 的首个结果)。
fn take_pending(call_id: &str) -> bool {
    if let Ok(mut q) = pending().lock() {
        if let Some(pos) = q.iter().position(|x| x == call_id) {
            q.remove(pos);
            return true;
        }
    }
    false
}

/// 截断到 [`MAX_FIELD_BYTES`](按 char 边界,不切坏 UTF-8),返回(文本, 丢弃字节数)。
fn cap_field(s: &str) -> (String, usize) {
    if s.len() <= MAX_FIELD_BYTES {
        return (s.to_owned(), 0);
    }
    // 找 <= cap 的 char 边界,避免切在多字节中间。
    let mut end = MAX_FIELD_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_owned(), s.len() - end)
}

/// 粗分类「V4A 是怎么从原始 args 里抽出来的」(给 viewer 摘要 / 过滤)。轻量 re-derive,
/// 与 `extract_apply_patch_input` 的实际分支对齐但不耦合其内部:
/// - `json_input`:args 是 JSON 且含 `input` 字段(标准形态)。
/// - `json_alt_key`:args 是 JSON、无 `input` 但 input 文本回收自别名 key(patch/diff/…)。
/// - `bare_v4a`:args 本身就是裸 V4A(无 JSON 包裹)。
/// - `raw_fallback`:既非合法 JSON 也不像裸 V4A → 原样透传(多半截断 / schema drift)。
pub(crate) fn classify_extraction(args_raw: &str, _input: &str) -> &'static str {
    let trimmed = args_raw.trim();
    if trimmed.is_empty() {
        return "empty";
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => {
            if v.get("input").and_then(Value::as_str).is_some() {
                "json_input"
            } else if v.is_object() {
                "json_alt_key"
            } else {
                "raw_fallback"
            }
        }
        Err(_) => {
            if trimmed.contains("*** Begin Patch") {
                "bare_v4a"
            } else {
                "raw_fallback"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample<'a>(args: &'a str, input: &'a str) -> ApplyPatchTrace<'a> {
        ApplyPatchTrace {
            source: "chat",
            model: "qwen-test",
            call_id: "call_1",
            fc_id: "fc_1",
            args_raw: args,
            input,
            interrupted: false,
            json_truncation: None,
            v4a_truncation: None,
            v4a_validation: None,
            decision: "completed",
            repairs: None,
        }
    }

    #[test]
    fn classify_covers_the_four_paths() {
        assert_eq!(
            classify_extraction(r#"{"input":"*** Begin Patch\n*** End Patch"}"#, ""),
            "json_input"
        );
        assert_eq!(
            classify_extraction(r#"{"patch":"*** Begin Patch"}"#, ""),
            "json_alt_key"
        );
        assert_eq!(
            classify_extraction("*** Begin Patch\n*** End Patch", ""),
            "bare_v4a"
        );
        assert_eq!(classify_extraction("garbage not json", ""), "raw_fallback");
        assert_eq!(classify_extraction("   ", ""), "empty");
    }

    #[test]
    fn build_value_carries_decision_and_reasons() {
        let mut t = sample(
            r#"{"input":"*** Begin Patch\n*** End Patch"}"#,
            "*** Begin Patch\n*** End Patch",
        );
        t.decision = "incomplete";
        t.interrupted = true;
        t.v4a_validation = Some((3, "expected '*** End Patch'"));
        let v = build_value(&t);
        assert_eq!(v["trace_kind"], "apply_patch");
        assert_eq!(v["decision"], "incomplete");
        assert_eq!(v["extraction"], "json_input");
        assert_eq!(v["checks"]["v4a_validation"]["line"], 3);
        let reasons = v["incomplete_reasons"].as_array().unwrap();
        assert!(reasons.iter().any(|r| r == "interrupted"));
        assert!(reasons.iter().any(|r| r == "v4a_invalid"));
    }

    #[test]
    fn build_result_value_flags_error_and_carries_output() {
        let ok = build_result_value("call_x", "Success. Updated 1 file.");
        assert_eq!(ok["phase"], "result");
        assert_eq!(ok["call_id"], "call_x");
        assert_eq!(ok["is_error"], false);
        assert_eq!(ok["output"]["text"], "Success. Updated 1 file.");

        let err = build_result_value("call_y", "error: context does not match at line 12");
        assert_eq!(err["is_error"], true);

        // 真机 seq977 回归:成功结果含文件名 ErrorBoundary.tsx,不能因 "error" 子串误报。
        let ok2 = build_result_value(
            "call_z",
            "Exit code: 0\nWall time: 0.1 seconds\nOutput:\nSuccess. Updated the following files:\nA frontend/src/components/common/ErrorBoundary.tsx\n",
        );
        assert_eq!(
            ok2["is_error"], false,
            "成功结果含 ErrorBoundary 文件名不应误报"
        );

        // 真实失败短语(不带 Exit code 包装)仍要判 error。
        let err2 = build_result_value(
            "call_w",
            "apply_patch verification failed: Failed to find context 'uploadImage' in foo.ts",
        );
        assert_eq!(err2["is_error"], true);
    }

    #[test]
    fn pending_pairs_once_then_dedupes_replay() {
        // 唯一 call_id 避免与并行测试/转换器 emit 撞车
        let id = "call_pending_test_unique_9af3";
        assert!(!take_pending(id), "未登记时不应命中");
        register_pending(id);
        assert!(take_pending(id), "首次结果应配对成功");
        assert!(!take_pending(id), "历史重放的重复结果应被去重(已移除)");
    }

    #[test]
    fn cap_field_truncates_on_char_boundary() {
        let big = "あ".repeat(MAX_FIELD_BYTES); // 3 bytes each → well over cap
        let (text, trunc) = cap_field(&big);
        assert!(text.len() <= MAX_FIELD_BYTES);
        assert!(trunc > 0);
        // 没切坏 UTF-8:能完整重新解析
        assert!(text.chars().all(|c| c == 'あ'));
    }
}
