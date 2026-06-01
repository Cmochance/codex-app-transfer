//! `~/.codex/config.toml` 增量同步(根级别 key + section table field).
//!
//! **不用真 TOML parser**:Codex CLI 用户可能在文件里写注释 / 多 section,
//! 真 parser round-trip 会丢这些。我们做最小行级改动:
//!
//! - [`sync_root_value`]:root-level `<key> = ...`(写在第一个 section 之前)
//! - [`sync_table_field`]:`[section]` 内 `<key> = ...`(#212 起加,Codex
//!   docs / `codex exec` 输出的标准形式,**不可** 用 root-level dotted key
//!   形式跟 Codex 已有 `[section]` 段并存,会触发 TOML duplicate table error)
//!
//! 1:1 对齐 Python `_sync_codex_toml_value`(`backend/registry.py:872-903`)。

use std::path::Path;

use crate::CodexError;

/// 同步一个根级别 key:
/// - `Some(raw_value)` → 把 `<key> = <raw_value>` 写到第一个 section 之前(若存在),
///   否则追加到末尾。`raw_value` 必须是已经按 TOML 字面量格式化好的字符串
///   (字符串值要传 `"\"abc\""` 含引号;整数传 `"1000000"`;布尔传 `"true"`)。
/// - `None` → 删除该 key 的所有出现位置。
///
/// 文件不存在时按"空内容"处理。写入用 `write_atomic`(tmp + rename)。
pub fn sync_root_value(
    config_toml_path: &Path,
    key: &str,
    raw_value: Option<&str>,
) -> Result<(), CodexError> {
    let current = read_or_empty(config_toml_path)?;
    let new_content = sync_root_value_in_memory(&current, key, raw_value);
    if new_content == current {
        return Ok(());
    }
    write_atomic(config_toml_path, &new_content)?;
    Ok(())
}

/// 纯函数:对一段 TOML 文本做同步,返回新文本。**与 IO 解耦,便于单测**。
pub fn sync_root_value_in_memory(current: &str, key: &str, raw_value: Option<&str>) -> String {
    let mut new_lines: Vec<String> = Vec::new();
    let mut inserted = false;

    for line in current.lines() {
        // 删除已有的 root-level `<key> ...` 行
        let stripped = line.trim_start();
        if line_matches_root_key(stripped, key) {
            continue;
        }
        // 插入点:第一个 section header 之前
        if !inserted && raw_value.is_some() && stripped.starts_with('[') {
            if let Some(v) = raw_value {
                new_lines.push(format!("{key} = {v}"));
            }
            inserted = true;
        }
        new_lines.push(line.to_owned());
    }

    if !inserted {
        if let Some(v) = raw_value {
            new_lines.push(format!("{key} = {v}"));
        }
    }

    let mut result = new_lines.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// 把 Rust 字符串转 TOML 双引号字面量;用 serde_json 做转义(TOML basic
/// string 与 JSON string 的转义规则在常见字符上一致)。
pub fn toml_string_literal(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

/// 在 `[section]` table 里 insert/replace `key = raw_value`,用 TOML
/// **section table 形式**(Codex docs 推荐+exec 输出的标准形式)。
///
/// **为什么不用 root-level dotted key**(2026-05-19 #212 code-reviewer
/// blocker 修):TOML 1.0 spec 禁止 dotted root key 跟同名 `[section]`
/// 并存(`sandbox_workspace_write.foo = …` 隐式创建 `sandbox_workspace_write`
/// table,后面再写 `[sandbox_workspace_write]` = duplicate table parse
/// error)。Codex `[sandbox_workspace_write]` 在文档/`codex exec` 输出里
/// 都是 section 形式,本 helper 跟它对齐,避免 mix-form 让 Codex CLI
/// 加载 config.toml 失败。
///
/// 行为:
/// - section + key 都存在 → 替换 `key = value` 那一行
/// - section 在但 key 不在 → 在 section body 末尾(下一个 `[header]` 或
///   EOF 前)插入 `key = value`
/// - section 不在 → append `[section]\nkey = value` 到文件末尾
/// - `raw_value = None` → 删 `key` 那一行,**保留** section(用户其它
///   key 可能也在同 section)
pub fn sync_table_field(
    config_toml_path: &Path,
    section: &str,
    key: &str,
    raw_value: Option<&str>,
) -> Result<(), CodexError> {
    let current = read_or_empty(config_toml_path)?;
    let new_content = sync_table_field_in_memory(&current, section, key, raw_value);
    if new_content == current {
        return Ok(());
    }
    write_atomic(config_toml_path, &new_content)?;
    Ok(())
}

/// 纯函数版本,便于单测。详见 [`sync_table_field`]。
pub fn sync_table_field_in_memory(
    current: &str,
    section: &str,
    key: &str,
    raw_value: Option<&str>,
) -> String {
    // **优先级 1**:用户已用 root-level **dotted key** 形式(等价合法 TOML,
    // `sandbox_workspace_write.network_access = false`)。chatgpt-codex P2#2
    // 反馈:若我们不识别此形式直接 append `[section]` 会跟 dotted key 隐式
    // 定义的同名 table 撞 duplicate → 用户 config 失效。改成走 root-level
    // 替换路径,保留用户原形式不破坏。
    let dotted_key = format!("{section}.{key}");
    if has_root_key_line(current, &dotted_key) {
        return sync_root_value_in_memory(current, &dotted_key, raw_value);
    }

    let section_header = format!("[{section}]");
    let mut lines: Vec<String> = current.lines().map(String::from).collect();

    // **section header 匹配**:除精确 trim 匹配,**额外兼容尾部注释**
    // (chatgpt-codex P2#1):`[sandbox_workspace_write] # local comment`
    // TOML spec 合法,精确匹配 miss 后 append 会造成 duplicate。
    let section_start = lines
        .iter()
        .position(|l| matches_section_header(l, &section_header));

    if let Some(start_idx) = section_start {
        // section body 结束位置:下一个 `[` 开头的 section header 或 EOF
        let mut end_idx = lines.len();
        for (offset, line) in lines.iter().enumerate().skip(start_idx + 1) {
            if line.trim_start().starts_with('[') {
                end_idx = offset;
                break;
            }
        }
        // 在 section body 内找 key
        let key_offset = lines[start_idx + 1..end_idx]
            .iter()
            .position(|l| line_matches_root_key(l.trim_start(), key));

        match (key_offset, raw_value) {
            (Some(off), Some(v)) => {
                lines[start_idx + 1 + off] = format!("{key} = {v}");
            }
            (Some(off), None) => {
                lines.remove(start_idx + 1 + off);
            }
            (None, Some(v)) => {
                lines.insert(end_idx, format!("{key} = {v}"));
            }
            (None, None) => {}
        }
    } else if let Some(v) = raw_value {
        // section 不存在 → append `[section]\nkey = value` 到文件末尾,
        // 前面留一行空行(若已有内容)以增强可读性。
        if !lines.is_empty() && !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
            lines.push(String::new());
        }
        lines.push(section_header);
        lines.push(format!("{key} = {v}"));
    }

    let mut result = lines.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// 检查 `current` 任一行是否是 root-level `<key> = ...` 形式。复用
/// [`line_matches_root_key`] 的严格匹配规则(防 `foo_bar` 误匹 `foo`)。
fn has_root_key_line(current: &str, key: &str) -> bool {
    current
        .lines()
        .any(|line| line_matches_root_key(line.trim_start(), key))
}

/// section header 匹配:精确 `[section]` 或带尾部 `#` 注释。
/// TOML spec 允许 `[section] # comment`,exact-string 比较会漏。
fn matches_section_header(line: &str, header: &str) -> bool {
    let trimmed = line.trim();
    if trimmed == header {
        return true;
    }
    if let Some(rest) = trimmed.strip_prefix(header) {
        // header 后必须是空白 + 可选 `#` 注释才算同 section,避免误匹
        // `[sandbox_workspace_write_alt]` 这种前缀同名 section。
        let rest = rest.trim_start();
        return rest.is_empty() || rest.starts_with('#');
    }
    false
}

fn line_matches_root_key(stripped_left: &str, key: &str) -> bool {
    // Python 用 `stripped.startswith(key) and "=" in stripped`,我们对齐。
    // 同时谨慎一点:`key` 之后必须是空白 / `=`,避免 `model_provider` 把
    // `model_provider_id` 也误删。
    if !stripped_left.starts_with(key) {
        return false;
    }
    if !stripped_left.contains('=') {
        return false;
    }
    let after_key = &stripped_left[key.len()..];
    matches!(after_key.chars().next(), Some(c) if c == '=' || c.is_ascii_whitespace())
}

fn read_or_empty(path: &Path) -> Result<String, CodexError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn write_atomic(path: &Path, content: &str) -> Result<(), CodexError> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.to_path_buf();
    let mut name = tmp.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    tmp.set_file_name(name);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_into_empty_appends_with_trailing_newline() {
        let out = sync_root_value_in_memory("", "openai_base_url", Some("\"http://x\""));
        assert_eq!(out, "openai_base_url = \"http://x\"\n");
    }

    #[test]
    fn replace_existing_key() {
        let input = "openai_base_url = \"old\"\n";
        let out = sync_root_value_in_memory(input, "openai_base_url", Some("\"new\""));
        assert_eq!(out, "openai_base_url = \"new\"\n");
    }

    #[test]
    fn insert_before_first_section_header() {
        let input = "# my comment\n[profiles]\napi_key = \"x\"\n";
        let out = sync_root_value_in_memory(input, "openai_base_url", Some("\"http://up\""));
        assert!(out.contains("openai_base_url = \"http://up\""));
        assert!(out.starts_with("# my comment\n"));
        assert!(out.contains("[profiles]"));
        // 确保插在 [profiles] 之前
        let idx_key = out.find("openai_base_url").unwrap();
        let idx_section = out.find("[profiles]").unwrap();
        assert!(idx_key < idx_section, "应插入在 [profiles] 之前");
    }

    #[test]
    fn delete_removes_all_instances() {
        let input = "openai_base_url = \"a\"\nfoo = 1\nopenai_base_url = \"b\"\n[s]\nbar = 2\n";
        let out = sync_root_value_in_memory(input, "openai_base_url", None);
        assert!(!out.contains("openai_base_url"));
        assert!(out.contains("foo = 1"));
        assert!(out.contains("bar = 2"));
        assert!(out.contains("[s]"));
    }

    #[test]
    fn does_not_touch_keys_with_same_prefix() {
        let input = "openai_base_url = \"a\"\nopenai_base_url_alt = \"b\"\n";
        let out = sync_root_value_in_memory(input, "openai_base_url", Some("\"new\""));
        assert!(out.contains("openai_base_url = \"new\""));
        assert!(
            out.contains("openai_base_url_alt = \"b\""),
            "前缀同名的 key 不应被改动"
        );
    }

    #[test]
    fn preserves_user_comments_and_other_keys() {
        let input = "\
# user wrote this
api_key = \"k\"
openai_base_url = \"old\"
# trailing
";
        let out = sync_root_value_in_memory(input, "openai_base_url", Some("\"new\""));
        assert!(out.contains("# user wrote this"));
        assert!(out.contains("api_key = \"k\""));
        assert!(out.contains("openai_base_url = \"new\""));
        assert!(out.contains("# trailing"));
    }

    #[test]
    fn integer_value_no_quotes() {
        let out = sync_root_value_in_memory("", "model_context_window", Some("1000000"));
        assert_eq!(out, "model_context_window = 1000000\n");
    }

    // ── sync_table_field tests(#212)──────────────────────────────

    #[test]
    fn table_field_inserts_section_when_missing() {
        let out = sync_table_field_in_memory(
            "",
            "sandbox_workspace_write",
            "network_access",
            Some("true"),
        );
        assert!(out.contains("[sandbox_workspace_write]"));
        assert!(out.contains("network_access = true"));
    }

    #[test]
    fn table_field_replaces_existing_key_in_section() {
        let input = "[sandbox_workspace_write]\nnetwork_access = false\n";
        let out = sync_table_field_in_memory(
            input,
            "sandbox_workspace_write",
            "network_access",
            Some("true"),
        );
        assert!(out.contains("network_access = true"));
        assert!(
            !out.contains("network_access = false"),
            "旧值必须被替换:{out}"
        );
        // 不重复 section header
        assert_eq!(out.matches("[sandbox_workspace_write]").count(), 1);
    }

    #[test]
    fn table_field_appends_key_to_existing_section() {
        let input = "[sandbox_workspace_write]\nexclude_tmpdir_env_var = false\n";
        let out = sync_table_field_in_memory(
            input,
            "sandbox_workspace_write",
            "network_access",
            Some("true"),
        );
        // 保留用户原 key + 加新 key
        assert!(out.contains("exclude_tmpdir_env_var = false"));
        assert!(out.contains("network_access = true"));
        assert_eq!(out.matches("[sandbox_workspace_write]").count(), 1);
    }

    #[test]
    fn table_field_delete_removes_key_keeps_section() {
        // section 内还有用户的其它 key,删 network_access 时保留 section + 其它 key
        let input =
            "[sandbox_workspace_write]\nnetwork_access = true\nexclude_tmpdir_env_var = false\n";
        let out =
            sync_table_field_in_memory(input, "sandbox_workspace_write", "network_access", None);
        assert!(!out.contains("network_access"));
        assert!(out.contains("[sandbox_workspace_write]"));
        assert!(out.contains("exclude_tmpdir_env_var = false"));
    }

    #[test]
    fn table_field_delete_when_key_absent_is_noop() {
        let input = "[sandbox_workspace_write]\nexclude_tmpdir_env_var = false\n";
        let out =
            sync_table_field_in_memory(input, "sandbox_workspace_write", "network_access", None);
        assert_eq!(out, input);
    }

    #[test]
    fn table_field_does_not_touch_other_sections() {
        let input = "[profiles]\napi_key = \"x\"\n\n[other]\nfoo = 1\n";
        let out = sync_table_field_in_memory(
            input,
            "sandbox_workspace_write",
            "network_access",
            Some("true"),
        );
        // 用户 section 保留
        assert!(out.contains("[profiles]"));
        assert!(out.contains("api_key = \"x\""));
        assert!(out.contains("[other]"));
        assert!(out.contains("foo = 1"));
        // 新 section append 到末尾
        assert!(out.contains("[sandbox_workspace_write]"));
        assert!(out.contains("network_access = true"));
    }

    // **chatgpt-codex P2#1 防回归**:section header 同行带 `#` 注释
    // (TOML spec 合法形式)我们必须识别,否则 append 新 section 撞 duplicate。
    #[test]
    fn table_field_recognizes_section_header_with_trailing_comment() {
        let input =
            "[sandbox_workspace_write] # local sandbox settings\nexclude_tmpdir_env_var = false\n";
        let out = sync_table_field_in_memory(
            input,
            "sandbox_workspace_write",
            "network_access",
            Some("true"),
        );
        // 不重复 section header(若 miss 识别会 append 第二份)
        assert_eq!(
            out.matches("[sandbox_workspace_write]").count(),
            1,
            "must reuse existing section, not duplicate: {out}"
        );
        assert!(out.contains("network_access = true"));
        // 验 toml crate 能正常 parse
        let _: toml::Value = toml::from_str(&out).expect("output must parse as valid TOML");
    }

    // **chatgpt-codex P2#2 防回归**:用户已用 root-level dotted key 形式
    // (等价合法 TOML)→ 我们必须 in-place replace,不能 append 新
    // `[section]` 否则跟 dotted key 隐式定义的同名 table 撞 duplicate。
    #[test]
    fn table_field_replaces_existing_dotted_root_key_form() {
        let input = "sandbox_workspace_write.network_access = false\nother = 1\n";
        let out = sync_table_field_in_memory(
            input,
            "sandbox_workspace_write",
            "network_access",
            Some("true"),
        );
        // 保持 dotted 形式(尊重用户原格式), value 改成 true
        assert!(
            out.contains("sandbox_workspace_write.network_access = true"),
            "must replace dotted-key value, not append [section]: {out}"
        );
        // 绝不能同时出现 dotted key 跟 [section] 两种形式
        assert!(
            !out.contains("[sandbox_workspace_write]"),
            "must not append section table when dotted form exists (duplicate-table TOML): {out}"
        );
        assert!(out.contains("other = 1"), "other root keys preserved");
        // 验 toml crate 能正常 parse
        let _: toml::Value = toml::from_str(&out).expect("output must parse as valid TOML");
    }

    /// **#212 BLOCKER 防回归**:确保 sync_table_field 写出的 TOML 可被
    /// `toml` crate 正常 parse(不触发 duplicate table error)。这是
    /// reviewer 提的 round-trip 验证 —— 之前的 dotted-root-key 方案就在
    /// 这里炸,本测试守住 section-form 实现不再回归。
    #[test]
    fn table_field_output_parses_as_valid_toml() {
        // 模拟 Codex 默认 sandbox_workspace_write section 已存在的真实 case
        let input = "model_provider = \"openai\"\n\n[sandbox_workspace_write]\nexclude_tmpdir_env_var = false\nexclude_slash_tmp = false\n";
        let out = sync_table_field_in_memory(
            input,
            "sandbox_workspace_write",
            "network_access",
            Some("true"),
        );
        let parsed: toml::Value = toml::from_str(&out).expect("output must parse as valid TOML");
        let section = parsed
            .get("sandbox_workspace_write")
            .and_then(|v| v.as_table())
            .expect("section 必存在");
        assert_eq!(
            section.get("network_access").and_then(|v| v.as_bool()),
            Some(true)
        );
        // 用户原 key 保留
        assert_eq!(
            section
                .get("exclude_tmpdir_env_var")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn toml_string_literal_escapes() {
        assert_eq!(toml_string_literal("hello"), "\"hello\"");
        assert_eq!(toml_string_literal("a\"b"), "\"a\\\"b\"");
        assert_eq!(toml_string_literal("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn delete_when_key_absent_is_noop_but_keeps_trailing_newline() {
        let input = "foo = 1\n";
        let out = sync_root_value_in_memory(input, "missing_key", None);
        assert_eq!(out, "foo = 1\n");
    }

    #[test]
    fn file_write_is_skipped_when_root_value_is_already_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "approval_policy = \"never\"\n").unwrap();

        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        sync_root_value(&path, "approval_policy", Some("\"never\"")).unwrap();

        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "same content should not rewrite config.toml");
    }

    #[test]
    fn file_write_is_skipped_when_table_field_is_already_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();

        // 第一次写入建立规范格式(空文件 → 建 section)
        sync_table_field(
            &path,
            "sandbox_workspace_write",
            "network_access",
            Some("true"),
        )
        .unwrap();

        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        // 再 sync 同一目标值 → 新旧内容一致 → 不应写盘(mtime 不变)
        sync_table_field(
            &path,
            "sandbox_workspace_write",
            "network_access",
            Some("true"),
        )
        .unwrap();

        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "same table field content should not rewrite config.toml"
        );
    }

    #[test]
    fn file_is_rewritten_when_root_value_changes() {
        // 反向守门(验收:真有变化仍正常写盘):防 skip 条件写反 / 误加无条件 return
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "approval_policy = \"never\"\n").unwrap();

        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        // 值真的变了 → 必须写盘
        sync_root_value(&path, "approval_policy", Some("\"on-request\"")).unwrap();

        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_ne!(before, after, "changed content must rewrite config.toml");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("on-request"),
            "new value must be persisted to disk"
        );
    }
}
