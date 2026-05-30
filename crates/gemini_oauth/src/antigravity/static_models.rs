//! Antigravity 模型清单**静态种子**(fallback)。
//!
//! 上游 `:fetchAvailableModels` 失败(网络 / token expire / Google 改 API 等)时
//! 退到这份种子,UI"获取模型"按钮还能拿到 sane default。
//!
//! 来源:[MOC-69] **2026-05-30 从真实上游 `:fetchAvailableModels` 抓取刷新**(16 条
//! 带 displayName 的 user-facing 模型;旧版 CLIProxyAPI 静态切片命名已过期,跟实时
//! 上游对不上,故改抓实时)。编译期 `include_str!` 进二进制。每条带 `display_name` /
//! `recommended` / `tag_title`(供前端 + Codex model catalog 显示)+ maxTokens /
//! maxOutputTokens(context window)。`is_skipped_model_id` 在 load 时过滤掉 7 条
//! (claude 两款 / gemini-2.5-* 旧版 / gemini-3.1-pro-high 实测不可用)→ 暴露 9 条。
//!
//! ⚠️ 上游可能新增 / 改动模型 — 这份种子定期(eg release 前)从 antigravity
//! `:fetchAvailableModels` 重抓刷新(上游响应是 model-id → {displayName, recommended,
//! tagTitle, maxTokens, maxOutputTokens, ...} 的 object)。

use std::sync::OnceLock;

use serde_json::Value;

use super::models::{is_skipped_model_id, AntigravityModelEntry};

const SEED_JSON: &str = include_str!("../../static_data/antigravity_models.json");

fn seed_models() -> &'static Vec<AntigravityModelEntry> {
    static CELL: OnceLock<Vec<AntigravityModelEntry>> = OnceLock::new();
    CELL.get_or_init(|| {
        let raw: Vec<Value> = serde_json::from_str(SEED_JSON)
            .expect("antigravity_models.json static seed parse failed");
        raw.into_iter()
            .filter_map(|v| serde_json::from_value::<AntigravityModelEntry>(v).ok())
            // [MOC-69] seed fallback 也过 SKIP_MODEL_IDS,跟实时 fetch 路径一致
            // (claude 两款等不提供给用户的款,seed 命中时同样排除)
            .filter(|m| !is_skipped_model_id(&m.id))
            .collect()
    })
}

/// 返静态种子 model 列表(clone)。fetch 失败时调用方退到这里
pub fn antigravity_static_models() -> Vec<AntigravityModelEntry> {
    seed_models().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锚定 seed 数量 = 9([MOC-69] 2026-05-30 实时上游 16 条 user-facing 模型
    /// - is_skipped_model_id 过滤 7 条:claude 两款 / gemini-2.5-* 四款旧版 /
    /// gemini-3.1-pro-high 实测不可用)
    #[test]
    fn seed_count_after_skip_filter() {
        assert_eq!(antigravity_static_models().len(), 9);
    }

    /// 锚定关键 model id 存在(防 seed 被意外清空 / 改名)。用 2026-05-30 实时上游
    /// 的当前命名(旧 gemini-3-pro-low/high 已不在上游)。
    #[test]
    fn seed_contains_canonical_models() {
        let models = antigravity_static_models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(
            ids.contains(&"gemini-3-flash-agent"),
            "缺 gemini-3-flash-agent"
        );
        assert!(
            ids.contains(&"gemini-3.5-flash-low"),
            "缺 gemini-3.5-flash-low"
        );
        assert!(ids.contains(&"gemini-3.1-pro-low"), "缺 gemini-3.1-pro-low");
        assert!(ids.contains(&"gemini-pro-agent"), "缺 gemini-pro-agent");
        assert!(
            ids.contains(&"gpt-oss-120b-medium"),
            "缺 gpt-oss-120b-medium"
        );
    }

    /// [MOC-69] seed 必须带 display_name + recommended(给前端 + Codex catalog 显示用)。
    /// 锚定关键款的 displayName 跟实时上游一致(命名错位坑:id 名 ≠ 档位)。
    #[test]
    fn seed_carries_display_name_and_recommended() {
        let models = antigravity_static_models();
        let find = |id: &str| models.iter().find(|m| m.id == id).cloned();

        let high = find("gemini-3-flash-agent").expect("gemini-3-flash-agent");
        assert_eq!(high.display_name, "Gemini 3.5 Flash (High)");
        assert!(high.recommended, "gemini-3-flash-agent 应为 recommended");
        assert_eq!(high.tag_title.as_deref(), Some("Fast"));

        let medium = find("gemini-3.5-flash-low").expect("gemini-3.5-flash-low");
        assert_eq!(medium.display_name, "Gemini 3.5 Flash (Medium)");

        // 非推荐款仍在 seed,但 recommended=false
        let lite = find("gemini-3.1-flash-lite").expect("gemini-3.1-flash-lite");
        assert!(!lite.recommended, "gemini-3.1-flash-lite 不应 recommended");
    }

    /// [MOC-69] seed fallback 也过 SKIP — claude 两款不出现在静态种子列表里
    #[test]
    fn seed_excludes_claude_models() {
        let ids: Vec<String> = antigravity_static_models()
            .iter()
            .map(|m| m.id.clone())
            .collect();
        assert!(
            !ids.iter().any(|id| id.starts_with("claude")),
            "claude 款不该出现在 seed 列表(SKIP 过滤),实际: {ids:?}"
        );
    }

    /// 锚定 owned_by/type 全部 "antigravity"(OpenAI /v1/models 客户端按 owned_by 区分)
    #[test]
    fn seed_all_owned_by_antigravity() {
        for m in antigravity_static_models() {
            assert_eq!(m.owned_by, "antigravity");
            assert_eq!(m.kind, "antigravity");
            assert_eq!(m.object, "model");
        }
    }

    /// [MOC-69] 防 seed 刷新静默丢条目:`seed_models()` 用 `filter_map(.ok())`,某条若
    /// 缺/改了 struct 的非可选字段会被**静默 skip**,仅靠 `=9` count 断言挡不住「同 commit
    /// 加一条 + 漏字段」抵消。这里逐条 `expect` 解析(失败给 actionable panic),并断言
    /// 「parse 成功数 == raw 总数」「过滤后 == raw - skip」,任何静默丢失都会响。
    #[test]
    fn seed_every_entry_parses_no_silent_drop() {
        let raw: Vec<Value> =
            serde_json::from_str(SEED_JSON).expect("antigravity_models.json 顶层数组解析失败");
        let mut skipped = 0usize;
        for (i, v) in raw.iter().enumerate() {
            let entry: AntigravityModelEntry = serde_json::from_value(v.clone())
                .unwrap_or_else(|e| panic!("seed 第 {i} 条解析失败(字段缺失/改名?): {e}\n{v}"));
            if is_skipped_model_id(&entry.id) {
                skipped += 1;
            }
        }
        assert_eq!(
            raw.len() - skipped,
            antigravity_static_models().len(),
            "seed 过滤后数量与 raw-skip 不符 —— 可能有 entry 解析失败被 filter_map 静默丢弃"
        );
    }
}
