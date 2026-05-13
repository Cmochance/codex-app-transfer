use serde_json::Value;

const TOOL_OUTPUT_INLINE_MAX_CHARS: usize = 4_000;
const TOOL_OUTPUT_HEAD_CHARS: usize = 1_200;
const TOOL_OUTPUT_TAIL_CHARS: usize = 1_200;
const TOOL_OUTPUT_VISIBLE_MAX_CHARS: usize = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredToolOutput {
    pub artifact_id: String,
    pub call_id: Option<String>,
}

pub(crate) trait ToolOutputArtifactStore {
    fn save_tool_output(
        &self,
        call_id: Option<&str>,
        kind: &'static str,
        raw: &str,
    ) -> StoredToolOutput;
}

pub(crate) fn normalize_tool_output_for_context_with_store<S: ToolOutputArtifactStore>(
    call_id: Option<&str>,
    output_value: Value,
    artifact_store: Option<&S>,
) -> String {
    let raw = match output_value {
        Value::String(s) => s,
        other => serde_json::to_string(&other).unwrap_or_default(),
    };
    if raw.chars().count() <= TOOL_OUTPUT_INLINE_MAX_CHARS {
        return raw;
    }
    let kind = classify_tool_output(&raw);
    let artifact = artifact_store.map(|store| store.save_tool_output(call_id, kind, &raw));
    build_bounded_tool_output_summary(&raw, kind, artifact.as_ref())
}

fn build_bounded_tool_output_summary(
    raw: &str,
    kind: &str,
    artifact: Option<&StoredToolOutput>,
) -> String {
    let original_chars = raw.chars().count();
    let original_lines = raw.lines().count();
    let mut out = String::new();

    out.push_str("[Tool output stored outside model context]\n");
    out.push_str("Visible content below is a bounded evidence summary, not the full raw output.\n");
    if let Some(artifact) = artifact {
        out.push_str(&format!("Artifact ID: {}\n", artifact.artifact_id));
        if let Some(call_id) = artifact.call_id.as_deref() {
            out.push_str(&format!("Tool call ID: {call_id}\n"));
        }
    } else {
        out.push_str("Artifact ID: unavailable; raw payload could not be stored.\n");
    }
    out.push_str(&format!("Artifact kind: {kind}\n"));
    out.push_str(&format!(
        "Original size: {original_chars} chars across {original_lines} lines.\n"
    ));
    if let Some(token_count) = extract_marker_value(raw, "Original token count:") {
        out.push_str(&format!("Original token count: {token_count}\n"));
    }
    if let Some(total_lines) = extract_marker_value(raw, "Total output lines:") {
        out.push_str(&format!("Reported output lines: {total_lines}\n"));
    }

    let path_hints = extract_path_hints(raw, 12);
    if !path_hints.is_empty() {
        out.push_str("Path hints:\n");
        for path in path_hints {
            out.push_str("- ");
            out.push_str(&path);
            out.push('\n');
        }
    }

    let url_hints = extract_url_hints(raw, 12);
    if !url_hints.is_empty() {
        out.push_str("URL hints:\n");
        for url in url_hints {
            out.push_str("- ");
            out.push_str(&url);
            out.push('\n');
        }
    }

    out.push_str("\n--- Begin head excerpt ---\n");
    out.push_str(&take_first_chars(raw, TOOL_OUTPUT_HEAD_CHARS));
    out.push_str("\n--- End head excerpt ---\n");
    out.push_str("\n--- Begin tail excerpt ---\n");
    out.push_str(&take_last_chars(raw, TOOL_OUTPUT_TAIL_CHARS));
    out.push_str("\n--- End tail excerpt ---\n");
    out.push_str(&format!(
        "\n[Omitted raw tool output from model context. Original size: {original_chars} chars.]"
    ));

    if out.chars().count() > TOOL_OUTPUT_VISIBLE_MAX_CHARS {
        let mut trimmed = take_first_chars(&out, TOOL_OUTPUT_VISIBLE_MAX_CHARS);
        trimmed.push_str("\n[Tool output compression summary truncated to visible budget.]");
        return trimmed;
    }
    out
}

fn classify_tool_output(raw: &str) -> &'static str {
    let sample = raw.chars().take(20_000).collect::<String>();
    let trimmed = sample.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<Value>(trimmed).is_ok()
    {
        return "json";
    }
    if sample.contains("https://")
        || sample.contains("http://")
        || sample.contains("web_search")
        || sample.contains("Search results")
        || sample.contains("source:")
    {
        return "web_or_search";
    }
    if sample.contains("Process exited with code")
        || sample.contains("Exit code")
        || sample.contains("Wall time:")
        || sample.contains("Output:")
    {
        return "command_output";
    }
    if !extract_path_hints(&sample, 1).is_empty() {
        return "file_or_code_output";
    }
    "opaque_tool_output"
}

fn extract_marker_value(raw: &str, marker: &str) -> Option<String> {
    let start = raw.find(marker)?;
    let rest = &raw[start + marker.len()..];
    let value = rest
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn extract_url_hints(raw: &str, max: usize) -> Vec<String> {
    let mut urls = Vec::new();
    for token in raw.lines().take(200).flat_map(str::split_whitespace) {
        let candidate = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
            )
        });
        if !(candidate.starts_with("http://") || candidate.starts_with("https://")) {
            continue;
        }
        if urls.iter().any(|existing| existing == candidate) {
            continue;
        }
        urls.push(candidate.to_owned());
        if urls.len() >= max {
            break;
        }
    }
    urls
}

fn extract_path_hints(raw: &str, max: usize) -> Vec<String> {
    let mut paths = Vec::new();
    for line in raw.lines().take(200) {
        for token in line.split_whitespace() {
            let candidate = token
                .trim_matches(|ch: char| {
                    matches!(
                        ch,
                        '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}'
                    )
                })
                .split(':')
                .next()
                .unwrap_or("");
            if !(candidate.starts_with('/') || candidate.starts_with("./")) {
                continue;
            }
            if !candidate.contains('.') {
                continue;
            }
            if paths.iter().any(|existing| existing == candidate) {
                continue;
            }
            paths.push(candidate.to_owned());
            if paths.len() >= max {
                return paths;
            }
        }
    }
    paths
}

fn take_first_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn take_last_chars(value: &str, max: usize) -> String {
    let mut chars = value.chars().rev().take(max).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}
