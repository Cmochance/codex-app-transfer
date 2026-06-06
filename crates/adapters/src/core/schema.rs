use serde_json::{json, Value};

/// 给 chat-completions function tool 的 parameters JSON Schema 补全严格 validator
/// 要求的 `required` 数组（缺失时补 `[]`），并对 `type:"object"` 节点补缺失的
/// `properties`（补 `{}`）。**递归**处理嵌套子 schema。
///
/// ## 为什么需要
///
/// Codex 部分内置工具（`list_mcp_resources` / `load_workspace_dependencies` /
/// `read_thread_terminal` 等，参数全 optional 或无参）的 parameters schema **省略**
/// 了 `required` 字段。OpenAI 官方 / DeepSeek 官方等宽容上游默认把缺失 required 当
/// 空集、照常接受；但严格 OpenAI 兼容中转网关（如 AIOHub）的 JSON Schema validator
/// 要求 object schema 显式带 `required` 数组，读到缺失字段得 `null` → 报
/// `null is not of type "array"` 把整轮请求 400 拒掉（MOC-188，用户反馈 fb-63e74a8a）。
///
/// 补 `required:[]` 语义中性（声明"无必填字段"），对宽容上游是 no-op、对严格 validator
/// 才是必需 —— 故可对 chat 路径 function tool 统一补，无需 per-provider 特判。
///
/// ## 边界（防误伤 / 防改崩）
///
/// - **只补、不改、不删**：用 entry-or-insert，已有 `required` / `properties`（无论
///   空非空）一律不动 → 对本就合规的工具是 no-op。
/// - **只认 object schema 节点**（`type=="object"`，或 `type` 数组含 `"object"` 的
///   nullable 形态如 `["object","null"]`）补 required/properties，不给 string /
///   number / array 等节点乱加。
/// - **白名单递归**：只下钻确定承载子 schema 的字段（`properties.*` /
///   `patternProperties.*` / `items` / `prefixItems` / `$defs` / `definitions` /
///   `anyOf` / `oneOf` / `allOf` / object 形态的 `additionalProperties`），**不进**
///   `default` / `const` / `examples` / `enum` 等"数据"字段 —— 避免把恰好长得像
///   schema 的用户数据误当 schema 补字段。
/// - **strict 由调用方把关**：`strict:true` 工具按 OpenAI 规范要求 `required` 列全所有
///   properties，补空数组反而违规，故调用方仅在 `strict==false` 时调用本 fn
///   （`strict:true` 工具的 schema 本应自带完整 required，原样透传）。
/// - **顶层 `type`**：本 fn 自身只对 object schema 节点（见上）补 required，对顶层无 type 的
///   输入安全降级为不补（只漏补、不错补，helper 正确性不依赖调用方）。集成到 chat 路径时
///   调用方（`tools.rs::convert_responses_tool_to_chat_tool`）已先把 object 形态 parameters
///   顶层补成 `type:"object"`，故该降级分支在实际调用中不触发，仅作单元级防御。
/// - depth 上限防 self-recursive `$ref` schema 死循环（对齐 `gemini_native` 的
///   `sanitize_schema_inplace`）。
pub(crate) fn ensure_object_schema_required(parameters: &mut Value) {
    ensure_object_schema_required_inplace(parameters, 0);
}

fn ensure_object_schema_required_inplace(node: &mut Value, depth: usize) {
    if depth > 64 {
        return;
    }
    let Some(obj) = node.as_object_mut() else {
        return;
    };

    // type 是标量 "object",或 union 数组含 "object"(nullable object,如
    // `["object","null"]` —— 某些 schema 生成器的 Optional 表达);两者 non-null 实例
    // 都是 object schema,严格 validator 同样要求 required(MOC-188 review P2)。
    let is_object_schema = match obj.get("type") {
        Some(Value::String(s)) => s == "object",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("object")),
        _ => false,
    };
    if is_object_schema {
        obj.entry("properties").or_insert_with(|| json!({}));
        obj.entry("required").or_insert_with(|| json!([]));
    }

    // ── 白名单递归：只下钻确定承载子 schema 的字段，不碰 default/const/examples 等数据 ──
    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        for (_k, v) in props.iter_mut() {
            ensure_object_schema_required_inplace(v, depth + 1);
        }
    }
    // patternProperties：regex key → 子 schema(dynamic-key map 的另一种表达,与
    // additionalProperties 并列);遍历 value 子 schema 递归。
    if let Some(Value::Object(pp)) = obj.get_mut("patternProperties") {
        for (_k, v) in pp.iter_mut() {
            ensure_object_schema_required_inplace(v, depth + 1);
        }
    }
    if let Some(items) = obj.get_mut("items") {
        match items {
            // 单 schema（标准 / draft-07 的"剩余元素"形态）
            Value::Object(_) => ensure_object_schema_required_inplace(items, depth + 1),
            // tuple validation：items 是 schema 数组（JSON Schema draft-07）
            Value::Array(arr) => {
                for v in arr.iter_mut() {
                    ensure_object_schema_required_inplace(v, depth + 1);
                }
            }
            _ => {}
        }
    }
    // prefixItems：JSON Schema 2020-12 的 tuple validation（draft-07 的 array-form
    // `items` 在 2020-12 拆成了 `prefixItems`），始终是子 schema 数组。
    if let Some(Value::Array(arr)) = obj.get_mut("prefixItems") {
        for v in arr.iter_mut() {
            ensure_object_schema_required_inplace(v, depth + 1);
        }
    }
    for defs_key in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = obj.get_mut(defs_key) {
            for (_k, v) in defs.iter_mut() {
                ensure_object_schema_required_inplace(v, depth + 1);
            }
        }
    }
    for comb in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(arr)) = obj.get_mut(comb) {
            for v in arr.iter_mut() {
                ensure_object_schema_required_inplace(v, depth + 1);
            }
        }
    }
    // additionalProperties 可以是 bool（不碰）或子 schema（递归）
    if let Some(ap) = obj.get_mut("additionalProperties") {
        if ap.is_object() {
            ensure_object_schema_required_inplace(ap, depth + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_required_when_missing_keeps_properties() {
        // list_mcp_resources 形态：有 properties、无 required
        let mut s = json!({
            "type": "object",
            "properties": {"cursor": {"type": "string"}, "server": {"type": "string"}},
            "additionalProperties": false
        });
        ensure_object_schema_required(&mut s);
        assert_eq!(s["required"], json!([]));
        // properties 与 additionalProperties 原样不动
        assert!(s["properties"]["cursor"].is_object());
        assert_eq!(s["additionalProperties"], json!(false));
    }

    #[test]
    fn adds_required_and_properties_when_both_missing() {
        let mut s = json!({"type": "object"});
        ensure_object_schema_required(&mut s);
        assert_eq!(s["required"], json!([]));
        assert_eq!(s["properties"], json!({}));
    }

    #[test]
    fn empty_properties_object_gets_required() {
        // load_workspace_dependencies 形态：properties:{} 已有、缺 required
        let mut s = json!({"type": "object", "properties": {}, "additionalProperties": false});
        ensure_object_schema_required(&mut s);
        assert_eq!(s["required"], json!([]));
    }

    #[test]
    fn existing_required_is_not_touched() {
        let mut s = json!({
            "type": "object",
            "properties": {"q": {"type": "string"}},
            "required": ["q"]
        });
        ensure_object_schema_required(&mut s);
        assert_eq!(s["required"], json!(["q"])); // 非空 required 不动
    }

    #[test]
    fn non_object_schema_gets_no_required() {
        let mut string_schema = json!({"type": "string"});
        ensure_object_schema_required(&mut string_schema);
        assert!(string_schema.get("required").is_none());

        let mut array_schema = json!({"type": "array", "items": {"type": "string"}});
        ensure_object_schema_required(&mut array_schema);
        assert!(array_schema.get("required").is_none());
    }

    #[test]
    fn union_object_null_type_gets_required() {
        // nullable object：type 数组含 "object"（如 ["object","null"]，某些 schema
        // 生成器的 Optional 表达）仍是 object schema，non-null 实例需带 required。
        let mut s = json!({"type": ["object", "null"], "properties": {"x": {"type": "string"}}});
        ensure_object_schema_required(&mut s);
        assert_eq!(s["required"], json!([]));
        // 纯非 object 的 union（如 ["string","null"]）不补
        let mut s2 = json!({"type": ["string", "null"]});
        ensure_object_schema_required(&mut s2);
        assert!(s2.get("required").is_none());
    }

    #[test]
    fn recurses_into_nested_object_property() {
        let mut s = json!({
            "type": "object",
            "properties": {
                "filter": {"type": "object", "properties": {"q": {"type": "string"}}}
            }
        });
        ensure_object_schema_required(&mut s);
        assert_eq!(s["required"], json!([]));
        assert_eq!(s["properties"]["filter"]["required"], json!([])); // 嵌套 object 也补
    }

    #[test]
    fn recurses_into_items_defs_and_combinators() {
        let mut s = json!({
            "type": "object",
            "properties": {
                "list": {"type": "array", "items": {"type": "object", "properties": {}}}
            },
            "$defs": {"Inner": {"type": "object", "properties": {}}},
            "definitions": {"Legacy": {"type": "object", "properties": {}}},
            "anyOf": [{"type": "object", "properties": {}}],
            "oneOf": [{"type": "object", "properties": {}}],
            "allOf": [{"type": "object", "properties": {}}]
        });
        ensure_object_schema_required(&mut s);
        assert_eq!(s["properties"]["list"]["items"]["required"], json!([]));
        assert_eq!(s["$defs"]["Inner"]["required"], json!([]));
        assert_eq!(s["definitions"]["Legacy"]["required"], json!([])); // draft-07 旧式拼写
        assert_eq!(s["anyOf"][0]["required"], json!([]));
        assert_eq!(s["oneOf"][0]["required"], json!([]));
        assert_eq!(s["allOf"][0]["required"], json!([]));
    }

    #[test]
    fn recurses_into_tuple_items_prefixitems_and_additionalproperties_subschema() {
        let mut s = json!({
            "type": "object",
            "properties": {
                // additionalProperties 是子 schema（map 形态参数）→ 递归补
                "dict": {"type": "object", "additionalProperties": {"type": "object", "properties": {}}},
                // patternProperties：regex key → 子 schema（与 additionalProperties 并列的 map 表达）
                "pat": {"type": "object", "patternProperties": {"^x$": {"type": "object", "properties": {}}}},
                // items 数组形态（draft-07 tuple）：object 元素补、标量元素不补
                "tup": {"type": "array", "items": [{"type": "object", "properties": {}}, {"type": "string"}]},
                // prefixItems（2020-12 tuple）
                "pre": {"type": "array", "prefixItems": [{"type": "object", "properties": {}}]}
            }
        });
        ensure_object_schema_required(&mut s);
        assert_eq!(
            s["properties"]["dict"]["additionalProperties"]["required"],
            json!([])
        );
        assert_eq!(
            s["properties"]["pat"]["patternProperties"]["^x$"]["required"],
            json!([])
        );
        assert_eq!(s["properties"]["tup"]["items"][0]["required"], json!([]));
        assert!(s["properties"]["tup"]["items"][1].get("required").is_none());
        assert_eq!(
            s["properties"]["pre"]["prefixItems"][0]["required"],
            json!([])
        );
    }

    #[test]
    fn does_not_touch_non_schema_data_fields() {
        // property 的 `default` 值恰好长得像 object schema —— 是数据、不是 schema，
        // 白名单递归不下钻 default，故不应被补 required（防误伤的关键回归测试）。
        let mut s = json!({
            "type": "object",
            "properties": {
                "cfg": {"type": "object", "properties": {}, "default": {"type": "object"}}
            }
        });
        ensure_object_schema_required(&mut s);
        assert_eq!(s["properties"]["cfg"]["required"], json!([])); // cfg 自身（schema）补了
        assert_eq!(
            s["properties"]["cfg"]["default"],
            json!({"type": "object"}) // cfg.default（数据）不被碰
        );
    }
}
