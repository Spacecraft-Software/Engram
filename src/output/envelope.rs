// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! The Response<T> envelope. Every sub-command and API route returns this
//! in machine mode — see spacecraft-cli-standard SKILL.md §6.

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Response<T: Serialize> {
    pub metadata: Metadata,
    pub data: T,
}

#[derive(Debug, Serialize)]
pub struct Metadata {
    pub tool: &'static str,
    pub version: &'static str,
    pub command: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pagination: Option<Pagination>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_agent: Option<String>,
    /// Present (and `true`) only on `--dry-run` responses — the
    /// spacecraft-cli-standard validation-safety §4 contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// Present only when the caller asked for token budgeting: what was
    /// kept, what was cut, and by which estimator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<crate::retrieval::BudgetReport>,
}

#[derive(Debug, Serialize)]
pub struct Pagination {
    pub page: u32,
    pub per_page: u32,
    pub total: u64,
}

impl<T: Serialize> Response<T> {
    pub fn new(command: impl Into<String>, data: T) -> Self {
        Self {
            metadata: Metadata {
                tool: env!("CARGO_PKG_NAME"),
                version: env!("CARGO_PKG_VERSION"),
                command: command.into(),
                timestamp: crate::time::now_iso8601(),
                pagination: None,
                tool_agent: detect_tool_agent(),
                dry_run: None,
                budget: None,
            },
            data,
        }
    }

    #[expect(
        dead_code,
        reason = "pagination lands with the M1/M3 list-shaped endpoints"
    )]
    pub fn with_pagination(mut self, page: u32, per_page: u32, total: u64) -> Self {
        self.metadata.pagination = Some(Pagination {
            page,
            per_page,
            total,
        });
        self
    }

    pub fn with_dry_run(mut self) -> Self {
        self.metadata.dry_run = Some(true);
        self
    }

    pub fn with_budget(mut self, b: crate::retrieval::BudgetReport) -> Self {
        self.metadata.budget = Some(b);
        self
    }
}

/// Detects which agent CLI is driving this process, for telemetry only —
/// never used to change output shape (the JSON schema stays identical
/// regardless of which agent is calling).
fn detect_tool_agent() -> Option<String> {
    for var in [
        "CLAUDECODE",
        "CURSOR_AGENT",
        "GEMINI_CLI",
        "AI_AGENT",
        "AGENT",
    ] {
        if std::env::var(var).is_ok() {
            return Some(var.to_ascii_lowercase().replace('_', "-"));
        }
    }
    None
}
