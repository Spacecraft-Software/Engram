// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Structured error type shared by CLI, MCP, and HTTP surfaces.
//! See spacecraft-cli-standard references/exit-codes-errors.md.

use serde::Serialize;

#[derive(Debug, Serialize, Copy, Clone)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    NotFound,
    InvalidArgument,
    Conflict,
    InternalError,
    StorageError,
}

#[derive(Debug, Serialize)]
pub struct AppError {
    pub code: ErrorCode,
    pub exit_code: i32,
    pub message: String,
    pub hint: String,
    pub timestamp: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
}

impl AppError {
    pub fn new(code: ErrorCode, exit_code: i32, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code,
            exit_code,
            message: message.into(),
            hint: hint.into(),
            timestamp: crate::time::now_iso8601(),
            command: std::env::args().collect::<Vec<_>>().join(" "),
            docs_url: Some("https://Engram.SpacecraftSoftware.org/errors".to_string()),
        }
    }

    pub fn storage(err: rusqlite::Error) -> Self {
        Self::new(
            ErrorCode::StorageError,
            1,
            format!("storage error: {err}"),
            "check that the engram database file is not locked by another process",
        )
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(
            ErrorCode::NotFound,
            3,
            format!("{what} not found"),
            "check the id with `engram memory list` first",
        )
    }

    /// Single-line JSON to stderr — PowerShell fragments multi-line stderr.
    pub fn emit_to_stderr(&self) {
        #[derive(Serialize)]
        struct Wrapper<'a> {
            error: &'a AppError,
        }
        let line = serde_json::to_string(&Wrapper { error: self }).expect("AppError serializes");
        eprintln!("{line}");
    }

    pub fn emit_human(&self) {
        use owo_colors::OwoColorize;
        eprintln!("{}: {}", "error".red().bold(), self.message.red());
        eprintln!("       {}: {}", "hint".yellow(), self.hint);
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::storage(e)
    }
}
