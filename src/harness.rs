// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! The agent harnesses engram knows how to work with.
//!
//! One static table, consulted wherever engram needs to reason about a
//! harness rather than about itself: reading a session transcript
//! ([`crate::transcript`]), and eventually installing command files into one.
//!
//! Everything here is pure data plus `Path::exists`. Detection never runs a
//! subprocess, never reads a config file's contents, and never creates a
//! directory — a harness that is not installed must stay not installed.
//!
//! # Why transcript support is an enum, not a `bool`
//!
//! Engram must never report "0 turns captured" as success for a harness it
//! simply cannot read. Making the absence of a reader a *variant carrying its
//! reason* means the no-reader case cannot reach a success path by omission:
//! the caller has to match on it, and the reason is already written down.

use serde::Serialize;
use std::path::PathBuf;

/// A harness engram recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Codex,
    Opencode,
    Antigravity,
    Goose,
    CopilotCli,
    Qwen,
}

/// Which reader implementation handles a harness's transcripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderKind {
    ClaudeCode,
    Codex,
}

/// Whether engram can read a harness's session transcripts, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptSupport {
    /// A reader exists.
    Reader(ReaderKind),
    /// Transcripts are stored in a readable, line-oriented form, but engram
    /// does not ship a reader for it yet.
    NotImplemented { detail: &'static str },
    /// Transcripts are not stored in a form engram can read at all.
    Unsupported { detail: &'static str },
}

/// Everything engram knows about one harness.
#[derive(Debug, Clone, Copy)]
pub struct HarnessSpec {
    pub id: Harness,
    /// Stable kebab-case identifier, as accepted by `--harness` and emitted
    /// in output.
    pub name: &'static str,
    /// Home-relative paths probed to decide whether the harness is installed.
    /// First hit wins; an entry may be a file or a directory.
    pub probe: &'static [&'static str],
    /// Home-relative directory holding session transcripts, when the harness
    /// has one. `None` when transcripts live somewhere unstructured.
    pub sessions_dir: Option<&'static str>,
    pub transcript: TranscriptSupport,
    /// Home-relative directory the harness loads user commands from.
    ///
    /// `None` means the harness has **no command surface engram can write**,
    /// which is reported plainly rather than papered over. Of the seven
    /// harnesses here, only some can host a slash command; claiming otherwise
    /// would make `install` look broken on the rest.
    pub commands_dir: Option<&'static str>,
    /// File-name pattern for a command in `commands_dir`. `{name}` is
    /// replaced by the command's short name.
    pub command_file: &'static str,
    /// Whether this harness reads YAML frontmatter at the top of a command
    /// file. Codex prompts are plain markdown and would render the
    /// frontmatter as literal text.
    pub command_frontmatter: bool,
    /// Where this harness registers MCP servers, when engram knows. Read to
    /// discover which database the user already shares between harnesses.
    pub mcp_config: Option<McpConfigSource>,
    /// Home-relative settings file holding this harness's lifecycle hooks.
    ///
    /// `None` for every harness but Claude Code, which is the only one on a
    /// surveyed machine with a hook system engram could write to.
    pub hooks_config: Option<&'static str>,
}

/// Where a harness's MCP registration lives, so `install` can discover which
/// database the user actually shares between harnesses rather than guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpConfigSource {
    /// A JSON file with an `mcpServers` (or `mcp`) object at the top level.
    Json(&'static str),
    /// The same, but JSON **with comments** — Opencode's `opencode.jsonc`.
    /// Comments are stripped before parsing; the file itself is never
    /// rewritten, because a serde round-trip would destroy them.
    Jsonc(&'static str),
    /// A TOML file with `[mcp_servers.<name>]` tables.
    Toml(&'static str),
}

/// Every harness engram recognizes, in a stable order.
///
/// The three entries with no transcript reader are listed deliberately rather
/// than omitted: `--list` reporting "antigravity: present, no reader, here is
/// why" is honest, whereas silently not mentioning it reads as a bug.
pub const ALL: &[HarnessSpec] = &[
    HarnessSpec {
        id: Harness::ClaudeCode,
        name: "claude-code",
        // Claude Code keeps MCP registration in ~/.claude.json and everything
        // else under ~/.claude/, and either alone means it is installed.
        probe: &[".claude", ".claude.json"],
        sessions_dir: Some(".claude/projects"),
        transcript: TranscriptSupport::Reader(ReaderKind::ClaudeCode),
        commands_dir: Some(".claude/commands"),
        command_file: "engram-{name}.md",
        command_frontmatter: true,
        mcp_config: Some(McpConfigSource::Json(".claude.json")),
        hooks_config: Some(".claude/settings.json"),
    },
    HarnessSpec {
        id: Harness::Codex,
        name: "codex",
        probe: &[".codex/config.toml", ".codex"],
        sessions_dir: Some(".codex/sessions"),
        transcript: TranscriptSupport::Reader(ReaderKind::Codex),
        commands_dir: Some(".codex/prompts"),
        command_file: "engram-{name}.md",
        command_frontmatter: false,
        mcp_config: Some(McpConfigSource::Toml(".codex/config.toml")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Opencode,
        name: "opencode",
        probe: &[".config/opencode/opencode.jsonc", ".config/opencode"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "opencode's session storage has not been surveyed",
        },
        commands_dir: Some(".config/opencode/command"),
        command_file: "engram-{name}.md",
        command_frontmatter: true,
        mcp_config: Some(McpConfigSource::Jsonc(".config/opencode/opencode.jsonc")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Antigravity,
        name: "antigravity",
        probe: &[".gemini/antigravity", ".antigravity"],
        sessions_dir: None,
        transcript: TranscriptSupport::Unsupported {
            detail: "antigravity stores conversations as protocol buffers plus a SQLite summaries database; there is no line-oriented transcript to read",
        },
        commands_dir: None,
        command_file: "engram-{name}.md",
        command_frontmatter: true,
        mcp_config: Some(McpConfigSource::Json(".gemini/antigravity/mcp_config.json")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Goose,
        name: "goose",
        probe: &[".config/goose/config.yaml", ".config/goose"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "goose's session storage has not been surveyed",
        },
        commands_dir: None,
        command_file: "engram-{name}.md",
        command_frontmatter: true,
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::CopilotCli,
        name: "copilot-cli",
        probe: &[".copilot/mcp-config.json", ".copilot"],
        sessions_dir: None,
        transcript: TranscriptSupport::Unsupported {
            detail: "copilot cli stores sessions in session-store.db, a SQLite database with an undocumented schema",
        },
        commands_dir: None,
        command_file: "engram-{name}.md",
        command_frontmatter: true,
        mcp_config: Some(McpConfigSource::Json(".copilot/mcp-config.json")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Qwen,
        name: "qwen",
        probe: &[".qwen/settings.json", ".qwen"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "qwen's session storage has not been surveyed",
        },
        commands_dir: None,
        command_file: "engram-{name}.md",
        command_frontmatter: true,
        mcp_config: Some(McpConfigSource::Json(".qwen/settings.json")),
        hooks_config: None,
    },
];

/// A harness plus whether it appears to be installed for the current user.
#[derive(Debug, Clone, Serialize)]
pub struct Detected {
    pub harness: Harness,
    pub name: &'static str,
    pub present: bool,
    /// The probe path that matched, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `null` when engram has no reader for this harness; the reason is then
    /// in `reader_detail`.
    pub reader: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reader_detail: Option<&'static str>,
}

/// Looks a harness up by its enum id.
pub fn spec(id: Harness) -> &'static HarnessSpec {
    ALL.iter()
        .find(|h| h.id == id)
        .expect("every Harness variant has a spec")
}

/// The user's home directory.
///
/// Reads `$HOME` directly rather than going through the `dirs` crate. That is
/// a **testability decision, not an oversight**: every harness path in this
/// module derives from here, so a test that sets `HOME` to a temporary
/// directory is hermetic by construction. A crate that caches the value or
/// falls back to a platform API would leak the developer's real home into
/// filesystem tests. Do not "improve" this into a dependency.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Resolves a home-relative path for the current user.
pub fn in_home(relative: &str) -> Option<PathBuf> {
    home_dir().map(|h| h.join(relative))
}

/// The first probe path that exists, if any.
fn probe_hit(spec: &HarnessSpec) -> Option<PathBuf> {
    let home = home_dir()?;
    spec.probe.iter().map(|p| home.join(p)).find(|p| p.exists())
}

/// Reports every known harness and whether it is installed.
pub fn detect() -> Vec<Detected> {
    ALL.iter().map(describe).collect()
}

/// Reports one harness's detection state.
pub fn describe(spec: &'static HarnessSpec) -> Detected {
    let hit = probe_hit(spec);
    let (reader, reader_detail) = match spec.transcript {
        TranscriptSupport::Reader(_) => (Some(spec.name), None),
        TranscriptSupport::NotImplemented { detail }
        | TranscriptSupport::Unsupported { detail } => (None, Some(detail)),
    };
    Detected {
        harness: spec.id,
        name: spec.name,
        present: hit.is_some(),
        path: hit.map(|p| p.to_string_lossy().into_owned()),
        reader,
        reader_detail,
    }
}

/// Absolute path to a harness's sessions directory, when it has one and the
/// home directory can be resolved.
pub fn sessions_dir(spec: &HarnessSpec) -> Option<PathBuf> {
    spec.sessions_dir.and_then(in_home)
}

/// Absolute path to a harness's command directory, when it has one.
pub fn commands_dir(spec: &HarnessSpec) -> Option<PathBuf> {
    spec.commands_dir.and_then(in_home)
}

/// The database path this harness already registered engram against.
///
/// Worth the trouble because every harness on a machine typically points at
/// one shared database. A generated command file that omitted `--db` would
/// fall back to clap's relative `engram.db` default and quietly write to a
/// different store than the one the user's agents read.
///
/// Parsed with deliberately narrow, format-specific scanning rather than a
/// full deserializer: engram must not need a TOML parser, and must not choke
/// on the JSONC comments Opencode's config carries.
pub fn registered_db(spec: &HarnessSpec) -> Option<PathBuf> {
    let source = spec.mcp_config?;
    let relative = match source {
        McpConfigSource::Json(p) | McpConfigSource::Jsonc(p) | McpConfigSource::Toml(p) => p,
    };
    let text = std::fs::read_to_string(in_home(relative)?).ok()?;
    let args = match source {
        McpConfigSource::Json(_) => json_engram_args(&text)?,
        McpConfigSource::Jsonc(_) => json_engram_args(&strip_jsonc_comments(&text))?,
        McpConfigSource::Toml(_) => toml_engram_args(&text)?,
    };
    db_from_args(&args)
}

/// Blanks out `//` and `/* */` comments so a JSONC document can be parsed.
///
/// String-aware: `"https://example.com"` must survive, and so must an escaped
/// quote inside one. Comments are replaced with spaces rather than removed so
/// byte offsets — and therefore any parse error positions — still line up.
///
/// This only ever feeds a *read*. Engram never rewrites a JSONC file: a serde
/// round-trip would silently delete the user's comments.
fn strip_jsonc_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                out.push(' ');
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                    out.push(' ');
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                out.push(' ');
                chars.next();
                out.push(' ');
                let mut prev_star = false;
                for next in chars.by_ref() {
                    out.push(if next == '\n' { '\n' } else { ' ' });
                    if prev_star && next == '/' {
                        break;
                    }
                    prev_star = next == '*';
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The value following a `--db` flag in an argument list.
fn db_from_args(args: &[String]) -> Option<PathBuf> {
    args.iter()
        .position(|a| a == "--db")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
}

/// `args` of the `engram` entry in an `mcpServers`-style JSON document.
fn json_engram_args(text: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    // `mcpServers` is the common key; Opencode uses `mcp`.
    let servers = value
        .get("mcpServers")
        .or_else(|| value.get("mcp"))?
        .as_object()?;
    let entry = servers.get("engram")?;
    // `command` may be a bare string with `args` alongside, or (Opencode) an
    // array whose first element is the program.
    let args = match entry.get("args").or_else(|| entry.get("command")) {
        Some(serde_json::Value::Array(a)) => a,
        _ => return None,
    };
    Some(
        args.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
    )
}

/// `args` of `[mcp_servers.engram]` in a TOML document.
///
/// Scanned line by line rather than parsed: pulling in a TOML dependency to
/// read one array from one optional file is not a trade worth making.
fn toml_engram_args(text: &str) -> Option<Vec<String>> {
    let mut in_section = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == "[mcp_servers.engram]";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("args") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let inner = rest.trim().trim_start_matches('[').trim_end_matches(']');
        return Some(
            inner
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        );
    }
    None
}

/// Installed harnesses that engram can actually read transcripts from.
pub fn readable_and_present() -> Vec<&'static HarnessSpec> {
    ALL.iter()
        .filter(|s| matches!(s.transcript, TranscriptSupport::Reader(_)))
        .filter(|s| probe_hit(s).is_some())
        .collect()
}

/// Guesses the harness engram is running under from the environment.
///
/// Only used to pick a default; an explicit `--harness` always wins, and an
/// ambiguous filesystem is an error rather than a guess.
pub fn from_env() -> Option<&'static HarnessSpec> {
    // Each harness sets its own marker variable. Checked non-empty because a
    // harness that exports the name with an empty value means nothing.
    const MARKERS: &[(&str, Harness)] = &[
        ("CLAUDECODE", Harness::ClaudeCode),
        ("CODEX_SANDBOX", Harness::Codex),
        ("OPENCODE", Harness::Opencode),
        ("GEMINI_CLI", Harness::Antigravity),
    ];
    MARKERS
        .iter()
        .find(|(var, _)| std::env::var_os(var).is_some_and(|v| !v.is_empty()))
        .map(|(_, id)| spec(*id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_exactly_one_spec() {
        for entry in ALL {
            assert_eq!(spec(entry.id).name, entry.name);
        }
        let mut names: Vec<_> = ALL.iter().map(|h| h.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "harness names must be unique");
    }

    /// The whole point of the enum: a harness without a reader carries the
    /// reason, so no caller can report an empty capture as a success.
    #[test]
    fn readerless_harnesses_carry_a_reason() {
        for entry in ALL {
            match entry.transcript {
                TranscriptSupport::Reader(_) => {}
                TranscriptSupport::NotImplemented { detail }
                | TranscriptSupport::Unsupported { detail } => {
                    assert!(
                        !detail.trim().is_empty(),
                        "{} has no reader and no reason",
                        entry.name
                    );
                }
            }
        }
    }

    #[test]
    fn jsonc_comments_are_stripped_without_harming_strings() {
        let src = r#"{
  // a line comment
  "url": "https://example.com/not-a-comment", /* trailing block */
  "escaped": "a \" then // still in the string",
  "n": 1
}"#;
        let stripped = strip_jsonc_comments(src);
        let value: serde_json::Value =
            serde_json::from_str(&stripped).expect("stripped JSONC parses");
        // A `//` inside a string is data, not a comment.
        assert_eq!(value["url"], "https://example.com/not-a-comment");
        assert_eq!(value["escaped"], r#"a " then // still in the string"#);
        assert_eq!(value["n"], 1);
        // Comments become spaces, so offsets — and error positions — hold.
        assert_eq!(stripped.len(), src.len());
    }

    #[test]
    fn engram_args_are_found_in_every_config_flavor() {
        // Claude Code / Copilot: `mcpServers` with a separate `args` array.
        let claude = r#"{"mcpServers":{"engram":{"command":"engram",
            "args":["--db","/shared/engram.db","mcp"]}}}"#;
        assert_eq!(
            db_from_args(&json_engram_args(claude).expect("parsed")),
            Some(PathBuf::from("/shared/engram.db"))
        );

        // Opencode: `mcp`, command-as-array, and comments.
        let opencode = r#"{
          // servers
          "mcp": { "engram": { "type": "local",
            "command": ["engram", "--db", "/shared/engram.db", "mcp"] } }
        }"#;
        assert_eq!(
            db_from_args(&json_engram_args(&strip_jsonc_comments(opencode)).expect("parsed")),
            Some(PathBuf::from("/shared/engram.db"))
        );

        // Codex: TOML.
        let codex = "[mcp_servers.other]\nargs = [\"x\"]\n\n\
                     [mcp_servers.engram]\ncommand = \"engram\"\n\
                     args = [\"--db\", \"/shared/engram.db\", \"mcp\"]\n";
        assert_eq!(
            db_from_args(&toml_engram_args(codex).expect("parsed")),
            Some(PathBuf::from("/shared/engram.db"))
        );
    }

    #[test]
    fn a_config_without_engram_yields_nothing_rather_than_a_wrong_guess() {
        assert!(json_engram_args(r#"{"mcpServers":{"other":{"args":[]}}}"#).is_none());
        assert!(json_engram_args("not json at all").is_none());
        assert!(toml_engram_args("[mcp_servers.other]\nargs = [\"x\"]\n").is_none());
        // Registered, but with no --db: fall through rather than invent one.
        assert!(db_from_args(&["mcp".to_string()]).is_none());
    }

    #[test]
    fn a_harness_with_a_reader_declares_a_sessions_dir() {
        for entry in ALL {
            if matches!(entry.transcript, TranscriptSupport::Reader(_)) {
                assert!(
                    entry.sessions_dir.is_some(),
                    "{} has a reader but nowhere to read from",
                    entry.name
                );
            }
        }
    }
}

// Rust guideline compliant 2026-05-18
