// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Reader for Qwen Code's transcripts.
//!
//! Qwen writes Claude Code's record envelope — `uuid`, `parentUuid`,
//! `sessionId`, `timestamp`, `type` and a verbatim `cwd` on every record — so
//! [`super::claude_code::read`] serves it unchanged, exactly as it already
//! serves OpenClaude. Two differences are real, and both are small:
//!
//! * The body lives at `message.parts[]` rather than `message.content[]`, and
//!   its entries carry no `type` field. The shared reader handles both, because
//!   an untyped block carrying text is text.
//! * Sessions sit one level deeper — `projects/<mangled-cwd>/chats/` — which is
//!   the only reason this module exists rather than pointing the harness spec
//!   straight at the Claude Code reader.
//!
//! The mangled directory is produced by [`super::mangle_cwd`], so this reader
//! inherits the correction that made dotted paths reachable.

use std::path::Path;

use super::{session_id_from_path, sort_newest_first, SessionRef, TranscriptError};
use crate::harness::{self, HarnessSpec};

/// Lists the Qwen sessions recorded for `cwd`.
///
/// # Errors
///
/// [`TranscriptError::NoHome`] when `$HOME` is unset, and an I/O error when the
/// directory exists but cannot be read. A missing directory is an empty list,
/// not an error — an installed harness that has never run in this project is
/// not a failure.
pub fn sessions(spec: &HarnessSpec, cwd: &Path) -> Result<Vec<SessionRef>, TranscriptError> {
    let base = harness::sessions_dir(spec).ok_or(TranscriptError::NoHome)?;
    let dir = base.join(super::mangle_cwd(cwd)).join("chats");

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(TranscriptError::Io(e)),
    };

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(TranscriptError::Io)?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let meta = entry.metadata().map_err(TranscriptError::Io)?;
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        found.push((
            SessionRef {
                harness: spec.id,
                session_id: session_id_from_path(&path),
                path: path.to_string_lossy().into_owned(),
                cwd: Some(cwd.to_string_lossy().into_owned()),
                bytes: meta.len(),
            },
            modified,
        ));
    }

    sort_newest_first(&mut found);
    Ok(found.into_iter().map(|(s, _)| s).collect())
}
