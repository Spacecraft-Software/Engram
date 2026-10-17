// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Idempotent writes to files engram manages on a user's behalf.
//!
//! Engram writes outside its database in two shapes, and this module is the
//! single implementation of both:
//!
//! * [`WritePolicy::Spliced`] — the file belongs to someone else and engram
//!   owns only the region between a pair of sentinels. `AGENTS.md` is the
//!   canonical case: everything outside the sentinels is preserved
//!   byte-for-byte (see [`crate::rules`]).
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
        .find(|dir| is_git_root(dir))
        .map(Path::to_path_buf)
}

/// Name of the repository a *linked worktree* belongs to, when `root` is one.
///
/// `root` must be a working-tree root as [`find_git_root`] returns it. A linked
/// worktree stores `.git` as a `gitdir: <repo>/.git/worktrees/<name>` file, so
/// naming it after its own directory files its history under a throwaway name
/// — every Claude Code worktree is `.claude/worktrees/<random-slug>`, and one
/// project's memory ended up split across `bravais` and `lamprey` that way.
///
/// The test is the `commondir` file inside the gitdir, which git writes for
/// linked worktrees only. A submodule also uses a `.git` file, but its gitdir
/// (`<super>/.git/modules/<name>`) has no `commondir`, and naming a submodule
/// after its superproject would be wrong — so it returns `None` and keeps its
/// own basename.
///
/// Returns `None` for an ordinary checkout, a submodule, or anything that
/// cannot be read; callers fall back to the root's basename.
pub fn linked_worktree_repo_name(root: &Path) -> Option<String> {
    let pointer = std::fs::read_to_string(root.join(".git")).ok()?;
    let gitdir = pointer.trim().strip_prefix("gitdir:")?.trim();
    let gitdir = root.join(gitdir);
    let commondir = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = gitdir.join(commondir.trim());
    let common = common.canonicalize().unwrap_or(common);

    // `<repo>/.git` for a normal repository; `<repo>.git` (or any name) for a
    // bare one, where the directory itself carries the repository's name.
    let named = if common.file_name()? == ".git" {
        common.parent()?.file_name()?
    } else {
        common.file_name()?
    };
    let name = named.to_string_lossy();
    let name = name.strip_suffix(".git").unwrap_or(&name);
    (!name.is_empty()).then(|| name.to_string())
}

/// True when `dir` is the root of a real working tree.
///
/// Existence of `.git` alone is not enough. An empty `.git` directory is not a
/// repository, and one sitting somewhere shared — `/tmp/.git` is the case that
/// prompted this — silently captures every path beneath it: `save-chat` would
/// resolve its project root to `/tmp`, write `/tmp/chat/`, and add `chat/` to
/// `/tmp/.gitignore`. Both forms git actually produces are accepted: a
/// directory containing `HEAD`, and the `gitdir:` *file* a worktree or submodule
/// checkout uses.
fn is_git_root(dir: &Path) -> bool {
    let git = dir.join(".git");
    match std::fs::metadata(&git) {
        Ok(m) if m.is_dir() => git.join("HEAD").exists(),
        Ok(m) if m.is_file() => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty `.git` directory is not a repository, and must not capture
    /// every path beneath it.
    ///
    /// This is not hypothetical: an empty `/tmp/.git` on the author's machine
    /// made `save-chat` resolve its project root to `/tmp`, where it created
    /// `/tmp/chat/` and added `chat/` to `/tmp/.gitignore`. Existence of `.git`
    /// is not the test; being a working tree is.
    #[test]
    fn an_empty_git_directory_is_not_a_working_tree() {
        let tmp = std::env::temp_dir().join(format!("engram-gitroot-{}", std::process::id()));
        let shallow = tmp.join("empty-git");
        let nested = shallow.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("create tree");
        std::fs::create_dir_all(shallow.join(".git")).expect("empty .git dir");
        assert_eq!(
            find_git_root(&nested),
            None,
            "an empty .git directory must not be treated as a repository root"
        );

        // A real repository has HEAD.
        std::fs::write(shallow.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        assert_eq!(find_git_root(&nested).as_deref(), Some(shallow.as_path()));

        // A worktree or submodule uses a `.git` file instead.
        let linked = tmp.join("worktree");
        let linked_nested = linked.join("x");
        std::fs::create_dir_all(&linked_nested).expect("create worktree tree");
        std::fs::write(linked.join(".git"), "gitdir: /elsewhere\n").expect("write gitdir file");
        assert_eq!(
            find_git_root(&linked_nested).as_deref(),
            Some(linked.as_path())
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Lays out what `git worktree add` and `git submodule add` produce,
    /// without needing `git`: a main repository `repo/`, a linked worktree
    /// whose gitdir carries `commondir`, and a submodule whose gitdir does not.
    fn plant_repo_with_worktree_and_submodule(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let main = tmp.join("repo");
        let git = main.join(".git");
        std::fs::create_dir_all(&git).expect("create .git");
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");

        // A relative `commondir`, exactly as git writes it.
        let wt_gitdir = git.join("worktrees").join("lamprey");
        std::fs::create_dir_all(&wt_gitdir).expect("create worktree gitdir");
        std::fs::write(wt_gitdir.join("commondir"), "../..\n").expect("write commondir");
        let worktree = main.join(".claude").join("worktrees").join("lamprey");
        std::fs::create_dir_all(&worktree).expect("create worktree");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .expect("write worktree pointer");

        // A submodule's pointer is relative, and its gitdir has no commondir.
        let sm_gitdir = git.join("modules").join("vendored");
        std::fs::create_dir_all(&sm_gitdir).expect("create module gitdir");
        std::fs::write(sm_gitdir.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        let submodule = main.join("vendored");
        std::fs::create_dir_all(&submodule).expect("create submodule");
        std::fs::write(submodule.join(".git"), "gitdir: ../.git/modules/vendored\n")
            .expect("write submodule pointer");

        (main, worktree, submodule)
    }

    /// A linked worktree is named after the repository it belongs to; a
    /// submodule and an ordinary checkout are not renamed.
    #[test]
    fn only_a_linked_worktree_takes_its_repository_name() {
        let tmp = std::env::temp_dir().join(format!("engram-worktree-{}", std::process::id()));
        let (main, worktree, submodule) = plant_repo_with_worktree_and_submodule(&tmp);

        assert_eq!(
            linked_worktree_repo_name(&worktree).as_deref(),
            Some("repo"),
            "a worktree must be named after its main repository, not its slug"
        );
        assert_eq!(
            linked_worktree_repo_name(&submodule),
            None,
            "a submodule is its own project and keeps its own name"
        );
        assert_eq!(linked_worktree_repo_name(&main), None);

        // `find_git_root` still stops at the worktree: `rule sync` writes there.
        let nested = worktree.join("src");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        assert_eq!(find_git_root(&nested).as_deref(), Some(worktree.as_path()));

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A bare repository's directory carries the name, minus `.git`.
    #[test]
    fn a_worktree_of_a_bare_repository_drops_the_git_suffix() {
        let tmp = std::env::temp_dir().join(format!("engram-bare-{}", std::process::id()));
        let bare = tmp.join("bravais.git");
        let gitdir = bare.join("worktrees").join("main");
        std::fs::create_dir_all(&gitdir).expect("create bare gitdir");
        std::fs::write(gitdir.join("commondir"), "../..\n").expect("write commondir");
        let worktree = tmp.join("main");
        std::fs::create_dir_all(&worktree).expect("create worktree");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect("write pointer");

        assert_eq!(
            linked_worktree_repo_name(&worktree).as_deref(),
            Some("bravais")
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

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
