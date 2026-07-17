// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Output mode detection cascade — spacecraft-cli-standard SKILL.md §5.

use std::io::IsTerminal;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum OutputMode {
    HumanWithColor,
    HumanNoColor,
    Json,
}

impl OutputMode {
    pub fn is_machine(self) -> bool {
        matches!(self, Self::Json)
    }
}

/// First matching condition wins: explicit flag > agent env var > TTY > non-TTY > fallback.
pub fn resolve_mode(json_flag: bool, no_color_flag: bool) -> OutputMode {
    if json_flag {
        return OutputMode::Json;
    }
    for var in ["AI_AGENT", "AGENT", "CI"] {
        if std::env::var(var).is_ok() {
            return OutputMode::Json;
        }
    }
    if !std::io::stdout().is_terminal() {
        return OutputMode::Json;
    }
    if no_color_flag || std::env::var("NO_COLOR").is_ok() {
        OutputMode::HumanNoColor
    } else {
        OutputMode::HumanWithColor
    }
}
