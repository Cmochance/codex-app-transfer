//! Vendored from ccusage `rust/crates/ccusage/src/main.rs:64-95` —
//! `CliError` + `Result<T>` + `cli_error()` helper. ccusage 把这套定义在 main.rs 顶层,
//! 通过 `crate::{Result, cli_error}` 给所有 sub-module 用。本项目 vendor 时把这部分
//! 拎出独立 module,等价语义。

use std::{fmt, io};

#[derive(Debug)]
pub struct CliError(String);

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CliError>;

pub fn cli_error(message: impl Into<String>) -> CliError {
    CliError(message.into())
}
