// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Idempotent writes to files engram manages on a user's behalf.
//!
//! Engram writes outside its database in two shapes, and this module is the
//! single implementation of both:
//!
//! * [`WritePolicy::Spliced`] — the file belongs to someone else and engram
//!   owns only the region between a pair of sentinels. `AGENTS.md` and
//!   `CLAUDE.md` are the canonical case: everything outside the sentinels is
//!   preserved byte-for-byte (see [`crate::rules`]).
//! * [`WritePolicy::Owned`] — engram authored the whole file and rewrites it
//!   wholesale. A chat archive is the canonical case.
//!
//! Both policies share the property that makes them safe to run from a hook,
//! a commit gate, or a slash command: **the write is a pure function of the
//! inputs, so an unchanged input performs no write at all** and reports
//! [`FileOutcome::Unchanged`]. Nothing here appends, and nothing here deletes.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// The comment pair delimiting an engram-managed region inside a foreign file.
///
/// Only markdown gets a sentinel: HTML comments survive every renderer that
/// matters, whereas JSON and TOML have no comment form that a serializer will
/// round-trip. Structured config is merged key-by-key, never spliced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sentinel {
    /// Opening tag, matched by *prefix* so attributes can be added to it later
    /// without orphaning blocks written by an older version of engram.
    pub begin_prefix: &'static str,
    /// Closing tag, matched exactly.
    pub end_marker: &'static str,
    /// Substring that must not appear in caller-supplied body text: content
    /// carrying it could terminate its own managed block and corrupt every
    /// subsequent write. Callers reject it at write time.
    pub needle: &'static str,
}

/// Sentinel for the `engram rule sync` block.
pub const RULES: Sentinel = Sentinel {
    begin_prefix: "<!-- engram:rules:begin",
    end_marker: "<!-- engram:rules:end -->",
    needle: "engram:rules:",
};

/// How much of a target file engram is entitled to rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePolicy {
    /// Rewrite only the sentinel region; preserve everything outside it.
    Spliced(Sentinel),
    /// Rewrite the entire file. Only for files engram authored.
    Owned,
}

/// What a single target file did during a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileOutcome {
    /// File did not exist and was written from scratch.
    Created,
    /// File existed and its content changed.
    Updated,
    /// File already held exactly this content; nothing written.
    Unchanged,
}

/// Per-file result, including whether the write described by `outcome` was
/// actually performed.
#[derive(Debug, Clone, Serialize)]
pub struct ManagedFile {
    pub path: String,
    pub outcome: FileOutcome,
    /// True when `--dry-run` suppressed the write that `outcome` describes.
    pub dry_run: bool,
    /// Why the write was declined, when it was. Absent on a normal write, so
    /// the serialized shape is unchanged for callers that never skip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Writes `body` into `path` under `policy`, creating parent directories as
/// needed, and reports what it did.
///
/// Nothing is written when `dry_run` is set or the resulting content is
/// byte-identical to what is already there — the second condition is what
/// makes repeated invocation free of spurious diffs and mtime churn.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read, if a parent
/// directory cannot be created, or if the write fails.
pub fn write_managed(
    path: &Path,
    body: &str,
    policy: WritePolicy,
    dry_run: bool,
) -> std::io::Result<ManagedFile> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };

    let (next, outcome) = match (existing, policy) {
        (Some(current), WritePolicy::Spliced(sentinel)) => {
            let next = splice_block(&current, body, &sentinel);
            let outcome = if next == current {
                FileOutcome::Unchanged
            } else {
                FileOutcome::Updated
            };
            (next, outcome)
        }
        // A spliced block landing in a new file still gets the trailing
        // newline that `splice_block` would have given it.
        (None, WritePolicy::Spliced(_)) => (format!("{body}\n"), FileOutcome::Created),
        (Some(current), WritePolicy::Owned) => {
            if current == body {
                (current, FileOutcome::Unchanged)
            } else {
                (body.to_string(), FileOutcome::Updated)
            }
        }
        (None, WritePolicy::Owned) => (body.to_string(), FileOutcome::Created),
    };

    if outcome != FileOutcome::Unchanged && !dry_run {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &next)?;
    }

    Ok(ManagedFile {
        path: path.to_string_lossy().into_owned(),
        outcome,
        dry_run,
        reason: None,
    })
}

/// Replaces the region between `sentinel`'s tags, or appends the block when
/// the file has no managed region yet. Content outside the sentinels is
/// preserved verbatim — that is the whole reason for the sentinel scheme.
pub fn splice_block(existing: &str, block: &str, sentinel: &Sentinel) -> String {
    if let Some(begin) = existing.find(sentinel.begin_prefix) {
        if let Some(rel_end) = existing[begin..].find(sentinel.end_marker) {
            let end = begin + rel_end + sentinel.end_marker.len();
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..begin]);
            out.push_str(block);
            out.push_str(&existing[end..]);
            return out;
        }
        // Opening sentinel with no closing one: a truncated or hand-mangled
        // block. Appending would leave two openers and make the next write
        // ambiguous, so treat everything from the opener onward as the block.
        let mut out = String::with_capacity(begin + block.len() + 1);
        out.push_str(&existing[..begin]);
        out.push_str(block);
        out.push('\n');
        return out;
    }

    let mut out = String::with_capacity(existing.len() + block.len() + 2);
    out.push_str(existing);
    if !existing.is_empty() && !existing.ends_with('\n') {
        out.push('\n');
    }
    if !existing.is_empty() {
        out.push('\n');
    }
    out.push_str(block);
    out.push('\n');
    out
}

/// Walks up from `start` looking for `.git`. Tested with `exists` rather than
/// `is_dir` because linked worktrees and submodules store `.git` as a file.
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(body: &str) -> String {
        format!(
            "{} count=\"1\" -->\n{body}\n{}",
            RULES.begin_prefix, RULES.end_marker
        )
    }

    #[test]
    fn splice_preserves_surrounding_content() {
        let existing = "# Title\n\nIntro paragraph.\n\nTrailing note.\n";
        let out = splice_block(existing, &block("first"), &RULES);
        assert!(out.starts_with("# Title\n"));
        assert!(out.contains("Trailing note."));
        assert!(out.contains("first"));
    }

    #[test]
    fn splice_is_idempotent() {
        let first = splice_block("# Title\n", &block("one"), &RULES);
        let second = splice_block(&first, &block("one"), &RULES);
        assert_eq!(first, second);
        assert_eq!(second.matches(RULES.begin_prefix).count(), 1);
    }

    #[test]
    fn splice_replaces_rather_than_accumulates() {
        let first = splice_block("# Title\n", &block("one"), &RULES);
        let second = splice_block(&first, &block("two"), &RULES);
        assert!(second.contains("two"));
        assert!(!second.contains("one"));
        assert_eq!(second.matches(RULES.begin_prefix).count(), 1);
    }

    #[test]
    fn splice_repairs_block_missing_its_terminator() {
        let mangled = format!(
            "# Title\n\n{} count=\"1\" -->\nhalf a block",
            RULES.begin_prefix
        );
        let out = splice_block(&mangled, &block("repaired"), &RULES);
        assert_eq!(out.matches(RULES.begin_prefix).count(), 1);
        assert_eq!(out.matches(RULES.end_marker).count(), 1);
        assert!(out.contains("repaired"));
        assert!(!out.contains("half a block"));
    }

    #[test]
    fn owned_write_is_created_then_unchanged() {
        let dir = std::env::temp_dir().join(format!(
            "engram-managed-{}-{}",
            std::process::id(),
            "owned-unchanged"
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("archive.texi");
        let _ = std::fs::remove_file(&path);

        let first = write_managed(&path, "body\n", WritePolicy::Owned, false).expect("first write");
        assert_eq!(first.outcome, FileOutcome::Created);

        let second =
            write_managed(&path, "body\n", WritePolicy::Owned, false).expect("second write");
        assert_eq!(second.outcome, FileOutcome::Unchanged);

        let third =
            write_managed(&path, "other\n", WritePolicy::Owned, false).expect("third write");
        assert_eq!(third.outcome, FileOutcome::Updated);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "other\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn owned_dry_run_writes_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "engram-managed-{}-{}",
            std::process::id(),
            "owned-dry-run"
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("archive.texi");
        let _ = std::fs::remove_file(&path);

        let planned = write_managed(&path, "body\n", WritePolicy::Owned, true).expect("dry run");
        assert_eq!(planned.outcome, FileOutcome::Created);
        assert!(planned.dry_run);
        assert!(!path.exists(), "dry run must not create the file");

        std::fs::remove_dir_all(&dir).ok();
    }
}

// Rust guideline compliant 2026-05-18
