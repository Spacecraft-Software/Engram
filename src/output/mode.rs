// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Output mode detection cascade — spacecraft-cli-standard SKILL.md §5,
//! plus the Standard §18.1 accessible-mode toggle.

use crate::cli::Format;
use std::io::IsTerminal;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum OutputMode {
    HumanWithColor,
    /// Plain human output — also what accessible mode (§18) renders as:
    /// engram's human output is already linear and animation-free, so the
    /// accessible contract reduces to "no color, status tags present".
    HumanNoColor,
    Json,
    Jsonl,
    Csv,
}

impl OutputMode {
    pub fn is_machine(self) -> bool {
        matches!(self, Self::Json | Self::Jsonl | Self::Csv)
    }
}

/// First matching condition wins: explicit `--format`/`--json` > agent env
/// var > TTY > non-TTY > fallback.
///
/// Agent vars (`AI_AGENT`, `AGENT`) are presence-based — set to any
/// non-empty value counts; harnesses export descriptive strings, never `1`.
/// `CI` is the one value-carrying exception: truthy means set and not
/// `""`/`0`/`false`.
pub fn resolve_mode(
    format_flag: Option<Format>,
    json_flag: bool,
    no_color_flag: bool,
    accessible_flag: bool,
) -> OutputMode {
    if let Some(fmt) = format_flag {
        return match fmt {
            Format::Json => OutputMode::Json,
            Format::Jsonl => OutputMode::Jsonl,
            Format::Csv => OutputMode::Csv,
        };
    }
    if json_flag {
        return OutputMode::Json;
    }
    if is_agent_env() {
        return OutputMode::Json;
    }
    if !std::io::stdout().is_terminal() {
        return OutputMode::Json;
    }
    if accessible_resolved(accessible_flag)
        || no_color_flag
        || env_non_empty("NO_COLOR")
        || std::env::var("TERM").is_ok_and(|t| t == "dumb")
    {
        OutputMode::HumanNoColor
    } else {
        OutputMode::HumanWithColor
    }
}

fn is_agent_env() -> bool {
    env_non_empty("AI_AGENT") || env_non_empty("AGENT") || ci_truthy()
}

fn env_non_empty(var: &str) -> bool {
    std::env::var(var).is_ok_and(|v| !v.is_empty())
}

fn ci_truthy() -> bool {
    std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// §18.1 activation cascade: the `--accessible` flag always wins; otherwise
/// `SPACECRAFT_A11Y` decides (`0` is an explicit off, anything else non-empty
/// is on). Silence means standard rendering.
fn accessible_resolved(flag: bool) -> bool {
    if flag {
        return true;
    }
    std::env::var("SPACECRAFT_A11Y").is_ok_and(|v| !v.is_empty() && v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-var manipulation is process-global, so these tests only exercise
    // the pure paths (explicit flags) that ignore the environment.
    #[test]
    fn explicit_format_flag_wins_unconditionally() {
        assert_eq!(
            resolve_mode(Some(Format::Jsonl), false, false, false),
            OutputMode::Jsonl
        );
        assert_eq!(
            resolve_mode(Some(Format::Csv), true, true, true),
            OutputMode::Csv
        );
        assert_eq!(
            resolve_mode(Some(Format::Json), false, false, false),
            OutputMode::Json
        );
    }

    #[test]
    fn json_flag_beats_everything_below_it() {
        assert_eq!(resolve_mode(None, true, true, true), OutputMode::Json);
    }
}
