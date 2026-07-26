// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Durable project rules — normative policy that outlives a session.
//!
//! A rule is an ordinary [`crate::store::Memory`] row with `role = "rule"` and
//! a stable, caller-chosen `rule_id`. Reusing the memories table rather than
//! adding a parallel one keeps one write path, one FTS index, and identical
//! behavior on every surface.
//!
//! # Why `sync` is not merely an export
//!
//! A row in SQLite never reaches a model's context on its own. Rendering rules
//! into `AGENTS.md` / `CLAUDE.md` — files that agent harnesses load
//! automatically — is what actually makes a rule take effect. The database is
//! the durable source of truth; the managed markdown block is the delivery
//! mechanism. Storing a rule without syncing it stores a rule nobody reads.

use crate::store::Rule;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// `role` value marking a memory row as a durable rule rather than a message.
pub const ROLE: &str = "rule";

/// `status` of a rule that is currently binding.
pub const STATUS_ACTIVE: &str = "active";
/// `status` of a withdrawn rule.
///
/// Retiring tombstones rather than deletes. Engram is a memory store; erasing
/// the record of a policy that once applied would contradict the point of it.
/// "We used to require X and stopped" is exactly the kind of thing a later
/// session needs to be able to find, and `search` still reaches retired rules.
pub const STATUS_RETIRED: &str = "retired";

/// Files `engram rule sync` writes when no `--file` is supplied. Both are
/// auto-loaded by agent harnesses, which is the entire point of syncing.
pub const DEFAULT_TARGETS: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Opening sentinel. Matched by prefix so attributes can be added to the tag
/// later without orphaning blocks written by an older version.
const BEGIN_PREFIX: &str = "<!-- engram:rules:begin";
const END_MARKER: &str = "<!-- engram:rules:end -->";

/// Rule text containing this substring could terminate its own managed block
/// and corrupt every subsequent `sync`, so it is rejected at write time.
pub const SENTINEL_NEEDLE: &str = "engram:rules:";

/// How the effective scope was determined, surfaced so callers can tell an
/// explicit choice from a directory-name guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScopeOrigin {
    /// Passed explicitly via `--scope` or the MCP `scope` argument.
    Explicit,
    /// Read from the `ENGRAM_SCOPE` environment variable.
    Env,
    /// Basename of the enclosing git working tree.
    GitRoot,
    /// Basename of the current working directory (no git tree found).
    Cwd,
}

/// An effective scope plus the directory `sync` treats as the project root.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedScope {
    pub name: String,
    pub root: PathBuf,
    pub origin: ScopeOrigin,
}

/// Resolves the scope to operate on.
///
/// First match wins: explicit argument, then `ENGRAM_SCOPE`, then the git
/// working-tree basename, then the current directory's basename. The returned
/// `root` is the git working tree when one encloses the current directory, so
/// `sync` writes to the repository root rather than wherever the process
/// happens to have been started.
///
/// # Errors
///
/// Returns an error if the current working directory cannot be read.
pub fn resolve_scope(explicit: Option<&str>) -> std::io::Result<ResolvedScope> {
    let cwd = std::env::current_dir()?;
    let git_root = find_git_root(&cwd);
    let root = git_root.clone().unwrap_or_else(|| cwd.clone());

    if let Some(name) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(ResolvedScope { name: name.to_string(), root, origin: ScopeOrigin::Explicit });
    }
    if let Some(name) = std::env::var("ENGRAM_SCOPE").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return Ok(ResolvedScope { name, root, origin: ScopeOrigin::Env });
    }
    if let Some(dir) = git_root {
        let name = basename(&dir);
        return Ok(ResolvedScope { name, root: dir, origin: ScopeOrigin::GitRoot });
    }
    let name = basename(&cwd);
    Ok(ResolvedScope { name, root, origin: ScopeOrigin::Cwd })
}

/// Walks up from `start` looking for `.git`. Tested with `exists` rather than
/// `is_dir` because linked worktrees and submodules store `.git` as a file.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find(|dir| dir.join(".git").exists()).map(Path::to_path_buf)
}

fn basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string())
}

/// Rejects a rule id that would be unusable as a stable key or as a markdown
/// heading.
///
/// # Errors
///
/// Returns a human-readable reason when the id is empty, contains whitespace,
/// or exceeds [`MAX_RULE_ID_LEN`].
pub fn validate_rule_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err("rule id must not be empty".to_string());
    }
    if id.chars().any(char::is_whitespace) {
        return Err(format!("rule id '{id}' must not contain whitespace; use kebab-case"));
    }
    if id.len() > MAX_RULE_ID_LEN {
        return Err(format!("rule id is {} characters; the maximum is {MAX_RULE_ID_LEN}", id.len()));
    }
    Ok(())
}

/// Generous upper bound — long enough for a descriptive kebab-case id, short
/// enough that a malformed argument (e.g. the rule body passed as the id) is
/// caught rather than stored.
pub const MAX_RULE_ID_LEN: usize = 96;

/// Rejects rule text that is empty or would corrupt the managed block.
///
/// # Errors
///
/// Returns a human-readable reason when the text is blank or contains the
/// sentinel needle.
pub fn validate_rule_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("rule text must not be empty".to_string());
    }
    if text.contains(SENTINEL_NEEDLE) {
        return Err(format!(
            "rule text must not contain '{SENTINEL_NEEDLE}' — it would terminate the managed block in synced files"
        ));
    }
    Ok(())
}

/// Renders the managed block for a scope.
///
/// Deliberately carries no generation timestamp: the block must be a pure
/// function of the rules so that re-running `sync` with unchanged rules
/// produces a byte-identical file, keeping the operation idempotent and
/// diff-free. Per-rule recency lives in each rule's own `updated_at`.
pub fn render_block(scope: &str, rules: &[Rule]) -> String {
    let mut out = String::new();
    out.push_str(&format!("{BEGIN_PREFIX} scope=\"{scope}\" count=\"{}\" -->\n", rules.len()));
    out.push_str("<!-- Generated by `engram rule sync`. Edits inside this block are overwritten. -->\n\n");
    out.push_str("## Project rules\n\n");

    if rules.is_empty() {
        out.push_str(&format!("No rules recorded for scope `{scope}`.\n\n"));
    } else {
        out.push_str(&format!(
            "Durable rules for scope `{scope}`. These are binding for work in this \
             repository. The source of truth is the engram database — run \
             `engram rule list --scope {scope}` to read them, `engram rule add` to \
             change one, then `engram rule sync` to regenerate this block. Do not \
             hand-edit between the sentinels.\n\n"
        ));
        for rule in rules {
            out.push_str(&format!("### `{}`\n\n", rule.rule_id));
            out.push_str(rule.text.trim());
            out.push_str("\n\n");
            out.push_str(&format!(
                "*Added {} · updated {} · by {}.*\n\n",
                rule.created_at, rule.updated_at, rule.agent
            ));
        }
    }

    out.push_str(END_MARKER);
    out
}

/// What a single target file did during a sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileOutcome {
    /// File did not exist and was written from scratch.
    Created,
    /// Managed block was inserted or replaced.
    Updated,
    /// File already contained exactly this block; nothing written.
    Unchanged,
}

/// Per-file result of a sync, including whether the write was actually made.
#[derive(Debug, Clone, Serialize)]
pub struct SyncedFile {
    pub path: String,
    pub outcome: FileOutcome,
    /// True when `--dry-run` suppressed the write that `outcome` describes.
    pub dry_run: bool,
}

/// Writes `block` into `path`, replacing an existing managed block or
/// appending one, and leaving all content outside the sentinels untouched.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read, or if the write
/// fails. Nothing is written when `dry_run` is set or the content is unchanged.
pub fn sync_file(path: &Path, block: &str, dry_run: bool) -> std::io::Result<SyncedFile> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };

    let (next, outcome) = match existing {
        Some(current) => {
            let next = splice_block(&current, block);
            let outcome = if next == current { FileOutcome::Unchanged } else { FileOutcome::Updated };
            (next, outcome)
        }
        None => (format!("{block}\n"), FileOutcome::Created),
    };

    if outcome != FileOutcome::Unchanged && !dry_run {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &next)?;
    }

    Ok(SyncedFile { path: path.to_string_lossy().into_owned(), outcome, dry_run })
}

/// Replaces the region between the sentinels, or appends the block when the
/// file has no managed region yet. Content outside the sentinels is preserved
/// verbatim — that is the whole reason for the sentinel scheme.
fn splice_block(existing: &str, block: &str) -> String {
    if let Some(begin) = existing.find(BEGIN_PREFIX) {
        if let Some(rel_end) = existing[begin..].find(END_MARKER) {
            let end = begin + rel_end + END_MARKER.len();
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..begin]);
            out.push_str(block);
            out.push_str(&existing[end..]);
            return out;
        }
        // Opening sentinel with no closing one: a truncated or hand-mangled
        // block. Appending would leave two openers and make the next sync
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

/// Absolute paths to write for a scope: the caller's `--file` list if given,
/// otherwise [`DEFAULT_TARGETS`] resolved against the project root.
pub fn target_paths(root: &Path, requested: &[PathBuf]) -> Vec<PathBuf> {
    if requested.is_empty() {
        return DEFAULT_TARGETS.iter().map(|name| root.join(name)).collect();
    }
    requested
        .iter()
        .map(|p| if p.is_absolute() { p.clone() } else { root.join(p) })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, text: &str) -> Rule {
        Rule {
            rule_id: id.to_string(),
            scope: "demo".to_string(),
            text: text.to_string(),
            agent: "claude-code".to_string(),
            created_at: "2026-07-26T00:00:00Z".to_string(),
            updated_at: "2026-07-26T00:00:00Z".to_string(),
            retired: false,
        }
    }

    #[test]
    fn render_is_deterministic() {
        let rules = vec![rule("a", "First."), rule("b", "Second.")];
        assert_eq!(render_block("demo", &rules), render_block("demo", &rules));
    }

    #[test]
    fn splice_preserves_surrounding_content() {
        let block = render_block("demo", &[rule("a", "First.")]);
        let original = format!("# Title\n\nIntro paragraph.\n\n{block}\n\nTrailing section.\n");
        let updated = splice_block(&original, &render_block("demo", &[rule("a", "Changed.")]));

        assert!(updated.starts_with("# Title\n\nIntro paragraph.\n"));
        assert!(updated.ends_with("Trailing section.\n"));
        assert!(updated.contains("Changed."));
        assert!(!updated.contains("First."));
    }

    #[test]
    fn splice_is_idempotent() {
        let block = render_block("demo", &[rule("a", "First.")]);
        let once = splice_block("# Title\n", &block);
        let twice = splice_block(&once, &block);
        assert_eq!(once, twice);
        assert_eq!(once.matches(BEGIN_PREFIX).count(), 1);
    }

    #[test]
    fn splice_repairs_block_missing_its_terminator() {
        let mangled = format!("# Title\n\n{BEGIN_PREFIX} scope=\"demo\" count=\"1\" -->\nhalf a block\n");
        let block = render_block("demo", &[rule("a", "First.")]);
        let repaired = splice_block(&mangled, &block);

        assert_eq!(repaired.matches(BEGIN_PREFIX).count(), 1);
        assert_eq!(repaired.matches(END_MARKER).count(), 1);
        assert!(!repaired.contains("half a block"));
    }

    #[test]
    fn rule_text_carrying_a_sentinel_is_rejected() {
        assert!(validate_rule_text("plain text").is_ok());
        assert!(validate_rule_text("  ").is_err());
        assert!(validate_rule_text("see <!-- engram:rules:end -->").is_err());
    }

    #[test]
    fn rule_ids_reject_whitespace_and_overlong_input() {
        assert!(validate_rule_id("skill-description-1000").is_ok());
        assert!(validate_rule_id("has space").is_err());
        assert!(validate_rule_id("").is_err());
        assert!(validate_rule_id(&"x".repeat(MAX_RULE_ID_LEN + 1)).is_err());
    }
}

// Rust guideline compliant 2026-05-18
