use codex_app_transfer_registry::Provider;
use serde_json::{json, Value};

/// Codex freeform tool name we special-case. See the `"custom" =>` arm in
/// `convert_responses_tool_to_chat_tool` below for the request-side rewrite
/// rationale, and `converter.rs::close_tool_call` for the response-side
/// wire re-shape — they must trigger on the exact same tool name.
pub(crate) const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";

/// Codex 0.130+ `tool_search` builtin name. Request side lowers `type:"tool_search"`
/// into a chat/function tool with this name; response side (`converter.rs` chat path,
/// `gemini_native/response.rs` Gemini path) re-shapes the model's function call back
/// into a `tool_search_call` wire — both must key off this exact name. See MOC-32
/// (chat path) and MOC-217 (gemini/antigravity path).
pub(crate) const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";

/// [MOC-296] 追加到 `tool_search` description 末尾的「按名补搜」规则。
///
/// 上游 prompt 的发现指引有盲区(2026-07-03 wire 实证):连接器(openai-curated
/// 如 Linear)工具全 defer 到 tool_search,但 description 里该源常只有 marketplace
/// tagline(零工具名),泛泛的"工具可能没给全"指引又埋在 ~9KB 文本末尾;system
/// prompt 也没有"被点名的工具先补搜"规则。结果:工具输出按名引用了未注入的工具
/// (如 Linear 截断提示 "use `get_issue`")时,模型对照工具列表 → 判"不可用" →
/// 卡死,实际 `tool_search("get_issue")` 精确名一搜必中(目录里一直有)。
///
/// chat 分支与 gemini_native 请求侧(MOC-217 复用先例)共用本常量;anthropic
/// 请求侧复用 chat 转换自动覆盖;官方 GPT passthrough 不经此转换、不受影响。
/// 只追加不改写上游原文(遵循"不做不必要剥除")。
pub(crate) const TOOL_SEARCH_BY_NAME_HINT: &str = "\n\nIf a tool is referenced by name anywhere (for example a hint like \"use `get_issue`\") but is not in your current tool list, call `tool_search` with that exact tool name first — deferred tools only become available after being discovered here. Do not conclude a named tool is unavailable until an exact-name search has returned no match.";

/// Chat-path replacement for Codex CLI's freeform `apply_patch` description.
/// Original upstream text says "do not wrap the patch in JSON" because the
/// Responses API freeform/lark grammar accepts raw text — but on the
/// chat-completions path the model MUST emit a function call whose `input`
/// argument is a JSON string containing the V4A patch. We rewrite the
/// description so the model sees instructions consistent with the wire
/// format it has to produce.
///
/// **重要:hunk body 的 space-prefixed 行语义** — 上游 freeform 工具用 lark
/// grammar 强制约束,模型在受约束的解码空间里不会搞错;但 chat function-call
/// 没有 grammar 约束,只剩 description。实测(issue #235 真机)DeepSeek
/// 反复在一个具体语义上栽跟头:
///
/// > `@@ <context> @@` 标记后的 space-prefixed 行 = 文件中 context 锚点
/// > **之后**的行,**不是** context 行本身的重复
///
/// 不显式说清这个,模型会把 context 行当成 space 行再写一次,parse_patch
/// 找不到双行 → 整个 patch 拒收。本 description 通过显式规则 + 一个最小
/// 可执行的更新文件 example 让模型看到正确形态。
pub(crate) const APPLY_PATCH_TOOL_DESCRIPTION_FOR_CHAT: &str = concat!(
    "Edit files using the apply_patch tool. ",
    "**ALWAYS use this tool to write file content** — new files, single-line edits, and full-file rewrites alike. ",
    "**NEVER use shell `cat <<EOF > file` / `printf '<content>' > file` / `echo '<content>' > file` / any `>` redirect to write actual file content** — doing so bypasses the Codex diff UI and audit trail. ",
    "(To create brand-new or empty files, use `*** Add File: <path>` — not a shell redirect.) ",
    "**PREFER SURGICAL TARGETED EDITS.** To change or replace existing content, emit ONLY the specific `-` (old) and `+` (new) lines for what actually changes, with minimal context. Do NOT regenerate the whole file/section and append it; do NOT rewrite an entire file just because part of it changed. Reserve full-file replacement (`*** Delete File: <path>` then `*** Add File: <path>` with every line `+`-prefixed, in one patch) for genuine cases ONLY: creating brand-new content, or when almost every line truly differs. ",
    "Call this function with a single `input` string containing a V4A patch. ",
    "**The patch MUST start with `*** Begin Patch` as the literal first line** (no leading whitespace, no other content before it), and end with `*** End Patch`. ",
    "Each file operation header is one of `*** Add File: <path>`, ",
    "`*** Update File: <path>` (optionally followed by `*** Move to: <path>`, but Update with Move STILL requires at least one hunk — see RENAME / MOVE FILE section), ",
    "or `*** Delete File: <path>`. ",
    "Within Update hunks, the simplest form is just `-`/`+` lines with no `@@` and ",
    "no context (suitable when the `-` line is unique in the file). If disambiguation ",
    "is needed, add space-prefixed context lines, or a single-sided `@@ <header>` ",
    "marker (e.g. `@@ class Foo`, `@@ def bar():`) — NEVER add a trailing `@@`. ",
    "Lines are `-line` (removed, no space after `-`), `+line` (added, no space after `+`), ",
    "or ` line` (single leading space = unchanged context). ",
    "Use relative paths only (never absolute). ",
    "Embed real newlines as `\\n` inside the JSON string value for `input`.\n\n",
    "CRITICAL `@@` ANCHOR SYNTAX (the most common cause of patch rejection on chat-completions providers):\n",
    "The V4A `@@` operator is SINGLE-SIDED: write `@@ <header>` where `<header>` ",
    "names the class/function/section the hunk belongs to (e.g. `@@ class MyClass`, ",
    "`@@ def my_function():`, `@@ fn main() {`). ",
    "**NEVER write a trailing `@@` (e.g. `@@ def f(): @@`)** — Codex Desktop's V4A ",
    "applier will treat the trailing `@@` as literal text inside the anchor and ",
    "fail with `Failed to find context '... @@'`. ",
    "The `@@` header is OPTIONAL: if 3 lines of surrounding context already uniquely ",
    "identify the location, omit the `@@` line entirely. ",
    "If a single `@@ <header>` is ambiguous (same name appears in multiple classes), ",
    "use MULTIPLE `@@` lines on separate rows (e.g. `@@ class Outer\\n@@ def inner():`) ",
    "to narrow down — each line is one `@@ <header>`, single-sided.\n\n",
    "ADD FILE FORMAT (different from Update — no hunks, no `@@`):\n",
    "After `*** Add File: <path>`, **every line of the new file's content MUST be ",
    "prefixed with `+`**, including blank lines (write them as a bare `+` on its own ",
    "row). Do NOT use `@@` markers, hunks, or space-prefixed context lines in an ",
    "Add File block — they are reserved for Update File. Writing raw source code ",
    "(e.g. `def main():` with no `+` prefix) directly after `*** Add File:` causes ",
    "`'def main():' is not a valid hunk header` errors.\n\n",
    "RENAME / MOVE FILE (`*** Move to:` always needs ≥1 hunk, never empty):\n",
    "`*** Update File: <old>\\n*** Move to: <new>` followed by **at least one hunk** with `-`/`+` lines (or `*** End of File` marker). An empty Update+Move block fails with `Update file hunk for path '<old>' is empty`. ",
    "**For pure rename (no content change)**: use a Delete + Add File pair within the same patch instead — `*** Delete File: <old>` followed by `*** Add File: <new>` with every original line prefixed `+`. ",
    "**For rename WITH content change**: keep `*** Update File:` + `*** Move to:` and include the actual `-`/`+` hunks for the changes.\n\n",
    "LINE PREFIX FORMAT (zero whitespace between prefix and content):\n",
    "Every line in a hunk starts with exactly ONE character followed by content with ",
    "NO intervening space — `-line_content` (NOT `- line_content`), `+line_content` ",
    "(NOT `+ line_content`), ` line_content` (single leading space = unchanged context). ",
    "Codex Desktop V4A applier may tolerate a stray space, but other apply_patch ",
    "implementations are strict — keep the prefix tight.\n\n",
    "EXAMPLE 1 (MINIMAL UPDATE — preferred form for simple single-line edits): ",
    "When the `-` line you remove is byte-exact and unique in the file, you may omit ",
    "BOTH `@@` markers AND context lines — just write `-` and `+` lines directly:\n",
    "*** Begin Patch\n",
    "*** Update File: src/config.py\n",
    "-DEBUG = False\n",
    "+DEBUG = True\n",
    "*** End Patch\n",
    "This is the simplest and most reliable mode on chat-completions providers. Use ",
    "it whenever the `-` line is unique enough to pinpoint the change location.\n\n",
    "EXAMPLE 2 — Update with `@@` header (only when needed: same name in multiple ",
    "classes/functions, or you want to disambiguate which occurrence to change):\n",
    "*** Begin Patch\n",
    "*** Update File: src/main.rs\n",
    "@@ fn main() {\n",
    "-    let x = 1;\n",
    "+    let x = 2;\n",
    "     println!(\"{}\", x);\n",
    "*** End Patch\n",
    "Notice: `@@ fn main() {` is single-sided (no trailing `@@`). The `-` line ",
    "is byte-exact what currently appears in the file. The space-prefixed line is ",
    "kept as-is for context. Use this form when `let x = 1;` appears in multiple ",
    "functions and you need to specify which one.\n\n",
    "EXAMPLE 3 — create a brand new file (Add File, no `@@`, every line `+`):\n",
    "*** Begin Patch\n",
    "*** Add File: hello.py\n",
    "+def greet(name: str) -> str:\n",
    "+    return f\"Hello, {name}!\"\n",
    "+\n",
    "+if __name__ == \"__main__\":\n",
    "+    print(greet(\"world\"))\n",
    "*** End Patch\n",
    "Notice: no `@@`, every line has `+` (including the blank line as a bare `+`).\n\n",
    "EXAMPLE 4 — update a function body with context lines (no `@@`, use when the ",
    "`-` line is not unique enough by itself but a few surrounding lines pin it):\n",
    "*** Begin Patch\n",
    "*** Update File: src/util.py\n",
    " def divide(a, b):\n",
    "     \"\"\"Divide two numbers.\"\"\"\n",
    "-    return a / b\n",
    "+    if b == 0:\n",
    "+        raise ValueError(\"divide by zero\")\n",
    "+    return a / b\n",
    "*** End Patch\n",
    "Notice: 2 lines of space-prefixed context above the `-` line uniquely identify ",
    "where to apply. Use this when minimal form (EXAMPLE 1) is ambiguous but `@@` ",
    "(EXAMPLE 2) is overkill.\n\n",
    "BYTE-EXACT MATCHING (#1 cause of `Failed to find context` on this path):\n",
    "Every `-` line and every space-prefixed context line MUST match the file ",
    "byte-for-byte — same leading whitespace, no trimmed trailing spaces, exact ",
    "characters. If unsure, run `cat <path>` or `sed -n '1,80p' <path>` via shell ",
    "to read it first, then compose the patch from real bytes. Guessing or ",
    "paraphrasing produces `Failed to find context '<your guess>'` errors.\n\n",
    "CHAT-PATH GOTCHAS (the lark grammar is gone here; observed empirically with non-OpenAI providers):\n",
    "1. Use the SINGLE-SIDED `@@ <header>` form. The double-sided `@@ ... @@` form ",
    "is NOT V4A — the trailing `@@` becomes literal text and breaks context matching.\n",
    "2. Do NOT combine `*** Add File: foo` and `*** Update File: foo` in the SAME patch — Update reads the file before Add lands on disk. ",
    "Either make Add File write the final content in one shot, or split into two separate patches.\n",
    "3. To populate a brand-new or empty file, use `*** Add File: <path>` with every line `+`-prefixed (not `*** Update File:`).\n",
    "4. In a multi-line file, lone `+` lines without a corresponding `-` APPEND below the previous context — they do NOT replace any existing line. ",
    "To change a line, use `-` to remove the old line AND `+` to add the new one; do not omit the `-`.\n",
    "5. If an Update fails with `Failed to find context`, the `-`/context lines did not match the file byte-for-byte. Re-read the file (`cat <path>` / `sed -n`) and fix those lines to match exactly, then retry the SAME surgical Update. Do NOT escalate to rewriting or re-appending the whole file/section — keep the edit targeted to the lines that change.\n",
    "6. `*** Begin Patch` MUST be the literal first line of `input` — no preamble, no whitespace, no `*** Add File:` directly. Forgetting it causes `invalid patch: The first line of the patch must be '*** Begin Patch'`.\n",
    "7. `*** Update File: <old>` + `*** Move to: <new>` requires at least one hunk (rename-only is NOT supported via Move). For pure rename without content change, use `*** Delete File: <old>` + `*** Add File: <new>` (copy original content with `+` prefix). Empty Update+Move fails with `Update file hunk for path '<old>' is empty`."
);

/// Chat-path replacement for the freeform `input` parameter description.
/// Mirrors `APPLY_PATCH_TOOL_DESCRIPTION_FOR_CHAT` but at the parameter level,
/// so the model sees the format constraint regardless of whether providers
/// surface tool-level or parameter-level descriptions more prominently.
/// Same anchor-vs-space-line gotcha called out here in compact form (some
/// providers truncate or de-emphasize tool-level descriptions on long
/// histories — keep the rule visible at parameter level too).
pub(crate) const APPLY_PATCH_INPUT_DESCRIPTION_FOR_CHAT: &str = concat!(
    "A V4A patch starting with `*** Begin Patch` and ending with `*** End Patch`. ",
    "Use `*** Add File:`, `*** Update File:`, or `*** Delete File:` headers. ",
    "Update File simplest form: just `-line`/`+line` rows directly after the header ",
    "(no `@@`, no context) — use this when the `-` line is unique in the file. ",
    "If ambiguous, add space-prefixed context ` line` lines around the change, or ",
    "single-sided `@@ <header>` (e.g. `@@ def func():`, NO trailing `@@`). ",
    "Writing `@@ <header> @@` (double-sided) fails with `Failed to find context '... @@'`. ",
    "Lines are `-text`/`+text`/` text` (single char prefix, NO space between prefix and content). ",
    "Add File uses NO `@@` and NO hunks — prefix EVERY new content line with `+` ",
    "(blank lines as bare `+`). Relative paths only. ",
    "`-` lines and space-prefixed context MUST be byte-exact to the file's current content ",
    "(read via `cat <path>` first if unsure) — guessing produces `Failed to find context` errors. ",
    "**PREFER surgical targeted Update** (`-` old line + `+` new line for ONLY the changed lines, minimal context) — do NOT regenerate or append the whole file/section. ",
    "Chat-path gotchas: do not Add+Update the same path in one patch; for brand-new/empty files use `*** Add File:` (not Update); lone `+` without `-` APPENDS rather than replaces — to replace a line, pair `-` (old) with `+` (new). ",
    "If Update fails with `Failed to find context`, re-read the file (`cat`) and fix the `-`/context lines to be byte-exact, then retry the SAME targeted Update — do NOT escalate to rewriting the whole file. ",
    "**`*** Begin Patch` MUST be the literal first line of `input`** (no preamble). ",
    "**`*** Update File: <old>` + `*** Move to: <new>` requires ≥1 hunk** — for pure rename use `*** Delete File:` + `*** Add File:` instead."
);

/// Responses tool 定义 → Chat tool 定义.
/// 把单个 Responses API tool 转成零或多个 Chat Completions tool。
///
/// 返回 `Vec<Value>` 而非 `Option<Value>` 是为了支持 `type:"namespace"` 展平
/// (Codex CLI 把 MCP server 工具集打成一个 namespace 包,内层 5-26 个具体
/// `type:"function"`,实测 9 个 server 共 88 个 tool 在第三方 chat provider
/// 之前必须展平为顶级 function 数组)。
///
/// 实测形态(2026-05-09 抓本机 ~/.codex/config.toml 配 12+ MCP server 时
/// Codex CLI 的入站 Responses API body):
/// - `function` × 420 / 轮(Codex 内置 + `read_mcp_resource` 等通用 meta)
/// - `namespace` × 218 / 轮(9 个 server 包装,内层 88 个具体 MCP function)
/// - `custom` × 28 / 轮(`apply_patch` 用 lark grammar)
/// - `web_search` × 28 / 轮(server-side built-in,无 name/parameters,
///   chat 端无等价,继续 drop + warn_once 提示用户)
pub fn convert_responses_tool_to_chat_tool(
    tool: &Value,
    provider: Option<&Provider>,
) -> Vec<Value> {
    let Some(obj) = tool.as_object() else {
        return vec![];
    };
    let Some(ttype) = obj.get("type").and_then(|v| v.as_str()) else {
        return vec![];
    };
    match ttype {
        "function" => {
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut parameters = obj.get("parameters").cloned().unwrap_or_else(|| json!({}));
            if let Some(po) = parameters.as_object_mut() {
                if !po.contains_key("type") {
                    po.insert("type".into(), Value::String("object".into()));
                }
            }
            let strict = obj.get("strict").and_then(|v| v.as_bool()).unwrap_or(false);
            // Codex 部分内置工具(list_mcp_resources / load_workspace_dependencies /
            // read_thread_terminal 等,参数全 optional 或无参)的 parameters schema 省略了
            // `required` 数组。OpenAI / DeepSeek 官方等宽容上游默认当空集放行;但严格 OpenAI
            // 兼容中转网关(如 AIOHub)的 validator 要求 object schema 显式带 `required`,读到
            // 缺失字段得 null → 报 `null is not of type "array"` 把整轮请求 400 拒掉
            // (MOC-188,用户反馈 fb-63e74a8a)。统一补 `required:[]`(语义中性,对宽容上游
            // no-op)。`strict:true` 工具按 OpenAI 规范须 required 列全 properties,补空反而
            // 违规 → 仅 non-strict 补,strict 工具原样透传(详见 core::schema 文档)。
            if !strict {
                crate::core::schema::ensure_object_schema_required(&mut parameters);
            }
            vec![json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                    "strict": strict,
                },
            })]
        }
        "custom" => {
            // Custom tool(Responses API freeform tool,无 JSON schema)降级为
            // 接受单字符串 input 的 function tool — chat completions 不认
            // `type:"custom"`,DeepSeek / Kimi / MiMo 等 chat 上游必须走 function。
            //
            // **apply_patch 特判**:Codex CLI 把 apply_patch 作为 freeform 工具
            // 注册,wire description 是 "Use the `apply_patch` tool to edit files.
            // This is a FREEFORM tool, so do not wrap the patch in JSON."
            // (上游 `codex-rs/core/src/tools/handlers/apply_patch_spec.rs` 实证)。
            // 经 chat function-call 反而**必须**把 patch 包进 JSON 字符串值 ——
            // 上游的 "do not wrap in JSON" 指令在 chat 路径下会误导模型,
            // 且原 description 没给 V4A 格式样例。这里替换成对 chat 路径准确
            // 的指引,把 V4A 关键字 / 文件操作头 / hunk 标记列清楚,让 DeepSeek
            // 之类的模型知道 input 字段该填什么。
            // 响应侧(converter.rs::close_tool_call)对 name==apply_patch 特判,
            // 把模型回来的 function_call 重新打包成 custom_tool_call wire,
            // 让 Codex CLI router (`ResponseItem::CustomToolCall`) 正确路由到
            // apply_patch handler(handler 硬要求 `ToolPayload::Custom { input }`,
            // 见 `codex-rs/core/src/tools/handlers/apply_patch.rs:324`)。
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let original_description = obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let (tool_description, input_description) = if name == APPLY_PATCH_TOOL_NAME {
                (
                    APPLY_PATCH_TOOL_DESCRIPTION_FOR_CHAT.to_owned(),
                    APPLY_PATCH_INPUT_DESCRIPTION_FOR_CHAT.to_owned(),
                )
            } else {
                (
                    original_description.to_owned(),
                    "Free-form input passed verbatim to the tool.".to_owned(),
                )
            };
            vec![json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": tool_description,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": input_description,
                            }
                        },
                        "required": ["input"],
                    },
                    "strict": false,
                },
            })]
        }
        "namespace" => {
            // Codex CLI 用 `type:"namespace"` 包装 MCP server 工具集 — 实测
            // `~/.codex/config.toml` 配的每个 `[mcp_servers.<name>]` 在入站
            // Responses API body 里都是一个 `{type:"namespace", name:"mcp__<name>__",
            // tools:[ {type:"function", ...}, ... ]}` 包,内层 5-26 个具体 function。
            // 第三方 chat completions provider 不认 namespace type,**必须递归
            // 展平内层 functions 为顶级 tool 数组**,模型才能看到具体 MCP tools
            // 像 `notion_create_pages` / `figma_get_file_data` 等并直接调用。
            //
            // 借鉴 `7as0nch/mimo2codex` `src/translate/reqToChat.ts:232-250` 同名
            // namespace 展平逻辑(见 reqToChat 注释 "Shape we've seen in the wild")。
            //
            // 不做的:展平内层时**不**改写 tool name(实测内层 function name 已经
            // 自带前缀如 `migrate_pages_to_workers_guide`,无冲突风险);**不**保留
            // namespace 包裹元数据(模型只需看到具体 tool name + description 即可)。
            //
            // **⚠️ 跟 `gemini_native::request.rs::responses_tools_to_chat_tools`
            // 的 `"namespace"` 分支故意分歧**:那边把 `namespace.name + description`
            // 作 prefix 注入到每个内层 function.description(`[MCP server <n>: <d>]`
            // ...)。原因:Gemini 3.x 缺这层 server-level context 时倾向选"动作类"
            // 工具(误选 create 而非 search,user 实测)。Chat completions 上游
            // (OpenAI/Anthropic Messages)未观察到此 bias,故 chat 路径不注入,
            // 保持 wire 干净。如果要让两个路径行为一致,可以把 prefix 逻辑提到
            // 公共 helper — 但当前 chat 路径模型选择没问题,保持差异化最小风险。
            let Some(inner) = obj.get("tools").and_then(|v| v.as_array()) else {
                tracing::debug!(
                    namespace_name = ?obj.get("name").and_then(|v| v.as_str()),
                    "dropping namespace tool with no nested `tools` array"
                );
                return vec![];
            };
            inner
                .iter()
                .flat_map(|inner_tool| convert_responses_tool_to_chat_tool(inner_tool, provider))
                .collect()
        }
        // Codex.app 默认每轮都给 tools 数组传 `{type:"web_search",
        // external_web_access:true, search_content_types:["text","image"]}`
        // (实测 dump 确认),作为 Responses API 标准 server-side built-in。
        // 协议层一律 drop,不映射到任何第三方 provider 的原生 web search。
        // 模型统一走自研 web_search / web_fetch(MOC-190)+ MCP 路径联网。
        // [MOC-208]
        "web_search" | "web_search_preview" => convert_web_search_tool(obj, provider),
        // Codex 0.130+ 引入的 `tool_search` builtin — `Feature::ToolSearchAlwaysDeferMcpTools`
        // 启用后所有 MCP server tools 都 defer 到 `tool_search`,LLM 通过它 BM25 query
        // 来发现具体工具。chat completions API 不认 `type=tool_search`,降级成普通
        // function tool(name=tool_search,description+parameters 透传)→ 给 LLM 完整
        // hint 看到 deferred server 列表。LLM 调用后由 `converter.rs::close_tool_call`
        // 把 chat function_call wire 重写成 Responses API `tool_search_call`,Codex
        // router (`core/src/tools/router.rs:106-122`) 路由到 `ToolPayload::ToolSearch`
        // 内部 BM25 dispatch,返 `ResponseInputItem::ToolSearchOutput` 进下轮 input。
        //
        // Wire schema 实证:`openai/codex` upstream `protocol/src/models.rs:2674-2740`
        // (rust-v0.133.0 tag)的 roundtrip test。
        //
        // Refs: MOC-32 / Linear document
        // `moc-32-方向-b-transfer-adapter-全面对接-codex-0130-responses-api-tool-types`
        "tool_search" => {
            let description = obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // [MOC-296] 上游 description 末尾追加「按名补搜」规则(见常量 doc:
            // 被点名未注入的连接器工具,模型须先 exact-name 补搜再判不可用)。
            let description = format!("{description}{TOOL_SEARCH_BY_NAME_HINT}");
            let mut parameters = obj
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{},"required":[]}));
            // tool_search 也合成 strict:false 的 chat function;与 function 分支同样要补
            // 缺失的 required —— 透传 Codex 给的 parameters 若 all-optional/缺 required,
            // 同样会被严格中转网关 400 拒(MOC-188 同源,review 反馈)。先确保顶层
            // type:object,再补 required:[](恒 strict:false,故无条件补)。
            if let Some(po) = parameters.as_object_mut() {
                po.entry("type")
                    .or_insert_with(|| Value::String("object".into()));
            }
            crate::core::schema::ensure_object_schema_required(&mut parameters);
            vec![json!({
                "type": "function",
                "function": {
                    "name": "tool_search",
                    "description": description,
                    "parameters": parameters,
                    "strict": false,
                },
            })]
        }
        // [MOC-210] 注意:Codex 内置 `image_generation` 工具在这条 chat-format 通用通道里
        // **仍然 drop**(落下面 `other`)。出图履约只在 antigravity(走 gemini 请求侧的
        // `responses_tools_to_chat_tools`,且只对 antigravity flavor 暴露 image_gen)+ proxy
        // 响应流拦截实现 —— 没有任何 openai_chat-format provider 能履约 image_gen。若在此
        // 无条件降级成 function tool 暴露,普通 chat provider 的模型可能真去调它 → proxy 不
        // 拦截 → Codex 收到无 executor 的 function_call 卡等 function_call_output(code-review
        // I-1)。故这里不特判,保持 drop。
        // Responses 专属类型(local_shell / file_search / computer_use* /
        // code_interpreter / mcp 等)Chat 端点不认,丢弃。
        // warn_once 防多轮重发刷屏(借鉴 mimo2codex `reqToChat.ts:158-172` warnOnce)。
        other => {
            crate::warn_once_drop_tool(other);
            vec![]
        }
    }
}

/// Codex 入站的 `web_search` / `web_search_preview` 工具**一律 drop**,不再映射
/// 到任何第三方 provider 的原生 web search。[MOC-208]
///
/// **理由**:本项目自研 `web_search` / `web_fetch`(MOC-190,Chrome-based —— 浏览器
/// TLS 指纹 + headless 渲染 + 全文返回 + `read_url_local` 本地缓存)已统一覆盖所有
/// provider 的联网能力,够用且可控。继续借第三方原生 web search 弊大于利:计费不
/// 可控(如 Kimi `$web_search` 每次 $0.005)、各家形态/参数不一难维护、built-in
/// search 与 Codex 每轮必发的 function tools 共存常触发上游 400(Gemini
/// `google_search` + functionDeclarations 即此症)。模型改走自研 web_search +
/// web_fetch + MCP 路径联网,行为统一。
fn convert_web_search_tool(
    _obj: &serde_json::Map<String, Value>,
    _provider: Option<&Provider>,
) -> Vec<Value> {
    crate::warn_once_drop_tool("web_search:provider-native-disabled");
    vec![]
}

pub fn normalize_tool_choice(tool_choice: &Value) -> Value {
    let Some(obj) = tool_choice.as_object() else {
        return tool_choice.clone();
    };
    if obj
        .get("function")
        .and_then(|v| v.as_object())
        .and_then(|f| f.get("name"))
        .is_some()
    {
        return tool_choice.clone();
    }
    match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "auto" => Value::String("auto".into()),
        "none" => Value::String("none".into()),
        "required" | "tool" | "any" => Value::String("required".into()),
        "function" if obj.get("function").is_none() => Value::String("required".into()),
        _ => tool_choice.clone(),
    }
}
