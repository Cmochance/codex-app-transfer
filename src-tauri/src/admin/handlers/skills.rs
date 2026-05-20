//! `/api/codex/skills/*` — Codex CLI `~/.codex/skills/` 目录 file-level snapshot
//! backup / restore (#25 Skills tab).
//!
//! 4 endpoints:
//! - GET  `/list`       — 列 `~/.codex/skills/<name>/` 各子目录概况
//! - POST `/backup`     — 整目录 tar.gz 到 `~/.codex-app-transfer/skills-backups/<ts>.tar.gz`
//! - GET  `/backups`    — 列已有 backup 文件
//! - POST `/restore`    — 从指定 backup 还原(覆盖 `~/.codex/skills/`)

use std::path::PathBuf;

use axum::{http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::json;

use super::super::services::skills_backup::{
    backup_skills, list_backups, list_skills, restore_backup,
};
use super::common::err;

fn resolve_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(PathBuf::from)
}

fn skills_dir() -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home.join(".codex").join("skills"))
}

fn backup_dir() -> Result<PathBuf, String> {
    let home = resolve_home().ok_or_else(|| "HOME / USERPROFILE not set".to_owned())?;
    Ok(home.join(".codex-app-transfer").join("skills-backups"))
}

#[derive(Debug, Deserialize, Default)]
pub struct RestoreInput {
    pub filename: String,
}

pub async fn list_handler() -> impl IntoResponse {
    let skills = match skills_dir() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match list_skills(&skills) {
        Ok(entries) => Json(json!({
            "success": true,
            "skillsDir": skills.display().to_string(),
            "count": entries.len(),
            "entries": entries,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn backup_handler() -> impl IntoResponse {
    let skills = match skills_dir() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let backups = match backup_dir() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match backup_skills(&skills, &backups) {
        Ok(path) => Json(json!({
            "success": true,
            "backupPath": path.display().to_string(),
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn backups_handler() -> impl IntoResponse {
    let backups = match backup_dir() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match list_backups(&backups) {
        Ok(entries) => Json(json!({
            "success": true,
            "backupDir": backups.display().to_string(),
            "count": entries.len(),
            "backups": entries,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn restore_handler(Json(input): Json<RestoreInput>) -> impl IntoResponse {
    let skills = match skills_dir() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let backups = match backup_dir() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match restore_backup(&skills, &backups, &input.filename) {
        Ok(()) => Json(json!({"success": true, "restored": input.filename})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}
