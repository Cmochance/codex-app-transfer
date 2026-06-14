//! 模型别名 / 多 provider 路由(对应 `backend/model_alias.py`).

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use serde_json::Value;

use crate::schema::ModelMappings;

pub struct ModelSlot {
    pub key: &'static str,
    pub legacy: &'static [&'static str],
    pub openai_id: Option<&'static str>,
}

pub static MODEL_SLOTS: &[ModelSlot] = &[
    ModelSlot {
        key: "default",
        legacy: &["default"],
        openai_id: None,
    },
    ModelSlot {
        key: "gpt_5_5",
        legacy: &[],
        openai_id: Some("gpt-5.5"),
    },
    ModelSlot {
        key: "gpt_5_4",
        legacy: &[],
        openai_id: Some("gpt-5.4"),
    },
    ModelSlot {
        key: "gpt_5_4_mini",
        legacy: &[],
        openai_id: Some("gpt-5.4-mini"),
    },
    ModelSlot {
        key: "gpt_5_3_codex",
        legacy: &[],
        openai_id: Some("gpt-5.3-codex"),
    },
    ModelSlot {
        key: "gpt_5_2",
        legacy: &[],
        openai_id: Some("gpt-5.2"),
    },
];

pub static MODEL_ORDER: Lazy<Vec<&'static str>> =
    Lazy::new(|| MODEL_SLOTS.iter().map(|s| s.key).collect());

pub const DEFAULT_MODEL_KEY: &str = "default";
const INTERNAL_ONE_M_SUFFIX: &str = "[1m]";

pub fn openai_model_slot(openai_id: &str) -> Option<&'static str> {
    let requested = openai_id.trim().to_ascii_lowercase();
    if requested.is_empty() {
        return None;
    }
    MODEL_SLOTS
        .iter()
        .find(|slot| slot.openai_id == Some(requested.as_str()))
        .map(|slot| slot.key)
}

pub fn provider_slug(provider: &crate::Provider) -> String {
    let source = if !provider.id.is_empty() {
        provider.id.as_str()
    } else if !provider.name.is_empty() {
        provider.name.as_str()
    } else {
        "provider"
    };
    slugify_provider_source(source)
}

fn slugify_provider_source(source: &str) -> String {
    let mut slug = String::new();
    let mut last_was_replacement = false;
    for ch in source.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-' {
            slug.push(ch);
            last_was_replacement = false;
        } else if !last_was_replacement {
            slug.push('-');
            last_was_replacement = true;
        }
    }

    let trimmed = slug
        .trim_matches(|ch| ch == '-' || ch == '_')
        .chars()
        .take(56)
        .collect::<String>();
    if trimmed.is_empty() {
        "provider".to_owned()
    } else {
        trimmed
    }
}

pub fn has_internal_one_m_suffix(model: &str) -> bool {
    model
        .trim()
        .to_ascii_lowercase()
        .ends_with(INTERNAL_ONE_M_SUFFIX)
}

pub fn strip_internal_model_suffix(model: &str) -> String {
    let trimmed = model.trim();
    if !has_internal_one_m_suffix(trimmed) {
        return trimmed.to_owned();
    }
    trimmed[..trimmed.len() - INTERNAL_ONE_M_SUFFIX.len()]
        .trim_end()
        .to_owned()
}

pub fn empty_model_mappings() -> ModelMappings {
    let mut map = IndexMap::with_capacity(MODEL_SLOTS.len());
    for slot in MODEL_SLOTS {
        map.insert(slot.key.to_owned(), String::new());
    }
    map
}

/// 与 Python `normalize_model_mappings` 等价:旧四槽位与新槽位的合并.
///
/// 行为:
/// - 输入为空 / 非映射 → 返回所有槽位为空字符串的映射
/// - `default` 直接拷贝
/// - 其他槽位:在 `[key, ...legacy]` 中找到第一个非空值
pub fn normalize_model_mappings(input: Option<&serde_json::Value>) -> ModelMappings {
    let mut out = empty_model_mappings();
    let Some(serde_json::Value::Object(src)) = input else {
        return out;
    };

    let default = src
        .get("default")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    out.insert("default".to_owned(), default);

    for slot in MODEL_SLOTS {
        if slot.key == DEFAULT_MODEL_KEY {
            continue;
        }
        let mut filled = String::new();
        let candidates = std::iter::once(slot.key).chain(slot.legacy.iter().copied());
        for cand in candidates {
            if let Some(v) = src.get(cand).and_then(|v| v.as_str()) {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    filled = trimmed.to_owned();
                    break;
                }
            }
        }
        out.insert(slot.key.to_owned(), filled);
    }
    out
}

// ───────────────────────── 池化模型路由(pool mode)─────────────────────────
//
// 池化模式下,所有 provider 的所有模型进入一个统一池,Codex catalog 用
// `<provider_slug>/<model>` 作 slug 显示,proxy 按 slug 反查表自动分流到对应
// 上游。本节的 helper 是 catalog 生成端(snapshot.rs)与 resolver 路由端
// (proxy_runner.rs)的**单一真源** —— 两端对同一份 config 必须产出逐字一致的
// slug,否则 picker 显示的模型与实际路由的 provider 会错位(把 prompt 发错上游)。

/// 池 catalog slug 里 provider 段与模型段的分隔符。
///
/// 选 `/`:① 实测 Codex catalog 接受(`codex debug models` 渲染保留,见 MOC pool
/// Phase 0);② 可读(vendor/model 习惯);③ resolver 既有 `decide_provider` 的
/// `split_once('/')` 兼容路径天然支持。真正的路由靠 [`build_catalog_slug_map`] 的
/// 精确反查表,分隔符不影响路由正确性(故碰撞 / 归一化都不致错路由)。
pub const POOL_SLUG_SEPARATOR: char = '/';

/// 池里的一条模型:Codex 看到的 catalog slug + 应改写到的上游真实模型 id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolEntry {
    /// 在传入 `providers` 切片里的下标(catalog 端取元数据 / resolver 端取鉴权都用它)。
    pub provider_idx: usize,
    /// Codex catalog slug,形如 `deepseek/deepseek-v4-pro`(provider 段已去碰撞)。
    pub slug: String,
    /// 上游真实模型 id(已 strip 内部 `[1m]` 后缀)。
    pub real_model: String,
    /// 该模型是否声明 1M 上下文 —— 源自被 strip 掉的内部 `[1m]` 标记。slug / real_model
    /// 都已去后缀(slug 干净、上游不收 `[1m]`),但 catalog 端需要这个信号:无显式
    /// `modelCapabilities` / documented window 的自定义模型,仅靠 `[1m]` 标 1M 时,池模式
    /// 也要给 1M 窗口(否则比单 provider 模式早压缩,bot review P2)。
    pub supports_one_m: bool,
}

fn push_pooled_with_one_m(
    raw: &str,
    out: &mut Vec<(String, bool)>,
    index: &mut HashMap<String, usize>,
) {
    let one_m = has_internal_one_m_suffix(raw);
    let cleaned = strip_internal_model_suffix(raw);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return;
    }
    match index.get(trimmed) {
        // 同一 clean id 的多个变体:只要有一个带 `[1m]` 即视为支持 1M。
        Some(&i) => {
            if one_m {
                out[i].1 = true;
            }
        }
        None => {
            index.insert(trimmed.to_owned(), out.len());
            out.push((trimmed.to_owned(), one_m));
        }
    }
}

/// 某 provider 在池里的"可选模型列表",每条附带"是否声明 1M"(由被 strip 的 `[1m]`
/// 标记得出)。优先用持久化 `pooledModels`,为空则回退槽位映射(`default` 优先,再按
/// `MODEL_SLOTS` 顺序)。clean id 去重、稳定顺序;同 clean id 多变体只要一个带 `[1m]` 即 true。
pub fn pooled_models_with_one_m(
    pooled_models: Option<&Value>,
    models: Option<&Value>,
) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    // 1. 持久化 pooledModels(字符串数组)
    if let Some(Value::Array(arr)) = pooled_models {
        for item in arr {
            if let Some(s) = item.as_str() {
                push_pooled_with_one_m(s, &mut out, &mut index);
            }
        }
    }
    if !out.is_empty() {
        return out;
    }

    // 2. 回退:槽位映射的非空值(default 优先,再按槽位顺序)
    let mappings = normalize_model_mappings(models);
    if let Some(d) = mappings.get(DEFAULT_MODEL_KEY) {
        push_pooled_with_one_m(d, &mut out, &mut index);
    }
    for slot in MODEL_SLOTS {
        if slot.key == DEFAULT_MODEL_KEY {
            continue;
        }
        if let Some(v) = mappings.get(slot.key) {
            push_pooled_with_one_m(v, &mut out, &mut index);
        }
    }
    out
}

/// 同 [`pooled_models_with_one_m`] 但只返 clean id 列表(已 strip `[1m]`、去重、稳定顺序)。
pub fn pooled_model_ids(pooled_models: Option<&Value>, models: Option<&Value>) -> Vec<String> {
    pooled_models_with_one_m(pooled_models, models)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// provider 是否已加入「整合」(`extra["pooledEnabled"] == true`)。整合页里用户「添加」进去
/// 的子集才置 true;未加入的不进池(catalog 不显示、resolver 不路由其 slug)。
pub fn provider_pooled_enabled(provider: &crate::Provider) -> bool {
    provider
        .extra
        .get("pooledEnabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 给一组 provider 产出全部池条目(catalog slug ↔ provider/real_model)。
///
/// **只纳入已加入整合的 provider**(`pooledEnabled==true`)—— 整合页用户「添加」的子集。
///
/// **确定性**是核心契约:catalog 生成端与 resolver 路由端各自调用本函数(对同一份
/// `providers`),必须得到逐字一致的 slug。为此:
/// - 按 `(sort_index, id, 原始下标)` 稳定排序后分配 provider base slug;
/// - base slug 碰撞(两 provider slug 化后撞名)时追加 `-2` / `-3` … 直到唯一;
/// - `provider_idx` 始终是**原始切片**下标,方便两端各自索引自己的数据;
/// - 全局 slug 去重(同 provider 内重复模型 / 跨 provider 撞全名都只保留首次)。
pub fn unique_pool_slugs(providers: &[crate::Provider]) -> Vec<PoolEntry> {
    // 只处理已加入整合的 provider;provider_idx 仍取**原始切片**下标(两端一致)。
    let mut order: Vec<usize> = (0..providers.len())
        .filter(|&i| provider_pooled_enabled(&providers[i]))
        .collect();
    order.sort_by(|&a, &b| {
        providers[a]
            .sort_index
            .cmp(&providers[b].sort_index)
            .then_with(|| providers[a].id.cmp(&providers[b].id))
            .then(a.cmp(&b))
    });

    // 先按稳定顺序给每个 provider 定唯一 base slug(碰撞追加 -N)。
    let mut used_base: HashSet<String> = HashSet::new();
    let mut base_for_idx: Vec<String> = vec![String::new(); providers.len()];
    for &idx in &order {
        let base = provider_slug(&providers[idx]);
        let mut candidate = base.clone();
        let mut n = 1;
        while !used_base.insert(candidate.clone()) {
            n += 1;
            candidate = format!("{base}-{n}");
        }
        base_for_idx[idx] = candidate;
    }

    let mut used_slug: HashSet<String> = HashSet::new();
    let mut entries: Vec<PoolEntry> = Vec::new();
    for &idx in &order {
        let base = &base_for_idx[idx];
        let provider = &providers[idx];
        let models_value = serde_json::to_value(&provider.models).ok();
        let pairs =
            pooled_models_with_one_m(provider.extra.get("pooledModels"), models_value.as_ref());
        for (real, supports_one_m) in pairs {
            let slug = format!("{base}{POOL_SLUG_SEPARATOR}{real}");
            if used_slug.insert(slug.clone()) {
                entries.push(PoolEntry {
                    provider_idx: idx,
                    slug,
                    real_model: real,
                    supports_one_m,
                });
            }
        }
    }
    entries
}

/// 由池条目构建 resolver 用的反查表:`catalog slug → (provider_idx, real_model)`。
pub fn build_catalog_slug_map(entries: &[PoolEntry]) -> HashMap<String, (usize, String)> {
    entries
        .iter()
        .map(|e| (e.slug.clone(), (e.provider_idx, e.real_model.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_input_returns_all_blank() {
        let m = normalize_model_mappings(None);
        assert_eq!(m.len(), MODEL_SLOTS.len());
        for slot in MODEL_SLOTS {
            assert_eq!(m[slot.key], "");
        }
    }

    #[test]
    fn default_slot_passthrough() {
        let v = json!({"default": "deepseek-v4-pro"});
        let m = normalize_model_mappings(Some(&v));
        assert_eq!(m["default"], "deepseek-v4-pro");
    }

    #[test]
    fn legacy_default_carries_to_default_slot() {
        // Python 版的 legacy = ("default",) 仅对 default 槽适用,这里验证它
        let v = json!({"default": "  glm-5.1  "});
        let m = normalize_model_mappings(Some(&v));
        assert_eq!(m["default"], "glm-5.1", "应 trim 空白");
    }

    #[test]
    fn openai_model_slot_maps_current_codex_slugs() {
        assert_eq!(openai_model_slot("gpt-5.5"), Some("gpt_5_5"));
        assert_eq!(openai_model_slot("gpt-5.4-mini"), Some("gpt_5_4_mini"));
        assert_eq!(openai_model_slot(" GPT-5.5 "), Some("gpt_5_5"));
        assert_eq!(openai_model_slot("unknown"), None);
    }

    #[test]
    fn provider_slug_matches_legacy_python_rules() {
        let mut provider = crate::Provider {
            id: "OpenAI.Custom_1".into(),
            name: "Ignored Name".into(),
            base_url: String::new(),
            auth_scheme: String::new(),
            api_format: String::new(),
            api_key: String::new(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra: IndexMap::new(),
        };
        assert_eq!(provider_slug(&provider), "openai-custom_1");

        provider.id.clear();
        provider.name = "七牛 / Qiniu++".into();
        assert_eq!(provider_slug(&provider), "qiniu");

        provider.name = "___".into();
        assert_eq!(provider_slug(&provider), "provider");
    }

    #[test]
    fn strip_internal_model_suffix_only_strips_one_m_marker() {
        assert_eq!(
            strip_internal_model_suffix("deepseek-v4-pro[1m]"),
            "deepseek-v4-pro"
        );
        assert_eq!(
            strip_internal_model_suffix("deepseek-v4-pro [1M]"),
            "deepseek-v4-pro"
        );
        assert_eq!(
            strip_internal_model_suffix("deepseek-v4-pro[beta]"),
            "deepseek-v4-pro[beta]"
        );
        assert_eq!(
            strip_internal_model_suffix("deepseek-v4-pro[1m-preview]"),
            "deepseek-v4-pro[1m-preview]"
        );
    }

    #[test]
    fn key_order_is_stable() {
        let m = empty_model_mappings();
        let keys: Vec<_> = m.keys().cloned().collect();
        assert_eq!(
            keys,
            vec![
                "default",
                "gpt_5_5",
                "gpt_5_4",
                "gpt_5_4_mini",
                "gpt_5_3_codex",
                "gpt_5_2",
            ]
        );
    }

    // ── 池化路由 helper ──

    fn mk_provider(id: &str, name: &str) -> crate::Provider {
        // 默认已加入整合(pooledEnabled=true),让 unique_pool_slugs 测试纳入它;
        // 「未加入则排除」由专门的 excludes 测试覆盖。
        let mut extra = IndexMap::new();
        extra.insert("pooledEnabled".to_owned(), serde_json::json!(true));
        crate::Provider {
            id: id.into(),
            name: name.into(),
            base_url: String::new(),
            auth_scheme: String::new(),
            api_format: String::new(),
            api_key: String::new(),
            models: IndexMap::new(),
            extra_headers: IndexMap::new(),
            model_capabilities: IndexMap::new(),
            request_options: IndexMap::new(),
            is_builtin: false,
            sort_index: 0,
            extra,
        }
    }

    #[test]
    fn unique_pool_slugs_excludes_providers_not_in_integration() {
        // pooledEnabled 缺失 / false → 不进池(整合子集语义)。
        let mut included = mk_provider("a", "A");
        included.models.insert("default".into(), "a-model".into());
        let mut excluded = mk_provider("b", "B");
        excluded.models.insert("default".into(), "b-model".into());
        excluded.extra.insert("pooledEnabled".into(), json!(false));
        let mut no_flag = mk_provider("c", "C");
        no_flag.models.insert("default".into(), "c-model".into());
        no_flag.extra.shift_remove("pooledEnabled");

        let entries = unique_pool_slugs(&[included, excluded, no_flag]);
        let slugs: Vec<&str> = entries.iter().map(|e| e.slug.as_str()).collect();
        assert_eq!(
            slugs,
            vec!["a/a-model"],
            "只有 pooledEnabled=true 的 a 进池"
        );
        // provider_idx 仍是原始下标(a 在 0)
        assert_eq!(entries[0].provider_idx, 0);
    }

    #[test]
    fn pooled_model_ids_prefers_pooled_models_list() {
        // pooledModels 非空 → 用它;strip [1m];去重;忽略槽位映射
        let pooled = json!(["deepseek-v4-pro[1m]", "deepseek-chat", "deepseek-v4-pro"]);
        let models = json!({"default": "ignored-default"});
        assert_eq!(
            pooled_model_ids(Some(&pooled), Some(&models)),
            vec!["deepseek-v4-pro", "deepseek-chat"]
        );
    }

    #[test]
    fn pooled_model_ids_falls_back_to_slot_mappings() {
        // pooledModels 缺失 → 回退槽位映射:default 优先,再按槽位顺序,strip + 去重
        let models = json!({
            "default": "deepseek-v4-pro",
            "gpt_5_5": "deepseek-v4-pro",
            "gpt_5_4": "deepseek-chat[1m]",
        });
        assert_eq!(
            pooled_model_ids(None, Some(&models)),
            vec!["deepseek-v4-pro", "deepseek-chat"]
        );
    }

    #[test]
    fn pooled_model_ids_empty_array_falls_back_to_mappings() {
        let pooled = json!([]);
        let models = json!({"default": "m1"});
        assert_eq!(pooled_model_ids(Some(&pooled), Some(&models)), vec!["m1"]);
    }

    #[test]
    fn unique_pool_slugs_builds_provider_prefixed_entries() {
        let mut a = mk_provider("deepseek", "DeepSeek");
        a.models.insert("default".into(), "deepseek-v4-pro".into());
        let mut b = mk_provider("kimi", "Kimi");
        b.extra.insert(
            "pooledModels".into(),
            json!(["kimi-k2.6", "kimi-for-coding"]),
        );

        let entries = unique_pool_slugs(&[a, b]);
        let slugs: Vec<&str> = entries.iter().map(|e| e.slug.as_str()).collect();
        assert!(slugs.contains(&"deepseek/deepseek-v4-pro"));
        assert!(slugs.contains(&"kimi/kimi-k2.6"));
        assert!(slugs.contains(&"kimi/kimi-for-coding"));

        let map = build_catalog_slug_map(&entries);
        let (idx, real) = map.get("kimi/kimi-for-coding").unwrap();
        assert_eq!(*idx, 1);
        assert_eq!(real, "kimi-for-coding");
    }

    #[test]
    fn unique_pool_slugs_disambiguates_colliding_provider_slugs() {
        // 两个 provider slug 化后都撞成 "qiniu" → 第二个加 -2 后缀,两边各自路由不混
        let mut a = mk_provider("", "七牛 / Qiniu");
        a.models.insert("default".into(), "qna-v1".into());
        let mut b = mk_provider("", "Qiniu!!");
        b.models.insert("default".into(), "qna-v2".into());

        let entries = unique_pool_slugs(&[a, b]);
        assert_eq!(entries.len(), 2, "两条互不相同的池条目");
        let idxs: HashSet<usize> = entries.iter().map(|e| e.provider_idx).collect();
        assert_eq!(idxs.len(), 2, "两条分别路由到不同 provider");

        let slugs: Vec<&str> = entries.iter().map(|e| e.slug.as_str()).collect();
        assert!(slugs.iter().any(|s| s.starts_with("qiniu/")));
        assert!(
            slugs.iter().any(|s| s.starts_with("qiniu-2/")),
            "碰撞 provider 应拿到 -2 后缀: {slugs:?}"
        );

        // 反查表:两条 slug 解析到不同 provider_idx + 各自 real model
        let map = build_catalog_slug_map(&entries);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn pooled_models_with_one_m_preserves_1m_marker() {
        // `[1m]` 标记被 strip 进 clean id,但 1M 信号保留在 bool 上(供 catalog 给 1M 窗口)。
        let pooled = json!(["custom-1m[1m]", "plain-model"]);
        let pairs = pooled_models_with_one_m(Some(&pooled), None);
        assert_eq!(
            pairs,
            vec![
                ("custom-1m".to_owned(), true),
                ("plain-model".to_owned(), false)
            ]
        );
        // pooled_model_ids 契约不变(只 clean id)
        assert_eq!(
            pooled_model_ids(Some(&pooled), None),
            vec!["custom-1m", "plain-model"]
        );
    }

    #[test]
    fn pooled_models_with_one_m_ors_1m_across_duplicate_variants() {
        // 同 clean id 多变体:无后缀在前、带 [1m] 在后 → 仍标 1M(OR 合并)。
        let pooled = json!(["m", "m[1m]"]);
        let pairs = pooled_models_with_one_m(Some(&pooled), None);
        assert_eq!(pairs, vec![("m".to_owned(), true)]);
    }

    #[test]
    fn unique_pool_slugs_carries_supports_one_m_from_marker() {
        let mut p = mk_provider("deepseek", "DeepSeek");
        p.models
            .insert("default".into(), "deepseek-v4-pro[1m]".into());
        let entries = unique_pool_slugs(&[p]);
        let e = entries
            .iter()
            .find(|e| e.slug == "deepseek/deepseek-v4-pro")
            .expect("slug 应 strip [1m]");
        assert!(e.supports_one_m, "entry 应携带 1M 标记");
        assert_eq!(
            e.real_model, "deepseek-v4-pro",
            "real_model 不带 [1m](上游不收)"
        );
    }
}
