// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! ISO 8601 UTC timestamps. Never local time, never SystemTime Debug-formatted.

/// Current time as ISO 8601 / RFC 3339 with a mandatory `Z` suffix.
pub fn now_iso8601() -> String {
    jiff::Timestamp::now().to_string()
}
