//! 编译期 i18n:读 src/i18n/strings.toml,phf_codegen 出
//! `static TABLE: phf::Map<&'static str, [&'static str; 2]>` 到 OUT_DIR/i18n_data.rs。
//!
//! - 数组形态 `[zh, en]`,index 与 `Locale` enum 强对齐
//! - 缺哪一边就 fallback 用另一边(parse_strings 处理)
//! - 增删 i18n key 时 cargo build 自动重跑(rerun-if-changed)

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Deserialize)]
struct Entry {
    #[serde(default)]
    zh: Option<String>,
    #[serde(default)]
    en: Option<String>,
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let src = manifest.join("src/i18n/strings.toml");
    println!("cargo:rerun-if-changed={}", src.display());

    let raw = fs::read_to_string(&src).expect("read strings.toml");
    let table: BTreeMap<String, Entry> = toml::from_str(&raw).expect("parse strings.toml");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let out_path = out_dir.join("i18n_data.rs");
    let mut out = fs::File::create(&out_path).expect("create i18n_data.rs");

    writeln!(
        out,
        "/// 编译期生成。共 {} 个 key。请编辑 src/i18n/strings.toml,不要直接改本文件。",
        table.len()
    )
    .unwrap();
    writeln!(out, "pub const KEY_COUNT: usize = {};", table.len()).unwrap();
    writeln!(out).unwrap();

    let mut codegen = phf_codegen::Map::<&str>::new();
    // 把 owned String 缓冲到 Vec,再借用进 phf builder(避免临时引用悬空)
    let owned: Vec<(String, String)> = table
        .iter()
        .map(|(k, v)| {
            let zh = v.zh.clone().or_else(|| v.en.clone()).unwrap_or_default();
            let en = v.en.clone().or_else(|| v.zh.clone()).unwrap_or_default();
            // 给 phf_codegen entry 一个 Rust 字面量表达式
            (
                k.clone(),
                format!("[{}, {}]", rust_str_literal(&zh), rust_str_literal(&en)),
            )
        })
        .collect();
    for (k, expr) in &owned {
        codegen.entry(k.as_str(), expr);
    }
    write!(
        out,
        "pub static TABLE: phf::Map<&'static str, [&'static str; 2]> = {};\n",
        codegen.build()
    )
    .unwrap();
}

fn rust_str_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                write!(out, "\\u{{{:x}}}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
