// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Archiving a scope's history as a Texinfo document.
//!
//! Lives apart from the CLI because two surfaces need it: `engram save-chat`
//! and the MCP `save_chat` tool. Both call [`save_chat`]; neither reimplements
//! any part of it.
//!
//! The renderer is a pure function of the scope's contents, exactly as
//! [`crate::rules::render_block`] is, and for the same reason: re-archiving an
//! unchanged scope must produce byte-identical output so the write is skipped
//! and the file's history stays diff-free.

#[derive(serde::Serialize)]
pub struct SaveChatResult {
    pub scope: String,
    pub scope_origin: crate::rules::ScopeOrigin,
    pub root: String,
    pub file: crate::managed_file::ManagedFile,
    pub signed_by: String,
    pub messages_saved: usize,
    /// What engram did to the project's `.gitignore`, if anything. Reported
    /// rather than performed silently: it edits a tracked file.
    pub gitignore: GitignoreOutcome,
}

/// Engram's handling of the project `.gitignore` during an archive.
///
/// This is a self-describing structure rather than the bare
/// `gitignore_updated: bool` it replaces. A lone boolean forced the reader to
/// supply three facts from memory — *whose* `.gitignore`, *what* entry, and
/// who the actor was — and `false` in particular read as a failure report
/// ("something did not get updated") when it actually means engram found the
/// entry already present and correctly left the file alone.
#[derive(serde::Serialize)]
pub struct GitignoreOutcome {
    /// The `.gitignore` engram considered, absolute.
    pub path: String,
    /// The entry engram ensures is present, so archives are never committed.
    pub entry: &'static str,
    pub action: GitignoreAction,
    /// One sentence, naming engram as the actor and the file as the object.
    /// Machine callers read `action`; this exists so a human reading the raw
    /// envelope needs no further explanation.
    pub detail: String,
}

/// What engram did to `.gitignore`.
#[derive(serde::Serialize, PartialEq, Eq, Debug, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum GitignoreAction {
    /// Engram appended the entry.
    Added,
    /// The entry was already there; engram wrote nothing.
    AlreadyIgnored,
    /// Dry run: engram would have appended the entry, and wrote nothing.
    WouldAdd,
}

/// Fallback signature when neither `--model` nor any of the model environment
/// variables is set. Names the archive's author as unknown rather than
/// attributing it to whichever model happened to be popular when this line
/// was written.
const UNKNOWN_MODEL: &str = "unknown-model";

pub fn save_chat(
    resolved: &crate::rules::ResolvedScope,
    mems: Vec<crate::store::Memory>,
    custom_file: Option<std::path::PathBuf>,
    model: Option<String>,
    dry_run: bool,
) -> Result<SaveChatResult, Box<dyn std::error::Error>> {
    let chat_dir = resolved.root.join("chat");

    let model_name = model
        .or_else(|| std::env::var("MODEL").ok())
        .or_else(|| std::env::var("LLM_MODEL").ok())
        .or_else(|| std::env::var("AI_AGENT").ok())
        .or_else(|| std::env::var("AGENT").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| UNKNOWN_MODEL.to_string());

    // A relative `--file` resolves against the project root, matching
    // `rule sync --file`; the previous behavior resolved against the process
    // cwd, so the same command produced different paths from different
    // directories.
    let target_file = match custom_file {
        Some(f) if f.is_absolute() => f,
        Some(f) => resolved.root.join(f),
        None => {
            let timestamp = crate::time::now_iso8601().replace(':', "-");
            chat_dir.join(format!("{}.texi", timestamp))
        }
    };

    let body = render_texinfo(&resolved.name, &mems, &model_name);
    let file = crate::managed_file::write_managed(
        &target_file,
        &body,
        crate::managed_file::WritePolicy::Owned,
        dry_run,
    )?;

    let gitignore = sync_gitignore(&resolved.root, dry_run)?;

    Ok(SaveChatResult {
        scope: resolved.name.clone(),
        scope_origin: resolved.origin,
        root: resolved.root.to_string_lossy().into_owned(),
        file,
        signed_by: model_name,
        messages_saved: mems.len(),
        gitignore,
    })
}

/// The entry engram keeps in `.gitignore`, so archived transcripts are never
/// committed to the project by accident.
const CHAT_IGNORE_ENTRY: &str = "chat/";

/// Adds `chat/` to the project's `.gitignore` when it is not already ignored,
/// and describes what it did.
///
/// Writes nothing when the entry is already present or `dry_run` is set — the
/// two cases are distinct in the report ([`GitignoreAction::AlreadyIgnored`]
/// versus [`GitignoreAction::WouldAdd`]), because the old boolean returned
/// `true` for a dry run and so claimed an update that never happened.
///
/// # Errors
///
/// Returns an error if `.gitignore` exists but cannot be read, or the write
/// fails.
fn sync_gitignore(root: &std::path::Path, dry_run: bool) -> std::io::Result<GitignoreOutcome> {
    let path = root.join(".gitignore");
    let display = path.display().to_string();
    let describe = |action: GitignoreAction| {
        let detail = match action {
            GitignoreAction::Added => format!(
                "engram added `{CHAT_IGNORE_ENTRY}` to this project's .gitignore ({display}), \
                 so archived chat transcripts are not committed"
            ),
            GitignoreAction::AlreadyIgnored => format!(
                "engram made no change: `{CHAT_IGNORE_ENTRY}` is already ignored by this \
                 project's .gitignore ({display})"
            ),
            GitignoreAction::WouldAdd => format!(
                "dry run, nothing written: engram would add `{CHAT_IGNORE_ENTRY}` to this \
                 project's .gitignore ({display})"
            ),
        };
        GitignoreOutcome {
            path: display.clone(),
            entry: CHAT_IGNORE_ENTRY,
            action,
            detail,
        }
    };

    let mut content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };

    if content
        .lines()
        .any(|l| matches!(l.trim(), "chat" | "chat/" | "/chat" | "/chat/"))
    {
        return Ok(describe(GitignoreAction::AlreadyIgnored));
    }
    if dry_run {
        return Ok(describe(GitignoreAction::WouldAdd));
    }

    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(CHAT_IGNORE_ENTRY);
    content.push('\n');
    std::fs::write(&path, &content)?;
    Ok(describe(GitignoreAction::Added))
}

/// Renders a scope's history as a complete Texinfo document.
///
/// Pure by design, exactly as [`rules::render_block`] is: re-running
/// `save-chat` over an unchanged scope must produce byte-identical output so
/// the write reports `unchanged` and the file's history stays diff-free.
///
/// That is why the provenance comment carries the *last message's* timestamp
/// rather than the export's wall clock. "When was this archived" is already
/// recorded by the default filename and the file's mtime; "how current is
/// this archive" is the question the header can answer without making every
/// re-run a spurious rewrite.
///
/// The previous implementation appended to an existing file by stripping its
/// trailing `@bye` and re-emitting the whole history again, so every re-run
/// duplicated every message. This renders the document whole, every time.
pub fn render_texinfo(scope: &str, mems: &[crate::store::Memory], signed_by: &str) -> String {
    let title = escape_texinfo(scope);
    let mut out = String::new();

    out.push_str("\\input texinfo @c -*-texinfo-*-\n");
    out.push_str("@c %**start of header\n");
    out.push_str("@documentencoding UTF-8\n");
    out.push_str(&format!("@settitle Chat history for scope: {title}\n"));
    out.push_str("@c %**end of header\n\n");

    out.push_str("@c Generated by `engram save-chat`. Edits are overwritten.\n");
    out.push_str(&format!("@c Archived by: {}\n", escape_texinfo(signed_by)));
    out.push_str(&format!("@c Messages: {}\n", mems.len()));
    if let Some(last) = mems.last() {
        out.push_str(&format!(
            "@c Through: {}\n",
            escape_texinfo(&last.created_at)
        ));
    }
    out.push('\n');

    out.push_str("@node Top\n");
    out.push_str(&format!("@top Chat history for scope: {title}\n\n"));

    // `@chapter`, not `@section`: `@top` sits at chapter level, so a section
    // here skips a level and makeinfo warns. One chapter per message keeps
    // the hierarchy legal without an artificial wrapper heading.
    for mem in mems {
        out.push_str(&format!(
            "@chapter Message by {} ({}) at {}\n",
            escape_texinfo(&mem.agent),
            escape_texinfo(&mem.role),
            escape_texinfo(&mem.created_at)
        ));
        out.push_str(&escape_texinfo(&mem.content));
        out.push_str("\n\n");
    }

    out.push_str("@bye\n");
    out
}

/// Escapes the three characters Texinfo treats as markup in body text.
fn escape_texinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '@' => out.push_str("@@"),
            '{' => out.push_str("@{"),
            '}' => out.push_str("@}"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(agent: &str, role: &str, content: &str, created_at: &str) -> crate::store::Memory {
        crate::store::Memory {
            id: format!("id-{created_at}"),
            agent: agent.to_string(),
            scope: "demo".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            created_at: created_at.to_string(),
            rule_id: None,
            updated_at: None,
            valid_from: None,
            valid_to: None,
            superseded_by: None,
        }
    }

    #[test]
    fn render_is_deterministic() {
        let mems = vec![
            memory("claude-code", "user", "First.", "2026-08-01T00:00:00Z"),
            memory(
                "claude-code",
                "assistant",
                "Second.",
                "2026-08-01T00:01:00Z",
            ),
        ];
        assert_eq!(
            render_texinfo("demo", &mems, "opus-5"),
            render_texinfo("demo", &mems, "opus-5"),
            "re-rendering an unchanged scope must be byte-identical"
        );
    }

    /// The regression this whole rewrite exists for: the previous
    /// implementation appended by stripping `@bye`, so every message appeared
    /// once per invocation.
    #[test]
    fn render_does_not_duplicate_messages() {
        let mems = vec![memory(
            "claude-code",
            "note",
            "the only message",
            "2026-08-01T00:00:00Z",
        )];
        let out = render_texinfo("demo", &mems, "opus-5");
        assert_eq!(out.matches("the only message").count(), 1);
        assert_eq!(out.matches("@bye").count(), 1);
        assert_eq!(out.matches("@node Top").count(), 1);
    }

    #[test]
    fn render_emits_a_valid_texinfo_header() {
        let mems = vec![memory("a", "note", "x", "2026-08-01T00:00:00Z")];
        let out = render_texinfo("demo", &mems, "opus-5");
        assert!(out.starts_with("\\input texinfo"));
        // Mandatory once real transcripts (non-ASCII) start arriving.
        assert!(out.contains("@documentencoding UTF-8"));
        assert!(out.contains("@settitle Chat history for scope: demo"));
        assert!(out.ends_with("@bye\n"));
    }

    #[test]
    fn render_escapes_texinfo_markup_everywhere() {
        let mems = vec![memory(
            "agent@host",
            "note",
            "braces {like this} and an @ sign",
            "2026-08-01T00:00:00Z",
        )];
        let out = render_texinfo("scope@x", &mems, "model{1}");
        assert!(out.contains("braces @{like this@} and an @@ sign"));
        assert!(out.contains("agent@@host"));
        assert!(out.contains("scope@@x"));
        assert!(out.contains("model@{1@}"));
    }

    /// The provenance header must be data-derived, not wall-clock, or every
    /// re-run would rewrite the file. See [`render_texinfo`].
    #[test]
    fn render_carries_the_last_message_timestamp_not_now() {
        let mems = vec![
            memory("a", "note", "one", "2026-08-01T00:00:00Z"),
            memory("a", "note", "two", "2026-08-02T00:00:00Z"),
        ];
        let out = render_texinfo("demo", &mems, "opus-5");
        assert!(out.contains("@c Through: 2026-08-02T00:00:00Z"));
        assert!(out.contains("@c Messages: 2"));
    }
}

// Rust guideline compliant 2026-05-18
