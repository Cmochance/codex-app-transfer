//! apply_patch **pre-flight 自动修复**:在把 V4A patch 发给 Codex apply 之前,读目标文件比对,
//! 自动对齐**安全**的上下文失配(尾随空格 / 首尾空白差异),消灭 V4A 头号失败
//! `apply_patch verification failed: Failed to find expected lines`。
//!
//! ## 为什么需要
//! 弱一点的 chat 模型(非 OpenAI)在大文件上常无法逐字节复刻 `Update File` 的 context/删除行
//! (尾随空格、缩进、记忆偏差)→ Codex 找不到锚点 → apply 失败 → 模型整文件重写,浪费时间和 token。
//! 实测真机报错(rollout 地面真相)正是这类。
//!
//! ## 安全边界(绝不损坏文件 —— 对齐用户「不做破坏性降级」硬规则)
//! - **只动锚点**:`Update File` 里的 context(空格前缀)/ 删除(`-`)行。`+新增` 行**绝不改动**。
//! - **只在唯一匹配时修**:锚点块在文件里按「忽略尾随空格 / 首尾空白」找候选,**恰好一个**位置才对齐;
//!   0 个(模型真改错内容)或 ≥2 个(歧义)一律**原样放行**,交给 Codex parse_patch 暴露真坏,绝不靠猜。
//! - **Add File / Delete File 不碰**(无锚点,不涉及匹配)。读不到文件 / 无 cwd → 原样放行。
//! - 每条修复 / 放行都记进 apply-patch 诊断页,可审计。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

/// 一条 pre-flight 处理记录(给诊断页 / 日志)。
#[derive(Debug, Clone, PartialEq)]
pub struct Repair {
    /// patch 里的文件路径(相对,原样)。
    pub file: String,
    /// `repaired`(对齐了锚点)/ `clean`(本就精确匹配,未改)/ `skipped:<原因>`(放行未修)。
    pub kind: String,
    /// 人类可读详情(改了几行 / 为何放行)。
    pub detail: String,
}

impl Repair {
    fn to_value(&self) -> Value {
        json!({"file": self.file, "kind": self.kind, "detail": self.detail})
    }
}

/// 把一组 [`Repair`] 转成诊断 `Value` 数组(给 ApplyPatchTrace 的 `repairs` 字段)。
pub fn repairs_to_value(repairs: &[Repair]) -> Value {
    Value::Array(repairs.iter().map(Repair::to_value).collect())
}

/// [MOC-194] 进程级「最近一次见到的 cwd」缓存。`optimize_patch` 用它跨请求记忆:Codex 只在
/// turn 开头请求发 `<cwd>`,apply_patch 工具循环后续请求 inbound 不带 cwd,不记忆则读盘规则全程
/// 失效。带 cwd 的请求更新缓存,不带的回退到缓存值。
static LAST_CWD: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// 当前请求带 cwd 则更新缓存并返回;不带则返回缓存的「最近 cwd」(可能为 None)。
fn remember_or_recall_cwd(cwd: Option<&str>) -> Option<String> {
    let cell = LAST_CWD.get_or_init(|| Mutex::new(None));
    let Ok(mut last) = cell.lock() else {
        return cwd.map(str::to_owned);
    };
    match cwd {
        Some(c) if !c.is_empty() => {
            *last = Some(c.to_owned());
            last.clone()
        }
        _ => last.clone(),
    }
}

/// 从 Codex Responses 请求里抽 `<cwd>...</cwd>`(Codex 注入的 environment_context 块,
/// 形如 `<environment_context>\n  <cwd>/abs/path</cwd>\n  <shell>zsh</shell>...`)。
/// 直接在序列化后的请求 JSON 文本里找(不依赖它落在 instructions 还是某条 input message)。
pub fn extract_cwd(request: Option<&Value>) -> Option<String> {
    let req = request?;
    let s = serde_json::to_string(req).ok()?;
    let start = s.find("<cwd>")? + "<cwd>".len();
    let rest = &s[start..];
    let end = rest.find("</cwd>")?;
    let cwd = rest[..end].trim();
    if cwd.is_empty() {
        None
    } else {
        Some(cwd.to_owned())
    }
}

/// [MOC-194 关键] 把请求里的 `<cwd>` 记入进程级缓存。**必须对每个请求调用**(不止 apply_patch):
/// 带 `<cwd>` 的是 **turn-start 请求**(不产生 apply_patch、不调 [`optimize_patch`]),而 apply_patch
/// 出现在**不带 cwd 的工具循环后续请求**里。只在 `optimize_patch` 里记忆 → 永远学不到 cwd(实测:
/// `LAST_CWD` 一直 None、所有 Tier B 读盘规则全程 no-op)。故记忆点必须在每请求都经过的地方
/// (转换器 `with_original_request`),turn-start 的 cwd 才能被后续 apply_patch 请求回退到。
pub fn remember_cwd_from_request(request: Option<&Value>) {
    if let Some(cwd) = extract_cwd(request) {
        let _ = remember_or_recall_cwd(Some(&cwd));
    }
}

/// apply_patch **中间层总入口**:按白名单规则**逐条恢复已知格式错误**,使模型不遵循 prompt 时
/// 产出的畸形 patch 仍能被 Codex 正确 apply。**只动确定的已知坑;未知一律原样放行(不猜不丢)。**
///
/// 两层结构(对齐 [[MOC-194]] 方案):
///
/// **Tier A 语法规整**(镜像 Codex 给 GPT 的 lark 语法,纯字符串、不读盘 —— 把 GPT 靠语法约束生成
/// 保证的合法性,在第三方 chat 路径事后保证):
/// - [`strip_trailing_at`] — 双边 `@@ … @@` → 单边(grammar `change_context: "@@" | "@@ " /(.+)/`;实测 18×)。
/// - [`ensure_add_file_plus`] — Add File 内容行漏 `+` → 补全(grammar `add_line: "+" /(.*)/`,Add File 无歧义)。
/// - [`ensure_v4a_envelope`] — 缺 `*** Begin/End Patch` → 补全(grammar `start: begin_patch hunk+ end_patch`;
///   gotcha #6 + 真机 seq230)。**仅 `json_complete`(非流式截断)时做**,且**放最后**以包裹 Tier B 产物。
///
/// **Tier B 语义恢复**(grammar 管不到的文件状态/内容层,需 `cwd` 读盘):
/// - [`recover_update_empty_file`] — Update 空文件 → Delete+Add(实测 50×,无损)。
/// - [`align_at_headers`] — `@@ <header>` 残缺锚点 → 对齐文件真实整行(`Failed to find context`)。
/// - [`fix_unprefixed_lines`] — Update 内无前缀行 → 按文件判定补 context 空格 / 删重复废行(seq235)。
/// - [`recover_empty_move`] — 空 Update+Move(rename-only)→ Delete+Add 复制原内容(实测 76×)。
/// - [`preflight_repair`] — Update 上下文 byte-exact 失配 → 读盘对齐(实测 134×)。
///
/// 未覆盖的错点:**原样透过**,交 Codex applier 报错(不猜不丢)。
/// `json_complete`:调用方传 `detect_json_truncation(args).is_none()`(chat);gemini args 一次性完整传 `true`。
pub fn optimize_patch(v4a: &str, cwd: Option<&str>, json_complete: bool) -> (String, Vec<Repair>) {
    // [MOC-194] **两类 cwd,分流使用**:
    // - `fresh_cwd` = 当前请求自带的 `<cwd>`(apply_patch 请求通常 None)。**判定文件 == Codex 应用
    //   文件**,可信。
    // - `recall_cwd` = 跨请求记忆的最近 cwd(Codex 只在 turn-start 请求发 `<cwd>`,apply_patch 工具
    //   循环后续请求不带 → 不记忆则读盘规则全 no-op)。可能 stale(多项目并发时被别项目污染)。
    //
    // **状态改写规则**(`recover_update_empty_file` / `recover_empty_move`:把 Update 转成 Delete+Add)
    // 的判定文件(读 recall_cwd)与应用文件(Codex 用 patch 相对路径在真实 cwd 应用)**可能不是同一个**
    // → stale cwd 下会删错项目的同名文件(破坏性)。故这两条**只用 fresh_cwd**(判定==应用才安全);
    // apply_patch 请求无 fresh cwd → 自动跳过透过(安全)。
    // **byte-exact 对齐规则**(align/preflight/fix_unprefixed)用 recall_cwd:最坏 stale 也只是「文件不存在
    // / 不唯一匹配 / byte 不符」→ 安全 no-op,不会误改。
    let fresh_cwd = cwd;
    let recalled = remember_or_recall_cwd(cwd);
    let recall_cwd = recalled.as_deref();
    let mut repairs = Vec::new();
    let mut s = v4a.to_owned();

    // ── Tier A 语法规整(纯字符串)──
    let (s1, r1) = strip_trailing_at(&s);
    s = s1;
    repairs.extend(r1);

    let (s_g, r_g) = ensure_add_file_plus(&s);
    s = s_g;
    repairs.extend(r_g);

    // ── Tier B 语义恢复 ──
    // 注:`Add File 已存在 → Delete+Add 覆盖` 规则**已撤销**(2026-06-09)。它会覆盖已有文件、
    // 可能丢失 Add 内容里没有的现存内容(破坏性降级);且会抢走模型收到 `already exists` 后
    // 自纠为**针对性 Update**(无损)的机会。改为原样透过、交 Codex 报 `already exists` 让模型自纠。
    //
    // 状态改写规则 → **fresh_cwd**(防 stale 删错文件,见上)。
    let (s_f, r_f) = recover_update_empty_file(&s, fresh_cwd);
    s = s_f;
    repairs.extend(r_f);

    let (s3, r3) = recover_empty_move(&s, fresh_cwd);
    s = s3;
    repairs.extend(r3);

    // byte-exact 对齐规则 → recall_cwd(最坏安全 no-op)。
    let (s_h, r_h) = align_at_headers(&s, recall_cwd);
    s = s_h;
    repairs.extend(r_h);

    let (s_u, r_u) = fix_unprefixed_lines(&s, recall_cwd);
    s = s_u;
    repairs.extend(r_u);

    let (s2, r2) = preflight_repair(&s, recall_cwd);
    s = s2;
    repairs.extend(r2);

    // ── 信封补全放最后:包裹 Tier B 可能新增的 Delete+Add 等结构 ──
    if json_complete {
        let (s4, r4) = ensure_v4a_envelope(&s);
        s = s4;
        if let Some(r) = r4 {
            repairs.push(r);
        }
    }
    (s, repairs)
}

/// **规则:双边 `@@ … @@` → 单边 `@@ …`**(prompt gotcha #1 / chat-path #1)。V4A 的 `@@` 是
/// **单边** anchor(`@@ <header>`);模型常写成双边 `@@ <header> @@`,Codex 把尾部 `@@` 当字面文本
/// → `Failed to find context '... @@'`。仅处理**列 0 的 `@@` 头行**(正文行有 `+`/`-`/空格 前缀,不碰),
/// 去掉尾部 `@@` 及其前导空白;**裸 `@@`(section 分隔)不动**。
fn strip_trailing_at(v4a: &str) -> (String, Vec<Repair>) {
    let mut changed = 0usize;
    let out: Vec<String> = v4a
        .lines()
        .map(|l| {
            if l.starts_with("@@") {
                let t = l.trim_end();
                // 裸 `@@`(len==2)是合法 section 分隔,跳过;`@@ x @@` 才去尾。
                if t.len() > 2 && t.ends_with("@@") {
                    let body = t[..t.len() - 2].trim_end();
                    if !body.is_empty() && body != "@@" {
                        changed += 1;
                        return body.to_owned();
                    }
                }
            }
            l.to_owned()
        })
        .collect();
    let mut joined = out.join("\n");
    if v4a.ends_with('\n') {
        joined.push('\n');
    }
    let repairs = if changed > 0 {
        vec![Repair {
            file: "(@@ header)".to_owned(),
            kind: "repaired".to_owned(),
            detail: format!("双边 @@ → 单边: {changed} 行(prompt gotcha #1)"),
        }]
    } else {
        Vec::new()
    };
    (joined, repairs)
}

/// **规则 G:Add File 内容行漏 `+` 前缀 → 补全**(grammar `add_hunk: … add_line+`、
/// `add_line: "+" /(.*)/`)。Add File 语义 = 后续每行都是新文件的**字面内容**、必须 `+` 前缀;
/// 模型偶尔漏写 `+` → Codex 不认作内容。Add File section 内**无歧义**(全是新增),给非 `+` 行
/// 统一补 `+`(空行 → 裸 `+`);已是 `+` 的不动(不重复成 `++`)。纯字符串、不读盘。
fn ensure_add_file_plus(v4a: &str) -> (String, Vec<Repair>) {
    if !v4a.contains("*** Add File:") {
        return (v4a.to_owned(), Vec::new());
    }
    let lines: Vec<&str> = v4a.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut repairs = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(path) = lines[i].strip_prefix("*** Add File: ") {
            out.push(lines[i].to_owned()); // header
            i += 1;
            let mut fixed = 0usize;
            // body 到下一个 `*** ` 控制行 / EOF;Add File body 全是 `+` 内容行。
            while i < lines.len() && !lines[i].starts_with("*** ") {
                if lines[i].starts_with('+') {
                    out.push(lines[i].to_owned());
                } else {
                    out.push(format!("+{}", lines[i]));
                    fixed += 1;
                }
                i += 1;
            }
            if fixed > 0 {
                repairs.push(Repair {
                    file: path.trim().to_owned(),
                    kind: "repaired".to_owned(),
                    detail: format!("Add File {fixed} 行漏 `+` 前缀 → 补全(lark add_line)"),
                });
            }
        } else {
            out.push(lines[i].to_owned());
            i += 1;
        }
    }
    let mut joined = out.join("\n");
    if v4a.ends_with('\n') {
        joined.push('\n');
    }
    (joined, repairs)
}

/// **规则:`@@ <header>` 锚点对齐文件真实行**(真机 seq181:`Failed to find context 'X'`)。
/// V4A 的 `@@ <header>` 是单边锚点,Codex 按**精确整行**匹配文件里的 section 行;模型常写**残缺**
/// 头(如 `@@ 系统架构建议`,而文件真实行是 `## 6. 系统架构建议`)→ 找不到锚点。当 `<header>` 不是
/// 文件里任何**整行**、但**恰好唯一包含于**某一文件行时,把 `@@ <header>` 对齐成 `@@ <该文件整行>`;
/// 0 个 / 多个包含 → 歧义,原样放行(不猜)。裸 `@@`(无 header)不动。需 `cwd`。
fn align_at_headers(v4a: &str, cwd: Option<&str>) -> (String, Vec<Repair>) {
    let Some(cwd) = cwd else {
        return (v4a.to_owned(), Vec::new());
    };
    if !v4a.contains("*** Update File:") {
        return (v4a.to_owned(), Vec::new());
    }
    let lines: Vec<&str> = v4a.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut repairs = Vec::new();
    let mut file_lines: Vec<String> = Vec::new();
    let mut have_file = false;
    let mut fixed = 0usize;
    let mut i = 0;
    while i < lines.len() {
        if let Some(path) = lines[i].strip_prefix("*** Update File: ") {
            // 切到新 Update File section → 载入该文件行
            file_lines = std::fs::read_to_string(resolve_path(path.trim(), cwd))
                .map(|c| c.lines().map(str::to_owned).collect())
                .unwrap_or_default();
            have_file = !file_lines.is_empty();
            out.push(lines[i].to_owned());
            i += 1;
            continue;
        }
        // `@@ <header>` 锚点(非裸 `@@`),且文件已载入
        if have_file {
            if let Some(header) = lines[i].strip_prefix("@@ ") {
                let h = header.trim();
                if !h.is_empty() && !file_lines.iter().any(|fl| fl == h) {
                    let hits: Vec<&String> =
                        file_lines.iter().filter(|fl| fl.contains(h)).collect();
                    if hits.len() == 1 {
                        out.push(format!("@@ {}", hits[0]));
                        fixed += 1;
                        i += 1;
                        continue;
                    }
                }
            }
        }
        out.push(lines[i].to_owned());
        i += 1;
    }
    if fixed > 0 {
        repairs.push(Repair {
            file: "(@@ anchor)".to_owned(),
            kind: "repaired".to_owned(),
            detail: format!("@@ 锚点残缺 → 对齐文件真实整行: {fixed} 处(Failed to find context)"),
        });
    }
    let mut joined = out.join("\n");
    if v4a.ends_with('\n') {
        joined.push('\n');
    }
    (joined, repairs)
}

/// **规则:`Update File` 目标是空文件 → `Delete File + Add File`**(prompt gotcha #3,无损)。
/// `*** Update File:` 无法作用于空文件(Codex 报 `cannot operate on a completely empty file`)。
/// 当目标文件存在且**为空**(全空白)、且 Update body 是**纯 `+` 行**(纯写内容,无 `-`/context 可
/// 匹配)时,转成 `*** Delete File: X` + `*** Add File: X` + 原 `+` body(空文件无内容可丢 → 无损)。
/// body 含 `-`/context(模型在空文件上写了匹配行,本就矛盾)/ 含 Move(交给 empty-move 规则)→ 不动。需 `cwd`。
fn recover_update_empty_file(v4a: &str, cwd: Option<&str>) -> (String, Vec<Repair>) {
    let Some(cwd) = cwd else {
        return (v4a.to_owned(), Vec::new());
    };
    if !v4a.contains("*** Update File:") {
        return (v4a.to_owned(), Vec::new());
    }
    let lines: Vec<&str> = v4a.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut repairs = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(path) = lines[i].strip_prefix("*** Update File: ") {
            let p = path.trim();
            let is_empty = std::fs::read_to_string(resolve_path(p, cwd))
                .map(|c| c.trim().is_empty())
                .unwrap_or(false);
            if is_empty {
                let body_start = i + 1;
                let mut j = body_start;
                while j < lines.len() && !lines[j].starts_with("*** ") {
                    j += 1;
                }
                let body = &lines[body_start..j];
                let has_move = body
                    .first()
                    .map(|l| l.starts_with("*** Move to:"))
                    .unwrap_or(false);
                let content: Vec<&&str> = body
                    .iter()
                    .filter(|l| !l.trim().is_empty() && !l.starts_with("@@"))
                    .collect();
                let all_plus = !content.is_empty() && content.iter().all(|l| l.starts_with('+'));
                if !has_move && all_plus {
                    out.push(format!("*** Delete File: {p}"));
                    out.push(format!("*** Add File: {p}"));
                    for b in body {
                        if b.starts_with('+') {
                            out.push((*b).to_owned());
                        }
                    }
                    repairs.push(Repair {
                        file: p.to_owned(),
                        kind: "repaired".to_owned(),
                        detail: "Update 空文件 → Delete+Add 写入(prompt gotcha #3)".to_owned(),
                    });
                    i = j;
                    continue;
                }
            }
        }
        out.push(lines[i].to_owned());
        i += 1;
    }
    let mut joined = out.join("\n");
    if v4a.ends_with('\n') {
        joined.push('\n');
    }
    (joined, repairs)
}

/// **规则:空 `Update File + Move to`(rename-only)→ `Delete File + Add File`**(prompt gotcha #7)。
/// 模型想纯重命名却写 `*** Update File: X` + `*** Move to: Y` 且**无 hunk** → Codex 报
/// `Update file hunk for path 'X' is empty`。按 prompt **自身建议**恢复:读 X 原内容,转成
/// `*** Delete File: X` + `*** Add File: Y` + 逐行 `+` 复制(空行为裸 `+`)。读不到 X → 原样放行。
fn recover_empty_move(v4a: &str, cwd: Option<&str>) -> (String, Vec<Repair>) {
    let Some(cwd) = cwd else {
        return (v4a.to_owned(), Vec::new());
    };
    if !v4a.contains("*** Move to:") {
        return (v4a.to_owned(), Vec::new());
    }
    let lines: Vec<&str> = v4a.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut repairs = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        // 匹配 `*** Update File: X` 紧跟 `*** Move to: Y`,且 Move 后到下一个 `*** ` 控制行之间无 hunk 行。
        if let Some(old) = lines[i].strip_prefix("*** Update File: ") {
            if i + 1 < lines.len() {
                if let Some(new) = lines[i + 1].strip_prefix("*** Move to: ") {
                    // 看 Move 之后、下一个**文件操作**控制行之前有没有 hunk 内容行。
                    // 注:`*** End of File` 是文档化的 **hunk 内标记**(prompt RENAME/MOVE 段),不是
                    // section 边界 —— 不能停在它(否则 rename+EOF 追加会被误判成空 rename、转成丢内容的
                    // Delete+Add,codex-connector #435 P1)。它本身即表示「有 hunk」,继续往后扫。
                    let mut j = i + 2;
                    let mut has_hunk = false;
                    while j < lines.len() {
                        let t = lines[j];
                        if t.trim_end() == "*** End of File" {
                            has_hunk = true;
                            j += 1;
                            continue;
                        }
                        if t.starts_with("*** ") {
                            break; // 真正的下一个文件操作 / End Patch 边界
                        }
                        if t.starts_with('+')
                            || t.starts_with('-')
                            || t.starts_with(' ')
                            || t.starts_with("@@")
                        {
                            has_hunk = true;
                        }
                        j += 1;
                    }
                    if !has_hunk {
                        // 空 rename-only → 读原文件转 Delete+Add。读不到 / 内容为空 → 不转(空 Add File
                        // 体可能被 Codex 拒)→ 原样放行交 Codex 处理。
                        let abs = resolve_path(old.trim(), cwd);
                        match std::fs::read_to_string(&abs) {
                            Ok(content) if !content.is_empty() => {
                                out.push(format!("*** Delete File: {}", old.trim()));
                                out.push(format!("*** Add File: {}", new.trim()));
                                for cl in content.lines() {
                                    out.push(format!("+{cl}"));
                                }
                                repairs.push(Repair {
                                    file: old.trim().to_owned(),
                                    kind: "repaired".to_owned(),
                                    detail: format!(
                                        "空 Update+Move(rename-only)→ Delete+Add 复制原内容 → {}(prompt gotcha #7)",
                                        new.trim()
                                    ),
                                });
                                i = j; // 跳过原 Update/Move(+空体)
                                continue;
                            }
                            _ => {
                                repairs.push(Repair {
                                    file: old.trim().to_owned(),
                                    kind: "skipped:unreadable_or_empty".to_owned(),
                                    detail: "空 Update+Move 但原文件读不到 / 为空 → 原样放行"
                                        .to_owned(),
                                });
                            }
                        }
                    }
                }
            }
        }
        out.push(lines[i].to_owned());
        i += 1;
    }
    let mut joined = out.join("\n");
    if v4a.ends_with('\n') {
        joined.push('\n');
    }
    (joined, repairs)
}

/// **缺信封自动补全**:模型常只写 `*** Add/Update File:` + 内容,漏掉 `*** Begin Patch` /
/// `*** End Patch` 头尾 → Codex(及本 adapter 的 V4A 校验)判 incomplete → 模型被迫重试。
/// 当 patch 含至少一个 `*** Add/Update/Delete File:` 操作、JSON 已完整(调用方先 gate
/// `detect_json_truncation` 为 None 才调本函数,确保不是流式截断)、但缺 Begin/End 信封时,
/// **纯补标记**(不改一字节内容、不猜),返回 `(补全后, Some(Repair))`;本就完整 / 非 patch 体
/// 返回 `(原样, None)`。
///
/// 安全:缺 Begin 时**仅当首个非空行就是操作行**才在最前补 `*** Begin Patch`(有前导散文则不动,
/// 交给 `repair_v4a_envelope` / Codex);缺 End 时去尾随空白后补 `*** End Patch`。
pub fn ensure_v4a_envelope(input: &str) -> (String, Option<Repair>) {
    let is_op = |l: &str| {
        let t = l.trim_end();
        t.starts_with("*** Add File:")
            || t.starts_with("*** Update File:")
            || t.starts_with("*** Delete File:")
    };
    if !input.lines().any(is_op) {
        return (input.to_owned(), None); // 不是可识别的 patch 体,不碰
    }
    let has_begin = input.lines().any(|l| l.trim_end() == "*** Begin Patch");
    let has_end = input.lines().any(|l| l.trim_end() == "*** End Patch");
    if has_begin && has_end {
        return (input.to_owned(), None);
    }
    let mut body = input.to_owned();
    let mut added: Vec<&str> = Vec::new();
    if !has_begin {
        // 仅当首个非空行就是操作行才安全(无前导散文混入信封内)。
        let first_nonempty = input.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        if !is_op(first_nonempty) {
            return (input.to_owned(), None);
        }
        body = format!("*** Begin Patch\n{body}");
        added.push("Begin Patch");
    }
    if !has_end {
        let trimmed = body.trim_end();
        body = format!("{trimmed}\n*** End Patch");
        added.push("End Patch");
    }
    (
        body,
        Some(Repair {
            file: "(envelope)".to_owned(),
            kind: "repaired".to_owned(),
            detail: format!("模型漏写信封,自动补全: {}", added.join(" + ")),
        }),
    )
}

/// **规则:Update body 内**无前缀行**按文件判定补全**(真机 seq235:单行漏前缀 → validate 拒 →
/// 整份 Update 重写浪费)。grammar `change_line: ("+"|"-"|" ") /(.*)/` 要求每行带前缀;模型偶尔
/// 漏写一行的前缀。**非破坏性**修(只补前缀 / 删可证重复的废行,绝不丢内容):
/// - 无前缀行**与相邻 `+<同内容>` 行重复**(模型写了两遍)→ 删该废行(内容在 `+` 行里,不丢);
/// - 否则无前缀**非空**行**在目标文件里有完全相同的整行** → 它是 context 行漏了空格 → 补 ` `
///   (合法 context 且 byte-exact;最不破坏的解释:行保留。模型若本意是删,顶多没删成、无数据损失);
/// - 其余(不在文件、非重复、空行)→ 原样透过,交 validate 报错让模型自纠(不猜)。
///
/// 仅作用于 `*** Update File:` section(Add File 的漏 `+` 由 [`ensure_add_file_plus`] 管)。需 `cwd`。
fn fix_unprefixed_lines(v4a: &str, cwd: Option<&str>) -> (String, Vec<Repair>) {
    let Some(cwd) = cwd else {
        return (v4a.to_owned(), Vec::new());
    };
    if !v4a.contains("*** Update File:") {
        return (v4a.to_owned(), Vec::new());
    }
    let lines: Vec<&str> = v4a.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut repairs = Vec::new();
    let mut in_update = false;
    let mut file_lines: Vec<String> = Vec::new();
    let mut drop_dups = 0usize;
    let mut add_ctx = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if let Some(path) = l.strip_prefix("*** Update File: ") {
            in_update = true;
            file_lines = std::fs::read_to_string(resolve_path(path.trim(), cwd))
                .map(|c| c.lines().map(str::to_owned).collect())
                .unwrap_or_default();
            out.push(l.to_owned());
            i += 1;
            continue;
        }
        if l.starts_with("*** ") {
            in_update = false; // 任何其它控制行结束 Update body
            out.push(l.to_owned());
            i += 1;
            continue;
        }
        let first = l.chars().next();
        let valid = matches!(first, Some('+') | Some('-') | Some(' '))
            || l.starts_with("@@")
            || l.is_empty();
        if in_update && !valid {
            // case1:与相邻 `+<同内容>` 重复的废行 → 删(内容在 + 行里,不丢)
            let plus_dup = format!("+{l}");
            let next_dup = lines.get(i + 1).map(|n| *n == plus_dup).unwrap_or(false);
            let prev_dup = out.last().map(|o| o == &plus_dup).unwrap_or(false);
            if next_dup || prev_dup {
                drop_dups += 1;
                i += 1;
                continue;
            }
            // case2:文件里有完全相同整行 → context 漏空格 → 补 ` `
            if file_lines.iter().any(|fl| fl == l) {
                out.push(format!(" {l}"));
                add_ctx += 1;
                i += 1;
                continue;
            }
            // else:透过(不猜)
        }
        out.push(l.to_owned());
        i += 1;
    }
    if drop_dups + add_ctx > 0 {
        repairs.push(Repair {
            file: "(unprefixed)".to_owned(),
            kind: "repaired".to_owned(),
            detail: format!(
                "Update 无前缀行修复: 补 context 空格 {add_ctx} / 删重复废行 {drop_dups}(lark change_line)"
            ),
        });
    }
    let mut joined = out.join("\n");
    if v4a.ends_with('\n') {
        joined.push('\n');
    }
    (joined, repairs)
}

/// 对 V4A patch 做 pre-flight 修复。`cwd` 用于把 patch 的相对路径解析到真实文件。
/// 返回 `(修复后 V4A, 处理记录)`。无 cwd / 无 `Update File` / 读不到文件时 V4A 原样返回。
pub fn preflight_repair(v4a: &str, cwd: Option<&str>) -> (String, Vec<Repair>) {
    let Some(cwd) = cwd else {
        return (v4a.to_owned(), Vec::new());
    };
    // 没有任何 Update File 直接短路(Add/Delete File 不涉及锚点匹配)。
    if !v4a.contains("*** Update File:") {
        return (v4a.to_owned(), Vec::new());
    }
    let mut repairs = Vec::new();
    let lines: Vec<&str> = v4a.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            out.push(line.to_owned());
            i += 1;
            // 收集本 Update File section 的 body(到下一个 `*** ` 控制行为止)。
            let body_start = i;
            while i < lines.len() && !lines[i].starts_with("*** ") {
                i += 1;
            }
            let body = &lines[body_start..i];
            let (repaired_body, rep) = repair_update_section(path.trim(), body, cwd);
            out.extend(repaired_body);
            repairs.push(rep);
        } else {
            out.push(line.to_owned());
            i += 1;
        }
    }
    // 保留尾随换行语义:lines() 丢掉末尾换行,join 后若原文以 \n 结尾则补上。
    let mut joined = out.join("\n");
    if v4a.ends_with('\n') {
        joined.push('\n');
    }
    (joined, repairs)
}

/// 修复一个 `Update File` section 的 body。`path` 是 patch 里的(相对)路径。
fn repair_update_section(path: &str, body: &[&str], cwd: &str) -> (Vec<String>, Repair) {
    let abs = resolve_path(path, cwd);
    let Ok(content) = std::fs::read_to_string(&abs) else {
        return (
            body.iter().map(|l| (*l).to_owned()).collect(),
            Repair {
                file: path.to_owned(),
                kind: "skipped:unreadable".to_owned(),
                detail: format!("读不到文件 {} → 原样放行", abs.display()),
            },
        );
    };
    let file_lines: Vec<&str> = content.lines().collect();

    // 把 body 切成 hunk(按 `@@` 行分段;`@@` 行本身保留、不参与锚点匹配)。
    let mut new_body: Vec<String> = Vec::with_capacity(body.len());
    let mut repaired_hunks = 0;
    let mut clean_hunks = 0;
    let mut skipped: Vec<String> = Vec::new();
    let mut hunk: Vec<&str> = Vec::new();
    let flush = |hunk: &mut Vec<&str>,
                 new_body: &mut Vec<String>,
                 repaired_hunks: &mut usize,
                 clean_hunks: &mut usize,
                 skipped: &mut Vec<String>| {
        if hunk.is_empty() {
            return;
        }
        match repair_hunk(hunk, &file_lines) {
            HunkOutcome::Clean => {
                *clean_hunks += 1;
                new_body.extend(hunk.iter().map(|l| (*l).to_owned()));
            }
            HunkOutcome::Repaired(fixed) => {
                *repaired_hunks += 1;
                new_body.extend(fixed);
            }
            HunkOutcome::Skipped(reason) => {
                skipped.push(reason);
                new_body.extend(hunk.iter().map(|l| (*l).to_owned()));
            }
        }
        hunk.clear();
    };

    for &l in body {
        if l.starts_with("@@") {
            flush(
                &mut hunk,
                &mut new_body,
                &mut repaired_hunks,
                &mut clean_hunks,
                &mut skipped,
            );
            new_body.push(l.to_owned());
        } else {
            hunk.push(l);
        }
    }
    flush(
        &mut hunk,
        &mut new_body,
        &mut repaired_hunks,
        &mut clean_hunks,
        &mut skipped,
    );

    let kind = if repaired_hunks > 0 {
        "repaired"
    } else if skipped.is_empty() {
        "clean"
    } else {
        "skipped:no_unique_match"
    };
    let detail = format!(
        "hunk: 修复 {repaired_hunks} / 本就匹配 {clean_hunks} / 放行 {}{}",
        skipped.len(),
        if skipped.is_empty() {
            String::new()
        } else {
            format!(" ({})", skipped.join("; "))
        }
    );
    (
        new_body,
        Repair {
            file: path.to_owned(),
            kind: kind.to_owned(),
            detail,
        },
    )
}

enum HunkOutcome {
    /// 锚点精确匹配文件,无需改。
    Clean,
    /// 锚点对齐成文件真实字节后的整个 hunk(含原样的 `+` 行)。
    Repaired(Vec<String>),
    /// 未修(0 或多个匹配),附原因。
    Skipped(String),
}

/// 修一个 hunk:锚点 = context(空格前缀)+ 删除(`-`)行的**内容**(去前缀),按序应是文件里的
/// 连续块。精确匹配→Clean;否则按「忽略尾随空格 / 首尾空白」找候选,唯一→对齐,否则放行。
fn repair_hunk(hunk: &[&str], file_lines: &[&str]) -> HunkOutcome {
    // 锚点行在 hunk 里的下标 + 内容(去单字符前缀)。
    let anchors: Vec<(usize, &str)> = hunk
        .iter()
        .enumerate()
        .filter_map(|(idx, l)| match l.chars().next() {
            Some(' ') => Some((idx, &l[1..])),
            Some('-') => Some((idx, &l[1..])),
            _ => None, // '+' 新增行 / 空行 / 其它不作锚点
        })
        .collect();
    if anchors.is_empty() {
        return HunkOutcome::Clean; // 纯新增,无锚点
    }
    let anchor_contents: Vec<&str> = anchors.iter().map(|(_, c)| *c).collect();

    // 精确匹配:文件里存在连续块完全等于锚点内容 → 无需修(Codex 自己能找到)。
    if !find_block(file_lines, &anchor_contents, |a, b| a == b).is_empty() {
        return HunkOutcome::Clean;
    }

    // 模糊匹配:逐行「忽略尾随空格」相等;仍 0 个再退「首尾空白都忽略」。
    let mut matches = find_block(file_lines, &anchor_contents, |a, b| {
        a.trim_end() == b.trim_end()
    });
    let mut mode = "尾随空格";
    if matches.is_empty() {
        matches = find_block(file_lines, &anchor_contents, |a, b| a.trim() == b.trim());
        mode = "首尾空白";
    }
    match matches.len() {
        1 => {
            let pos = matches[0];
            // 把锚点行对齐成文件真实字节(保留 hunk 里 +/- /空格 的交错与 `+` 行)。
            let mut fixed: Vec<String> = hunk.iter().map(|l| (*l).to_owned()).collect();
            for (k, (idx, _)) in anchors.iter().enumerate() {
                let prefix = hunk[*idx].chars().next().unwrap(); // ' ' 或 '-'
                let file_line = file_lines[pos + k];
                fixed[*idx] = format!("{prefix}{file_line}");
            }
            HunkOutcome::Repaired(fixed)
        }
        n if n > 1 => HunkOutcome::Skipped(format!("{mode}下 {n} 处匹配(歧义)")),
        // 0 连续匹配 → 试「忽略空行差异」(EP-1:模型漏/多写空行致整块失配)。锚点**非空行**序列
        // 在文件里唯一定位(允许文件该区间含模型漏写的空行),命中则用文件真实区间(含空行 + 字节)
        // 重建锚点,`+` 插入行保持原位。0/多处仍放行(不猜)。
        _ => {
            // blank-tolerant 重建会丢弃空白锚点行、改用文件空行 → 无法忠实表达「删除一个空行」的 `-`
            // (会被静默转成 context = 该删没删)。若 hunk 含空白行删除,放弃 blank-tolerant、透过(不猜)。
            let has_blank_deletion = hunk
                .iter()
                .any(|l| l.starts_with('-') && l[1..].trim().is_empty());
            if has_blank_deletion {
                return HunkOutcome::Skipped(
                    "含空白行删除,blank-tolerant 不安全 → 放行".to_owned(),
                );
            }
            let regions = find_regions_blank_tolerant(file_lines, &anchor_contents);
            match regions.len() {
                1 => {
                    let (s, e) = regions[0];
                    HunkOutcome::Repaired(rebuild_hunk_with_region(hunk, &file_lines[s..e]))
                }
                0 => HunkOutcome::Skipped("锚点在文件中 0 匹配(疑模型改错内容)".to_owned()),
                n => HunkOutcome::Skipped(format!("忽略空行下 {n} 处匹配(歧义)")),
            }
        }
    }
}

/// EP-1 辅助:在 `file_lines` 里找锚点**非空行**序列能唯一定位的区间(允许文件区间内含模型漏写的
/// 空行,但不允许有额外的非空行)。返回所有匹配区间 `[start, end)`(end 为最后一个匹配非空行的下一位)。
fn find_regions_blank_tolerant(
    file_lines: &[&str],
    anchor_contents: &[&str],
) -> Vec<(usize, usize)> {
    let nb: Vec<&str> = anchor_contents
        .iter()
        .map(|c| c.trim_end())
        .filter(|c| !c.trim().is_empty())
        .collect();
    if nb.is_empty() {
        return Vec::new();
    }
    let mut regions = Vec::new();
    for start in 0..file_lines.len() {
        if file_lines[start].trim().is_empty() || file_lines[start].trim_end() != nb[0] {
            continue;
        }
        let mut fi = start;
        let mut ai = 0;
        let mut ok = true;
        while ai < nb.len() {
            if fi >= file_lines.len() {
                ok = false;
                break;
            }
            let fl = file_lines[fi];
            if fl.trim().is_empty() {
                fi += 1; // 跳过文件空行(模型可能漏写)
                continue;
            }
            if fl.trim_end() == nb[ai] {
                ai += 1;
                fi += 1;
            } else {
                ok = false; // 出现额外非空行 → 此 start 不匹配
                break;
            }
        }
        if ok && ai == nb.len() {
            regions.push((start, fi));
        }
    }
    regions
}

/// EP-1 辅助:用文件真实区间 `region`(含空行)重建 hunk —— 锚点(context/`-`)对齐成文件字节、
/// 补回模型漏写的文件空行(作 context),`+` 插入行按 hunk 原序保持。模型自带的空白锚点行丢弃
/// (改用文件的空行,避免重复)。
fn rebuild_hunk_with_region(hunk: &[&str], region: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut fi = 0usize; // region 游标
    for &hl in hunk {
        match hl.chars().next() {
            Some('+') => out.push(hl.to_owned()), // 插入行原样保位
            Some(' ') | Some('-') => {
                let prefix = hl.chars().next().unwrap();
                let content = &hl[1..];
                if content.trim().is_empty() {
                    continue; // 模型的空锚点行丢弃,用文件空行
                }
                // 先补回文件里模型漏写的空行(作 context)
                while fi < region.len() && region[fi].trim().is_empty() {
                    out.push(format!(" {}", region[fi]));
                    fi += 1;
                }
                if fi < region.len() {
                    out.push(format!("{prefix}{}", region[fi]));
                    fi += 1;
                } else {
                    out.push(hl.to_owned());
                }
            }
            _ => {} // 无前缀空行等丢弃,用文件空行
        }
    }
    out
}

/// 在 `file_lines` 里找所有起点 `i`,使 `file_lines[i..i+anchor.len()]` 与 `anchor` 逐行 `eq` 为真。
/// 返回所有匹配起点。
fn find_block<F: Fn(&str, &str) -> bool>(
    file_lines: &[&str],
    anchor: &[&str],
    eq: F,
) -> Vec<usize> {
    if anchor.is_empty() || anchor.len() > file_lines.len() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for i in 0..=(file_lines.len() - anchor.len()) {
        if (0..anchor.len()).all(|k| eq(file_lines[i + k], anchor[k])) {
            hits.push(i);
        }
    }
    hits
}

/// 把 patch 路径解析到绝对路径。绝对路径原样;相对路径对 `cwd` 拼接。
fn resolve_path(path: &str, cwd: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_file(name: &str, content: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        (dir, name.to_owned())
    }

    #[test]
    fn extract_cwd_from_env_block() {
        let req = json!({
            "input": [{"type":"message","role":"user","content":"<environment_context>\n  <cwd>/Users/x/proj</cwd>\n  <shell>zsh</shell>\n</environment_context>"}]
        });
        assert_eq!(extract_cwd(Some(&req)).as_deref(), Some("/Users/x/proj"));
        assert_eq!(extract_cwd(None), None);
        assert_eq!(extract_cwd(Some(&json!({"input":[]}))), None);
    }

    #[test]
    fn trailing_whitespace_anchor_is_repaired_to_file_bytes() {
        // 文件 context 行无尾随空格;patch 的 context 行带尾随空格 → 应被对齐成文件真实字节。
        let (dir, name) = tmp_file("a.txt", "fn main() {\n    let x = 1;\n    let y = 2;\n}\n");
        let cwd = dir.path().to_str().unwrap();
        // patch: 在 `let x = 1;` 后加一行;context 带尾随空格(模型常见错)。
        let v4a = format!(
            "*** Begin Patch\n*** Update File: {name}\n    let x = 1;   \n+    let z = 9;\n    let y = 2;\n*** End Patch\n"
        );
        let (out, reps) = preflight_repair(&v4a, Some(cwd));
        assert!(
            out.contains("    let x = 1;\n"),
            "尾随空格应被对齐掉:\n{out}"
        );
        assert!(out.contains("+    let z = 9;"), "新增行保留");
        assert_eq!(reps[0].kind, "repaired", "{:?}", reps);
    }

    #[test]
    fn exact_match_left_clean() {
        let (dir, name) = tmp_file("b.txt", "alpha\nbeta\ngamma\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!("*** Begin Patch\n*** Update File: {name}\n alpha\n-beta\n+BETA\n gamma\n*** End Patch\n");
        let (out, reps) = preflight_repair(&v4a, Some(cwd));
        assert_eq!(reps[0].kind, "clean");
        assert_eq!(out, v4a, "精确匹配不改一字节");
    }

    #[test]
    fn ambiguous_match_is_skipped_not_guessed() {
        // 锚点 ` x` 在文件里多处出现 → 歧义 → 放行不猜。
        let (dir, name) = tmp_file("c.txt", "x\nx\nx\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a =
            format!("*** Begin Patch\n*** Update File: {name}\n x   \n+added\n*** End Patch\n");
        let (out, reps) = preflight_repair(&v4a, Some(cwd));
        assert!(reps[0].kind.starts_with("skipped"), "{:?}", reps);
        assert_eq!(out, v4a, "歧义不改");
    }

    #[test]
    fn no_match_skipped() {
        let (dir, name) = tmp_file("d.txt", "real content\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!("*** Begin Patch\n*** Update File: {name}\n model hallucinated line\n+x\n*** End Patch\n");
        let (out, reps) = preflight_repair(&v4a, Some(cwd));
        assert!(reps[0].kind.starts_with("skipped"));
        assert_eq!(out, v4a);
    }

    #[test]
    fn unreadable_file_passes_through() {
        let v4a = "*** Begin Patch\n*** Update File: nonexistent_zzz.txt\n a\n+b\n*** End Patch\n";
        let (out, reps) = preflight_repair(v4a, Some("/tmp/no_such_dir_xyz"));
        assert_eq!(out, v4a);
        assert_eq!(reps[0].kind, "skipped:unreadable");
    }

    #[test]
    fn envelope_added_when_model_omits_begin_end() {
        // 真机 seq230 形态:只有 Add File + 内容,无 Begin/End。
        let body = "*** Add File: outputs/x.md\n+# Title\n+body\n";
        let (out, rep) = ensure_v4a_envelope(body);
        assert!(out.starts_with("*** Begin Patch\n"), "{out}");
        assert!(out.trim_end().ends_with("*** End Patch"), "{out}");
        assert!(
            out.contains("+# Title") && out.contains("+body"),
            "内容不丢"
        );
        assert!(rep.is_some());
    }

    #[test]
    fn envelope_only_end_added() {
        let body = "*** Begin Patch\n*** Add File: x\n+a\n";
        let (out, rep) = ensure_v4a_envelope(body);
        assert_eq!(out.matches("*** Begin Patch").count(), 1, "不重复加 Begin");
        assert!(out.trim_end().ends_with("*** End Patch"));
        assert!(rep.unwrap().detail.contains("End Patch"));
    }

    #[test]
    fn envelope_complete_untouched() {
        let body = "*** Begin Patch\n*** Add File: x\n+a\n*** End Patch\n";
        let (out, rep) = ensure_v4a_envelope(body);
        assert_eq!(out, body);
        assert!(rep.is_none());
    }

    #[test]
    fn envelope_not_added_to_nonpatch_or_leading_prose() {
        // 非 patch 体不碰
        let (o1, r1) = ensure_v4a_envelope("just some text\nno ops here\n");
        assert_eq!(o1, "just some text\nno ops here\n");
        assert!(r1.is_none());
        // 缺 Begin 且首个非空行不是操作行(有前导散文)→ 不安全,不补 Begin
        let prose = "here is my patch:\n*** Add File: x\n+a\n*** End Patch\n";
        let (o2, _r2) = ensure_v4a_envelope(prose);
        assert!(
            !o2.starts_with("*** Begin Patch"),
            "有前导散文不应贸然补 Begin"
        );
    }

    #[test]
    fn strip_trailing_at_double_sided_to_single() {
        let v4a = "*** Begin Patch\n*** Update File: x\n@@ def f(): @@\n-a\n+b\n*** End Patch\n";
        let (out, reps) = strip_trailing_at(v4a);
        assert!(out.contains("@@ def f():\n"), "应去尾部 @@:\n{out}");
        assert!(!out.contains("@@ def f(): @@"));
        assert_eq!(reps.len(), 1);
    }

    #[test]
    fn strip_trailing_at_keeps_bare_and_single() {
        // 裸 @@(section 分隔)+ 单边 @@ 都不动
        let v4a = "*** Update File: x\n@@\n@@ class Foo\n-a\n+b\n";
        let (out, reps) = strip_trailing_at(v4a);
        assert_eq!(out, v4a);
        assert!(reps.is_empty());
    }

    #[test]
    fn recover_empty_move_to_delete_add() {
        let (dir, name) = tmp_file("old.md", "line1\nline2\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!(
            "*** Begin Patch\n*** Update File: {name}\n*** Move to: new.md\n*** End Patch\n"
        );
        let (out, reps) = recover_empty_move(&v4a, Some(cwd));
        assert!(out.contains(&format!("*** Delete File: {name}")), "{out}");
        assert!(out.contains("*** Add File: new.md"), "{out}");
        assert!(
            out.contains("+line1") && out.contains("+line2"),
            "复制原内容:\n{out}"
        );
        assert!(!out.contains("*** Move to:"), "Move 已被替换");
        assert_eq!(reps[0].kind, "repaired");
    }

    #[test]
    fn recover_empty_move_with_hunk_untouched() {
        // Update+Move 但**有** hunk(rename + 内容改)→ 不碰(prompt 允许)。
        let (dir, name) = tmp_file("old2.md", "a\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!(
            "*** Begin Patch\n*** Update File: {name}\n*** Move to: new2.md\n-a\n+b\n*** End Patch\n"
        );
        let (out, reps) = recover_empty_move(&v4a, Some(cwd));
        assert_eq!(out, v4a, "有 hunk 的 Move 不动");
        assert!(reps.is_empty());
    }

    #[test]
    fn rename_with_eof_marker_hunk_not_treated_as_empty() {
        // codex-connector #435 P1:rename + `*** End of File` 追加 hunk 不能被当空 rename(否则转成
        // 丢内容的 Delete+Add)→ 识别为有 hunk → 透过不转。
        let (dir, name) = tmp_file("eof_old.md", "a\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!(
            "*** Begin Patch\n*** Update File: {name}\n*** Move to: eof_new.md\n*** End of File\n+tail\n*** End Patch\n"
        );
        let (out, reps) = recover_empty_move(&v4a, Some(cwd));
        assert_eq!(out, v4a, "含 EOF hunk 的 rename 应透过不转:\n{out}");
        assert!(reps.is_empty(), "{:?}", reps);
    }

    #[test]
    fn add_on_existing_passes_through_unchanged() {
        // 规则 #2 已撤:Add 已存在文件**不再**转 Delete+Add(避免覆盖丢数据),原样透过让 Codex
        // 报 already exists、模型自纠为针对性 Update。
        let (dir, name) = tmp_file("exists.md", "important old content\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!("*** Begin Patch\n*** Add File: {name}\n+new content\n*** End Patch\n");
        let (out, reps) = optimize_patch(&v4a, Some(cwd), true);
        assert!(
            !out.contains("*** Delete File:"),
            "不应再插 Delete(已撤规则#2):\n{out}"
        );
        assert!(
            out.contains(&format!("*** Add File: {name}")),
            "Add 原样保留"
        );
        assert!(
            !reps.iter().any(|r| r.detail.contains("Delete File 覆盖")),
            "不应有覆盖类修复: {:?}",
            reps
        );
    }

    #[test]
    fn update_empty_file_to_delete_add() {
        let (dir, name) = tmp_file("empty.txt", "");
        let cwd = dir.path().to_str().unwrap();
        let v4a =
            format!("*** Begin Patch\n*** Update File: {name}\n+line1\n+line2\n*** End Patch\n");
        let (out, reps) = recover_update_empty_file(&v4a, Some(cwd));
        assert!(out.contains(&format!("*** Delete File: {name}")), "{out}");
        assert!(out.contains(&format!("*** Add File: {name}")), "{out}");
        assert!(out.contains("+line1") && out.contains("+line2"));
        assert!(!out.contains("*** Update File:"), "Update 已转换");
        assert_eq!(reps[0].kind, "repaired");
    }

    #[test]
    fn update_nonempty_file_not_converted() {
        let (dir, name) = tmp_file("nonempty.txt", "existing\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!(
            "*** Begin Patch\n*** Update File: {name}\n-existing\n+changed\n*** End Patch\n"
        );
        let (out, reps) = recover_update_empty_file(&v4a, Some(cwd));
        assert_eq!(out, v4a, "非空文件 Update 不碰");
        assert!(reps.is_empty());
    }

    #[test]
    fn add_file_missing_plus_prefix_is_filled() {
        // Add File 里有的行漏 `+`(模型常见)、有空行 → 全补 `+`,已有 `+` 的不动。
        let v4a = "*** Begin Patch\n*** Add File: new.md\n+# Title\nplain line no plus\n\n+already plus\n*** End Patch\n";
        let (out, reps) = ensure_add_file_plus(v4a);
        assert!(
            out.contains("\n+plain line no plus\n"),
            "漏 + 的行应补:\n{out}"
        );
        assert!(out.contains("\n+\n+already plus"), "空行 → 裸 +:\n{out}");
        assert!(!out.contains("++already plus"), "已是 + 的不重复");
        assert_eq!(reps[0].kind, "repaired");
        assert!(
            reps[0].detail.contains("2 行"),
            "漏 + 的 plain 行 + 空行 = 2: {:?}",
            reps
        );
    }

    #[test]
    fn add_file_all_plus_untouched_and_update_not_affected() {
        // 全 + 的 Add File 不动;Update section 的非 + 行(context/-)绝不被 G 碰。
        let v4a = "*** Begin Patch\n*** Add File: a\n+x\n+y\n*** Update File: b\n cont\n-old\n+new\n*** End Patch\n";
        let (out, reps) = ensure_add_file_plus(v4a);
        assert_eq!(out, v4a, "Add 全 + + Update 不动:\n{out}");
        assert!(reps.is_empty());
    }

    #[test]
    fn at_header_aligned_to_unique_file_line() {
        // 真机 seq181:@@ 头残缺(漏 `## 6. `),唯一包含于一个文件行 → 对齐。
        let (dir, name) = tmp_file("doc.md", "intro\n## 6. 系统架构建议\n建议分层\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!(
            "*** Begin Patch\n*** Update File: {name}\n@@ 系统架构建议\n 建议分层\n+新增一行\n*** End Patch\n"
        );
        let (out, reps) = align_at_headers(&v4a, Some(cwd));
        assert!(
            out.contains("@@ ## 6. 系统架构建议"),
            "@@ 应对齐成文件真实整行:\n{out}"
        );
        assert_eq!(reps[0].kind, "repaired");
    }

    #[test]
    fn at_header_exact_or_ambiguous_untouched() {
        // 已是文件真实整行 → 不动;多处包含(歧义)→ 不动。
        let (dir, name) = tmp_file("doc2.md", "## A\nx\n## A\n");
        let cwd = dir.path().to_str().unwrap();
        // 精确整行 `## A` 存在,但歧义(两行)→ 不动
        let v4a = format!("*** Update File: {name}\n@@ ## A\n x\n+y\n");
        let (out, reps) = align_at_headers(&v4a, Some(cwd));
        assert_eq!(out, v4a);
        assert!(reps.is_empty());
        // 子串 `A` 在 `## A` 两行里出现 → 歧义不动
        let v4a2 = format!("*** Update File: {name}\n@@ A\n x\n+y\n");
        let (out2, reps2) = align_at_headers(&v4a2, Some(cwd));
        assert_eq!(out2, v4a2);
        assert!(reps2.is_empty());
    }

    #[test]
    fn unprefixed_dup_of_plus_line_dropped() {
        // 真机 seq235:无前缀行 + 紧跟 `+<同内容>` → 删废行(内容在 + 行,不丢)。
        let (dir, name) = tmp_file("u.md", "other\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!("*** Update File: {name}\n*data source*\n+*data source*\n+more\n");
        let (out, reps) = fix_unprefixed_lines(&v4a, Some(cwd));
        assert!(!out.contains("\n*data source*\n"), "无前缀废行应删:\n{out}");
        assert!(out.contains("+*data source*"), "+ 行保留(内容不丢)");
        assert_eq!(reps[0].kind, "repaired");
    }

    #[test]
    fn unprefixed_existing_file_line_gets_context_space() {
        // 无前缀行在文件里有同行 → context 漏空格 → 补 ` `。
        let (dir, name) = tmp_file("u2.md", "alpha\nkeepme\nbeta\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!("*** Update File: {name}\nkeepme\n+added\n");
        let (out, reps) = fix_unprefixed_lines(&v4a, Some(cwd));
        assert!(out.contains("\n keepme\n"), "应补空格成 context:\n{out}");
        assert_eq!(reps[0].kind, "repaired");
    }

    #[test]
    fn unprefixed_unknown_passes_through() {
        // 不在文件、非重复 → 透过(不猜)。
        let (dir, name) = tmp_file("u3.md", "real\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!("*** Update File: {name}\nhallucinated garbage line\n+x\n");
        let (out, reps) = fix_unprefixed_lines(&v4a, Some(cwd));
        assert_eq!(out, v4a, "未知无前缀行原样透过");
        assert!(reps.is_empty());
    }

    #[test]
    fn cwd_recall_remembers_last_seen() {
        // 带 cwd → 更新并返回;不带 → 回退到最近缓存(全局态,只做非 flaky 断言)。
        let a = remember_or_recall_cwd(Some("/tmp/recall_unique_path_7af3"));
        assert_eq!(a.as_deref(), Some("/tmp/recall_unique_path_7af3"));
        // 不带 cwd → 必有缓存值(刚设过),不为 None。
        assert!(remember_or_recall_cwd(None).is_some());
        // remember_cwd_from_request 从 turn-start 请求抽 cwd 并写入缓存(供后续 apply_patch 回退)。
        let req = json!({"input":[{"type":"message","role":"user","content":"<environment_context>\n  <cwd>/tmp/ts_proj_b3f9</cwd>\n</environment_context>"}]});
        remember_cwd_from_request(Some(&req));
        assert!(
            remember_or_recall_cwd(None).is_some(),
            "remember 后 recall 应有值"
        );
    }

    #[test]
    fn blank_line_drift_block_realigned() {
        // EP-1 真机 seq111:模型 context 块漏了文件里的空行 → 整块失配。忽略空行唯一定位 → 重建
        // (补回文件空行 + 对齐字节),`+` 插入保位。
        let (dir, name) = tmp_file(
            "main.py",
            "from a import x\nfrom b import y\n\nfrom c import z\nfrom d import w\n",
        );
        let cwd = dir.path().to_str().unwrap();
        // patch 的 context 漏了 `from b` 与 `from c` 之间的空行,想在 `from d` 后插一行。
        let v4a = format!(
            "*** Begin Patch\n*** Update File: {name}\n from a import x\n from b import y\n from c import z\n from d import w\n+from e import v\n*** End Patch\n"
        );
        let (out, reps) = preflight_repair(&v4a, Some(cwd));
        assert!(out.contains("+from e import v"), "插入行保留:\n{out}");
        // 重建后 context 块应含被补回的空行(裸 ' ')。
        assert!(out.contains("\n \n"), "应补回文件空行作 context:\n{out}");
        assert_eq!(reps[0].kind, "repaired", "{:?}", reps);
    }

    #[test]
    fn blank_tolerant_skips_blank_line_deletion() {
        // 含「删除一个空行」的 `-` → blank-tolerant 重建无法忠实表达 → 放行不改(不静默转 context)。
        let (dir, name) = tmp_file("bd.txt", "x\ny\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!("*** Update File: {name}\n x\n-\n y\n+z\n");
        let (out, reps) = preflight_repair(&v4a, Some(cwd));
        assert_eq!(out, v4a, "含空白行删除应放行不改:\n{out}");
        assert!(reps[0].kind.starts_with("skipped"), "{:?}", reps);
    }

    #[test]
    fn blank_tolerant_ambiguous_passthrough() {
        // 精确失配(文件 p/q 间有空行,patch 没写)但忽略空行后**多处**匹配 → 歧义放行不猜。
        let (dir, name) = tmp_file("dup.txt", "p\n\nq\nX\np\n\nq\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!("*** Update File: {name}\n p\n q\n+r\n");
        let (out, reps) = preflight_repair(&v4a, Some(cwd));
        assert_eq!(out, v4a, "歧义(忽略空行后多处)不改:\n{out}");
        assert!(reps[0].kind.starts_with("skipped"), "{:?}", reps);
    }

    #[test]
    fn optimize_pipeline_fixes_multiple_issues() {
        // 一个 patch 同时:漏信封 + 双边 @@ + 尾随空格上下文 → 全恢复。
        let (dir, name) = tmp_file("multi.txt", "fn main() {\n    let x = 1;\n}\n");
        let cwd = dir.path().to_str().unwrap();
        let v4a = format!(
            "*** Update File: {name}\n@@ fn main() {{ @@\n    let x = 1;   \n+    let y = 2;\n"
        );
        let (out, reps) = optimize_patch(&v4a, Some(cwd), true);
        assert!(out.starts_with("*** Begin Patch\n"), "补信封:\n{out}");
        assert!(out.trim_end().ends_with("*** End Patch"), "补 End:\n{out}");
        assert!(out.contains("@@ fn main() {\n"), "双边 @@ 转单边:\n{out}");
        assert!(out.contains("    let x = 1;\n"), "尾随空格对齐:\n{out}");
        assert!(out.contains("+    let y = 2;"), "新增行保留");
        // 至少 3 类修复都记录
        let kinds: Vec<&str> = reps.iter().map(|r| r.kind.as_str()).collect();
        assert!(
            kinds.iter().filter(|k| **k == "repaired").count() >= 2,
            "{:?}",
            reps
        );
    }

    #[test]
    fn add_file_untouched_no_cwd_noop() {
        let v4a = "*** Begin Patch\n*** Add File: new.txt\n+hello\n*** End Patch\n";
        // 无 Update File → 短路原样返回(即便给 cwd)。
        let (out, reps) = preflight_repair(v4a, Some("/tmp"));
        assert_eq!(out, v4a);
        assert!(reps.is_empty());
        // 无 cwd → 原样
        let (out2, reps2) = preflight_repair(v4a, None);
        assert_eq!(out2, v4a);
        assert!(reps2.is_empty());
    }
}
