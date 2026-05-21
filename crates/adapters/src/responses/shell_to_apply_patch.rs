//! Adapter server-side normalize:把模型 emit 的 `exec_command` shell file-write
//! 模式重写为 `apply_patch` custom_tool_call,让 Codex Desktop 自家 V4A applier
//! 渲染 4+2 diff UI(代码块行号 / +N -M / 文件清单 / 颜色高亮 / 点击跳转 /
//! 工具调用抬头),无需依赖模型主动选 apply_patch 工具。
//!
//! 设计灵感来自 Antigravity Go agent(`~/.local/bin/agy`,闭源 Google
//! internal Go 1.27 RC build,无公开 source)— `strings` dump 实证 binary
//! 含 `FILE_CHANGE_TYPE_EDIT` / `file_diff` / `file_diffs` /
//! `trajectory_file_diffs` 等 token,印证它在 server 层产出统一 diff 事件
//! 而非依赖模型工具选择;**具体的 shell → V4A patch normalize 规则**是
//! codex-app-transfer 自创,不存在可对照的上游 source。
//!
//! MVP scope:**只支持 Add File / 完整 overwrite 三种 shell 形态**:
//!   1. here-doc:`cat <<'EOF' > /path/file\n<body>\nEOF`
//!   2. printf:`printf '<content>' > /path/file`(支持 `\n` `\t` `\\` 转义)
//!   3. echo:`echo '<content>' > /path/file`(字面 literal,不展开转义)
//!
//! 严格 reject(透传原 exec_command 不动):
//!   - `>>` append / `sed -i` in-place / `tee` / `tee -a`
//!   - 多命令组合(`;` `&&` `||` 在 cmd 前段)
//!   - pipe `|` / 子 shell `$(...)` 或 `` `...` `` 变量替换
//!   - 多 `>` redirect / `&>` / `2>`
//!   - here-doc 终止符不匹配 / `echo` / `printf` flags(`-e` `-n` 等)
//!
//! Edit / append (`>>` / `tee -a` / `cat <<EOF >>`) / `sed -i` in-place /
//! output normalize 等更复杂模式落 `docs/followup-tracker.md` #39 跟踪。
//!
//! **Corner cases**(都有 unit test 覆盖,但在 MVP scope 外的输入也明示):
//! - 空 body(`echo '' > file`)→ V4A 单 `+` 行(模拟 echo 写一个 newline 行为)
//! - body 无 trailing newline:`echo` 路径自动补 `\n`(bash `echo` 默认行为),
//!   `printf` / here-doc 保持原 body 不补
//! - double-quote literal 仅展开 `\\` / `\"` 转义(其他转义 reject);
//!   `printf` literal 展开 `\n` / `\t` / `\\` / `\'` / `\"` / `\r`
//! - 相对路径(`./local.txt`)接受,绝对路径(`/tmp/x.txt`)接受;
//!   含 shell metachar / 空格 / 引号的 path reject

/// 检测到的 file-write 操作 → V4A patch payload。
///
/// `patch_input` 可直接作为 Codex Desktop apply_patch 函数调用的 `input`
/// 字符串(自带 `*** Begin Patch` / `*** End Patch` 包裹 + Add File 头 + `+`
/// 前缀每一行)。
#[derive(Debug, Clone)]
pub struct V4APatch {
    /// 目标文件路径(用于 telemetry / 日志);从原始 shell `>` redirect 提取。
    pub target_path: String,
    /// 完整 V4A patch envelope。
    pub patch_input: String,
}

/// 顶层入口:接受 `exec_command` 工具调用的 `cmd` 字符串,返回 `Some(V4APatch)`
/// 当且仅当 cmd 是 MVP scope 内的纯 file-write,否则 `None`(透传原 exec_command)。
pub fn detect_shell_file_write(raw_cmd: &str) -> Option<V4APatch> {
    let cmd = raw_cmd.trim();
    if cmd.is_empty() {
        return None;
    }

    // Stage 1: 全局禁用模式(无论命令是哪类,都 reject)
    if has_disallowed_constructs(cmd) {
        return None;
    }

    // Stage 2: 按 pattern 顺序尝试匹配
    if let Some(patch) = try_here_doc(cmd) {
        return Some(patch);
    }
    if let Some(patch) = try_printf_redirect(cmd) {
        return Some(patch);
    }
    if let Some(patch) = try_echo_redirect(cmd) {
        return Some(patch);
    }

    None
}

/// 全局禁用 token 检查 — 任意一个出现都拒绝(透传 exec_command)。
fn has_disallowed_constructs(cmd: &str) -> bool {
    // append redirect:`>>` 表示 append,无法纯 Add File 覆盖
    if cmd.contains(">>") {
        return true;
    }
    // sed -i / sed -i '':in-place edit,需要 old content
    if cmd.contains("sed -i") {
        return true;
    }
    // tee:tee 默认 truncate,但模型用 tee 通常配 pipe(`echo x | tee file`)
    // 或 `tee -a` append,边界复杂,MVP 全 reject
    if regex_word_match(cmd, "tee") {
        return true;
    }
    // 子 shell 变量替换:`$(...)` / `` `...` ``,内容动态生成无法预知
    if cmd.contains("$(") {
        return true;
    }
    if cmd.contains('`') {
        return true;
    }
    // bare 变量引用:`$VAR` / `${VAR}`(IMPORTANT-2 修复 / code-reviewer
    // pre-push)。模型用 `echo "hello $USER" > file` 通常期待 shell 展开成
    // `hello alyse`,但我们 normalize 后直接写入字面 `$USER`,跟模型意图静默
    // divergence。配置文件 / 文档场景特别危险(`SECRET=$API_KEY` 字面落盘
    // 是真 bug)。`$(...)` 已在上面 reject,这里只补 bare var reject:任何
    // `$` 出现都 reject,简单且保险(单引号 literal 内 `$` 不展开但语义上
    // 模型既可能想字面也可能想展开,reject 强迫模型用 apply_patch 显式表达)。
    if cmd.contains('$') {
        return true;
    }
    // multi-command:在 here-doc body 中可以有 `;` 等,但 here-doc 解析逻辑
    // 自己处理。在 cmd 前段(`> file` 之前)如果有 `;` `&&` `||` `|` `&>` `2>`
    // 等,reject。这里采用更严的策略:cmd 中任何位置含 `&&` `||` `&>`
    // 都拒绝;`;` `|` 在 here-doc body 内可接受,需要专门 pattern 处理时再放
    // 行。MVP 严格策略:整 cmd 不能含这些 token。
    if cmd.contains("&&") || cmd.contains("||") || cmd.contains("&>") {
        return true;
    }
    // `2>` (stderr redirect):reject 任意 stderr 重定向
    if cmd.contains("2>") {
        return true;
    }
    false
}

/// 简单"独立 word"匹配(避免 `committee` 把 `tee` 当 token 命中):
/// 检查 needle 是否作为完整 word 出现(前后是 cmd 边界 / 空格 / `;` 等)。
fn regex_word_match(cmd: &str, needle: &str) -> bool {
    let bytes = cmd.as_bytes();
    let nlen = needle.len();
    let mut i = 0;
    while i + nlen <= bytes.len() {
        if &bytes[i..i + nlen] == needle.as_bytes() {
            let prev_ok = i == 0
                || matches!(bytes[i - 1], b' ' | b'\t' | b';' | b'\n' | b'(' | b'|');
            let next_ok = i + nlen == bytes.len()
                || matches!(
                    bytes[i + nlen],
                    b' ' | b'\t' | b';' | b'\n' | b')' | b'|' | b'<' | b'>'
                );
            if prev_ok && next_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// 尝试 `cat <<['"]?EOF['"]? > /path\n<body>\nEOF` 模式。
/// 接受 unquoted / single-quote / double-quote 终止符(三种都常见)。
fn try_here_doc(cmd: &str) -> Option<V4APatch> {
    // 必须以 `cat <<` 开头(允许前面 leading whitespace 但 trim 过了)
    let rest = cmd.strip_prefix("cat ")?.trim_start();
    let rest = rest.strip_prefix("<<")?.trim_start();

    // 解析终止符 token:可能是 'EOF' / "EOF" / EOF / -EOF(`<<-` 已被
    // strip_prefix 排除,这里不支持 `<<-`,strict)
    let (terminator, after_term) = parse_heredoc_terminator(rest)?;
    let after_term = after_term.trim_start();

    // 必须是 `> /path`(单个 redirect)
    let redirect_rest = after_term.strip_prefix('>')?.trim_start();
    // path 到 newline 结束
    let nl_idx = redirect_rest.find('\n')?;
    let path = redirect_rest[..nl_idx].trim();
    if path.is_empty() {
        return None;
    }
    if !is_simple_path(path) {
        return None;
    }
    // body 从 newline 之后开始
    let body_start = &redirect_rest[nl_idx + 1..];

    // 找 body 末尾的 terminator 行(`<terminator>` 单独占一行,trim 后等于
    // terminator)
    let body = strip_heredoc_terminator(body_start, &terminator)?;

    let patch_input = build_add_file_patch(path, body);
    Some(V4APatch {
        target_path: path.to_string(),
        patch_input,
    })
}

/// 解析 here-doc 终止符 token,返回 (terminator_no_quotes, remaining).
fn parse_heredoc_terminator(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('\'') {
        let end = rest.find('\'')?;
        Some((rest[..end].to_string(), &rest[end + 1..]))
    } else if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        Some((rest[..end].to_string(), &rest[end + 1..]))
    } else {
        // unquoted:取直到 whitespace / `>`
        let end = s
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(s.len());
        if end == 0 {
            return None;
        }
        let token = &s[..end];
        // terminator 必须是合法 shell identifier 风格(纯 alnum + underscore)
        if !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return None;
        }
        Some((token.to_string(), &s[end..]))
    }
}

/// 在 `body_with_terminator` 中找单独占一行的 terminator,返回 terminator
/// 之前的内容(不含 terminator 行)。
fn strip_heredoc_terminator<'a>(
    body_with_terminator: &'a str,
    terminator: &str,
) -> Option<&'a str> {
    let mut search_from = 0usize;
    while search_from <= body_with_terminator.len() {
        let slice = &body_with_terminator[search_from..];
        let idx = slice.find(terminator)?;
        let absolute_idx = search_from + idx;
        // 检查 terminator 前一字符是不是 newline(或 body 开头)
        let prev_is_nl = absolute_idx == 0
            || body_with_terminator.as_bytes()[absolute_idx - 1] == b'\n';
        // 检查 terminator 后跟的是 newline / EOF(允许 trailing whitespace
        // 直到 newline / 字符串末尾)
        let after = &body_with_terminator[absolute_idx + terminator.len()..];
        let trailing_only_ws_then_nl_or_eof = after
            .split_once('\n')
            .map(|(before_nl, _)| before_nl.chars().all(char::is_whitespace))
            .unwrap_or_else(|| after.chars().all(char::is_whitespace));
        if prev_is_nl && trailing_only_ws_then_nl_or_eof {
            // body 不含末尾的 newline + terminator 行
            let body_end = if absolute_idx > 0 { absolute_idx - 1 } else { 0 };
            return Some(&body_with_terminator[..body_end]);
        }
        search_from = absolute_idx + terminator.len();
    }
    None
}

/// 尝试 `printf '<content>' > /path` 或 `printf "<content>" > /path` 模式。
/// **支持** \n / \t / \\ / \' / \" 转义展开(shell printf 语义)。
/// **拒绝** 含 `%` 格式串(`%s` `%d` 等,因为格式串需要额外 args)。
fn try_printf_redirect(cmd: &str) -> Option<V4APatch> {
    let rest = cmd.strip_prefix("printf ")?.trim_start();
    let (literal, after) = parse_quoted_literal(rest)?;
    // printf 格式串如果含 `%`(非 `%%` 转义)且后面没跟参数 → 模型可能误用,
    // MVP 直接 reject(避免错误展开)
    if has_unescaped_percent(&literal) {
        return None;
    }
    let after = after.trim();
    // printf 后必须只剩 `> /path`
    let path = after.strip_prefix('>')?.trim();
    if path.is_empty() || !is_simple_path(path) {
        return None;
    }
    let body = expand_printf_escapes(&literal);
    let patch_input = build_add_file_patch(path, &body);
    Some(V4APatch {
        target_path: path.to_string(),
        patch_input,
    })
}

/// 尝试 `echo '<content>' > /path` 或 `echo "<content>" > /path` 模式。
/// echo **不**展开转义(MVP 拒绝 `echo -e`),body 是字面 literal。
fn try_echo_redirect(cmd: &str) -> Option<V4APatch> {
    let rest = cmd.strip_prefix("echo ")?.trim_start();
    // 拒绝 echo flags(`-e` / `-n` / `-E`)
    if rest.starts_with('-') {
        return None;
    }
    let (literal, after) = parse_quoted_literal(rest)?;
    let after = after.trim();
    let path = after.strip_prefix('>')?.trim();
    if path.is_empty() || !is_simple_path(path) {
        return None;
    }
    // echo 默认在 body 末尾追加一个 newline(bash 行为),保持忠实模拟
    let mut body = literal;
    if !body.ends_with('\n') {
        body.push('\n');
    }
    let patch_input = build_add_file_patch(path, &body);
    Some(V4APatch {
        target_path: path.to_string(),
        patch_input,
    })
}

/// 解析一段以 `'` 或 `"` 包裹的 literal,返回 (literal_no_quotes, remaining).
/// single-quote 内不支持任何转义(shell 标准:single-quote literal 不解释);
/// double-quote 内支持基础 `\"` `\\`,但拒绝变量(`$var` / `${...}` 已在
/// `has_disallowed_constructs` 提前拒绝)。
fn parse_quoted_literal(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(after_open) = s.strip_prefix('\'') {
        // single-quote literal:**绝不**支持转义,直到下一个 `'` 结束
        let end = after_open.find('\'')?;
        Some((after_open[..end].to_string(), &after_open[end + 1..]))
    } else if let Some(after_open) = s.strip_prefix('"') {
        // double-quote literal:支持 `\\` `\"` 转义,其他字面
        let mut out = String::new();
        let mut iter = after_open.char_indices();
        while let Some((idx, ch)) = iter.next() {
            match ch {
                '"' => return Some((out, &after_open[idx + 1..])),
                '\\' => {
                    let (_i2, ch2) = iter.next()?;
                    match ch2 {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        // 拒绝其他转义(模型可能想要 \n,但 double-quote
                        // shell 不展开 \n,字面保留 → 跟 printf 行为差异
                        // 大,MVP 简单全 reject 其他转义)
                        _ => return None,
                    }
                }
                _ => out.push(ch),
            }
        }
        None
    } else {
        None
    }
}

/// 检查 literal 是否含 unescaped `%`(printf 格式串需要 args)。
fn has_unescaped_percent(literal: &str) -> bool {
    let bytes = literal.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // `%%` 是 literal `%` 转义,继续
            if i + 1 < bytes.len() && bytes[i + 1] == b'%' {
                i += 2;
                continue;
            }
            return true;
        }
        i += 1;
    }
    false
}

/// 展开 printf 风格转义:`\n` `\t` `\\` `\'` `\"`。其他转义保持字面。
fn expand_printf_escapes(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    let mut iter = literal.chars();
    while let Some(ch) = iter.next() {
        if ch == '\\' {
            match iter.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('\'') => out.push('\''),
                Some('"') => out.push('"'),
                Some('r') => out.push('\r'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// "simple path" 检查:必须是绝对路径(`/` 开头)或相对路径(`./` `../`
/// 或不含特殊 shell 字符的 token)。拒绝含空格 / 引号 / shell metachar 的
/// path,避免歧义。
fn is_simple_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    !path.chars().any(|c| {
        c.is_whitespace()
            || c == '"'
            || c == '\''
            || c == '$'
            || c == '`'
            || c == '*'
            || c == '?'
            || c == '['
            || c == ']'
            || c == '{'
            || c == '}'
            || c == '|'
            || c == '&'
            || c == ';'
            || c == '<'
            || c == '>'
            || c == '('
            || c == ')'
    })
}

/// 把 (path, body) 组装成 V4A `*** Add File: <path>` patch envelope。
/// body 按 `\n` 拆行,每行加 `+` 前缀。空 body 写入 1 行 `+`(空文件)。
fn build_add_file_patch(path: &str, body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 128);
    out.push_str("*** Begin Patch\n");
    out.push_str("*** Add File: ");
    out.push_str(path);
    out.push('\n');
    if body.is_empty() {
        // V4A Add File 至少一行 — 空文件用单独 `+` 行
        out.push_str("+\n");
    } else {
        // 注意:body 可能以 `\n` 结尾(printf '\n' / echo append 后),拆分时
        // 不要产生最后一个空 element。用 split_inclusive + manual handle 或
        // 用 lines() 但 lines() 会丢末尾 \n。这里用 split('\n'),如果末尾
        // 是 '\n',split 会产生最后一个空 element,跳过它。
        let parts: Vec<&str> = body.split('\n').collect();
        let last_idx = parts.len() - 1;
        for (idx, line) in parts.iter().enumerate() {
            if idx == last_idx && line.is_empty() {
                // 末尾 newline 产生的空 element:不输出 `+` 行(避免多余空行)
                break;
            }
            out.push('+');
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("*** End Patch\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Happy path =====

    #[test]
    fn here_doc_unquoted_terminator() {
        let cmd = "cat <<EOF > /tmp/a.py\nprint(\"hi\")\nEOF";
        let p = detect_shell_file_write(cmd).expect("should detect");
        assert_eq!(p.target_path, "/tmp/a.py");
        assert!(p.patch_input.contains("*** Add File: /tmp/a.py"));
        assert!(p.patch_input.contains("+print(\"hi\")"));
        assert!(p.patch_input.ends_with("*** End Patch\n"));
    }

    #[test]
    fn here_doc_single_quoted_terminator() {
        let cmd = "cat <<'EOF' > /tmp/b.txt\nline1\nline2\nEOF";
        let p = detect_shell_file_write(cmd).expect("should detect");
        assert_eq!(p.target_path, "/tmp/b.txt");
        assert!(p.patch_input.contains("+line1\n+line2\n"));
    }

    #[test]
    fn here_doc_multiline_body() {
        let cmd =
            "cat <<PYEOF > /tmp/app.py\ndef main():\n    pass\n\nif __name__ == \"__main__\":\n    main()\nPYEOF";
        let p = detect_shell_file_write(cmd).expect("should detect");
        assert!(p.patch_input.contains("+def main():"));
        assert!(p.patch_input.contains("+    pass"));
        // 空行作为单独 `+` 行
        assert!(p.patch_input.contains("+\n+if __name__"));
    }

    #[test]
    fn printf_single_quote_with_newlines() {
        let cmd = "printf 'a\\nb\\nc\\n' > /tmp/c.txt";
        let p = detect_shell_file_write(cmd).expect("should detect");
        // \n 展开后 body = "a\nb\nc\n"
        assert!(p.patch_input.contains("+a\n+b\n+c\n"));
    }

    #[test]
    fn echo_single_quote_simple() {
        let cmd = "echo 'hello' > /tmp/d.txt";
        let p = detect_shell_file_write(cmd).expect("should detect");
        // echo 在末尾自动加 newline
        assert!(p.patch_input.contains("+hello\n"));
    }

    #[test]
    fn echo_chinese_content() {
        let cmd = "echo '# 测试笔记' > /tmp/note.md";
        let p = detect_shell_file_write(cmd).expect("should detect");
        assert!(p.patch_input.contains("+# 测试笔记"));
    }

    // ===== Reject path =====

    #[test]
    fn reject_append_redirect() {
        assert!(detect_shell_file_write("echo 'x' >> /tmp/a.txt").is_none());
        assert!(detect_shell_file_write("cat <<EOF >> /tmp/a.txt\nx\nEOF").is_none());
    }

    #[test]
    fn reject_sed_inplace() {
        assert!(detect_shell_file_write("sed -i 's/x/y/' /tmp/a.txt").is_none());
        assert!(detect_shell_file_write("sed -i '' 's/x/y/' /tmp/a.txt").is_none());
    }

    #[test]
    fn reject_multi_command() {
        assert!(detect_shell_file_write("mkdir -p /tmp/d && echo 'x' > /tmp/d/a.txt").is_none());
        assert!(detect_shell_file_write("echo 'x' > /tmp/a.txt || true").is_none());
    }

    #[test]
    fn reject_var_substitution() {
        assert!(detect_shell_file_write("echo \"$(date)\" > /tmp/a.txt").is_none());
        assert!(detect_shell_file_write("echo \"`date`\" > /tmp/a.txt").is_none());
    }

    #[test]
    fn reject_multi_redirect() {
        assert!(detect_shell_file_write("echo 'x' > /tmp/a.txt 2> /tmp/err.txt").is_none());
        assert!(detect_shell_file_write("echo 'x' &> /tmp/a.txt").is_none());
    }

    #[test]
    fn reject_tee() {
        assert!(detect_shell_file_write("echo 'x' | tee /tmp/a.txt").is_none());
    }

    #[test]
    fn reject_pipe() {
        assert!(detect_shell_file_write("cat /tmp/a.txt | head > /tmp/b.txt").is_none());
    }

    #[test]
    fn reject_bare_dollar_var() {
        // IMPORTANT-2 修复(code-reviewer):`$VAR` / `${VAR}` 在 double-quote
        // 内 shell 会展开,如果 normalize 成 V4A 写字面 `$VAR` 跟模型意图
        // divergence,reject 强制走 apply_patch 或 透传 shell。
        assert!(detect_shell_file_write(r#"echo "hello $USER" > /tmp/a.txt"#).is_none());
        assert!(detect_shell_file_write(r#"echo "${HOME}/file" > /tmp/a.txt"#).is_none());
        assert!(detect_shell_file_write(r#"printf "$1" > /tmp/a.txt"#).is_none());
        // 即使在 single-quote 内 $ 也 reject(简单一致策略,模型可以改 apply_patch)
        assert!(detect_shell_file_write("echo '$USER literal' > /tmp/a.txt").is_none());
    }

    #[test]
    fn reject_path_with_metachar() {
        assert!(detect_shell_file_write("echo 'x' > /tmp/file with space.txt").is_none());
        assert!(detect_shell_file_write("echo 'x' > /tmp/*.txt").is_none());
    }

    #[test]
    fn reject_echo_with_flags() {
        assert!(detect_shell_file_write("echo -e 'a\\nb' > /tmp/a.txt").is_none());
        assert!(detect_shell_file_write("echo -n 'x' > /tmp/a.txt").is_none());
    }

    #[test]
    fn reject_printf_format_string() {
        assert!(detect_shell_file_write("printf 'hello %s' > /tmp/a.txt").is_none());
    }

    #[test]
    fn passthrough_unrelated_commands() {
        assert!(detect_shell_file_write("ls -la /tmp").is_none());
        assert!(detect_shell_file_write("cat /tmp/a.txt").is_none());
        assert!(detect_shell_file_write("grep -r pattern /tmp").is_none());
        assert!(detect_shell_file_write("python3 /tmp/a.py").is_none());
    }

    // ===== V4A patch envelope structure =====

    #[test]
    fn patch_envelope_correct_structure() {
        let cmd = "echo 'a' > /tmp/x.txt";
        let p = detect_shell_file_write(cmd).unwrap();
        assert!(p.patch_input.starts_with("*** Begin Patch\n"));
        assert!(p.patch_input.contains("*** Add File: /tmp/x.txt\n"));
        assert!(p.patch_input.ends_with("*** End Patch\n"));
    }

    #[test]
    fn empty_body_single_plus_line() {
        let cmd = "echo '' > /tmp/empty.txt";
        let p = detect_shell_file_write(cmd).unwrap();
        assert!(p.patch_input.contains("*** Add File: /tmp/empty.txt\n+\n*** End Patch\n"));
    }

    #[test]
    fn relative_path_accepted() {
        let cmd = "echo 'x' > ./local-file.txt";
        let p = detect_shell_file_write(cmd).expect("relative path 应接受");
        assert_eq!(p.target_path, "./local-file.txt");
    }
}
