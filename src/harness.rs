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
    /// A Claude Code fork with its own config root.
    ///
    /// Renamed explicitly on both derives: kebab-casing the variant would give
    /// `open-claude`, but the fork calls itself `openclaude` and that is what
    /// [`HarnessSpec::name`] carries. The two must agree — the name is what
    /// `--harness` accepts and what every response serializes.
    #[serde(rename = "openclaude")]
    #[clap(name = "openclaude")]
    OpenClaude,
    Codex,
    Opencode,
    Kimi,
    /// Renamed on both derives for the same reason as `OpenClaude`:
    /// kebab-casing gives `vs-code`, but the editor is written `vscode` and
    /// that is what [`HarnessSpec::name`] carries.
    #[serde(rename = "vscode")]
    #[clap(name = "vscode")]
    VsCode,
    Cursor,
    Antigravity,
    Goose,
    CopilotCli,
    Qwen,
    Grok,
    /// Z.ai's Z Code. Renamed on both derives for the reason `OpenClaude` is:
    /// kebab-casing gives `z-code`, and the vendor writes it `zcode`.
    #[serde(rename = "zcode")]
    #[clap(name = "zcode")]
    ZCode,
    Deepcode,
    PoeCode,
    Kilo,
    Mimocode,
    Warp,
    Cline,
    Aichat,
    Bailian,
}

/// Which reader implementation handles a harness's transcripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderKind {
    ClaudeCode,
    Codex,
    /// Serves Opencode and Z.ai's Z Code, which share one schema.
    Opencode,
    Goose,
    CopilotCli,
    Qwen,
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
/// How a harness lets engram install something the user can invoke by name.
///
/// A bool was enough while every target was a markdown command file that either
/// did or did not carry frontmatter. Antigravity broke that: it has **no
/// slash-command directory at all** — its extension surface is skills, packaged
/// in plugins — so the shape of the artifact differs, not just its header. An
/// enum makes each surface carry exactly the fields it needs, and makes
/// "engram cannot install here" carry its reason instead of a shared sentence
/// that fits none of the harnesses it was applied to.
pub enum CommandSurface {
    /// Markdown command files in a home-relative directory.
    Markdown {
        /// Home-relative directory the harness loads user commands from.
        dir: &'static str,
        /// File-name pattern; `{name}` is the command's short name.
        file: &'static str,
        /// Whether the harness reads YAML frontmatter. Codex prompts are plain
        /// markdown and would render the block as literal text.
        frontmatter: bool,
    },
    /// A Gemini-family plugin directory. Skills inside it are discovered as
    /// `plugins/<plugin>/skills/<skill>/SKILL.md`, and expand the way a slash
    /// command does (`agy --disable-slash-commands` disables both).
    Plugin {
        /// Home-relative directory holding plugin subdirectories.
        dir: &'static str,
    },
    /// A bare skills directory the harness scans directly, one subdirectory
    /// per skill holding `SKILL.md`.
    ///
    /// Distinct from [`CommandSurface::Plugin`]: there is no plugin wrapper and
    /// no manifest, and discovery is automatic — nothing has to be registered
    /// in a config file engram does not own.
    Skill {
        /// Home-relative directory the harness scans for skills.
        dir: &'static str,
    },
    /// Nothing engram can write, and why not. The reason is per-harness
    /// because the reasons genuinely differ.
    None { detail: &'static str },
}

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
    /// How, if at all, engram can put a command in front of this harness's
    /// user. Reported plainly rather than papered over: only some harnesses
    /// can host one, and claiming otherwise would make `install` look broken
    /// on the rest.
    pub command_surface: CommandSurface,
    /// The harness's own command for exporting a conversation to a file.
    ///
    /// Recorded because it is the *answer* to a missing reader, not trivia. A
    /// harness engram cannot read is not a dead end when it can write its own
    /// export and `engram import` can read that — but only if the user is told
    /// which command to run, at the moment they hit the wall.
    pub export_command: Option<&'static str>,

    /// Extra home-relative directories this harness *loads* commands or skills
    /// from, which engram never writes to.
    ///
    /// Engram writes one target per harness, but several harnesses read more
    /// than one directory, and on a machine where those directories are
    /// symlinked together a command engram wrote for harness A shows up a
    /// second time in harness B. Recording what a harness reads is what lets
    /// `install` say so instead of leaving the user to notice the duplicate in
    /// their own slash-command list.
    pub also_scans: &'static [&'static str],
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
    /// A JSON file with an `mcpServers` (or `mcp`, or VS Code's `servers`)
    /// object at the top level.
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
        // Claude Code loads skills as well as commands.
        command_surface: CommandSurface::Markdown {
            dir: ".claude/commands",
            file: "engram-{name}.md",
            frontmatter: true,
        },
        export_command: Some("/export [file] (interactive slash command)"),
        also_scans: &[".claude/skills"],
        mcp_config: Some(McpConfigSource::Json(".claude.json")),
        hooks_config: Some(".claude/settings.json"),
    },
    HarnessSpec {
        id: Harness::OpenClaude,
        name: "openclaude",
        // A Claude Code fork: same config layout under its own root, so the
        // MCP registration lives in `~/.openclaude.json` (the `~/.claude.json`
        // analogue) and NOT in `~/.openclaude/settings.json`, which holds
        // env/model/hooks and no mcpServers block.
        probe: &[".openclaude", ".openclaude.json"],
        sessions_dir: Some(".openclaude/projects"),
        // The transcripts are Claude Code's format down to the record keys, so
        // the same reader serves both. Fork-specific record types are handled
        // in the reader's allowlist rather than by a second reader.
        transcript: TranscriptSupport::Reader(ReaderKind::ClaudeCode),
        command_surface: CommandSurface::Markdown {
            dir: ".openclaude/commands",
            file: "engram-{name}.md",
            frontmatter: true,
        },
        export_command: Some("/export [file] (interactive slash command)"),
        also_scans: &[".openclaude/skills"],
        mcp_config: Some(McpConfigSource::Json(".openclaude.json")),
        // The fork has a `hooks` key, but its shape is unverified against a
        // real run; `install --hooks` stays Claude-Code-only until it is.
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Codex,
        name: "codex",
        probe: &[".codex/config.toml", ".codex"],
        sessions_dir: Some(".codex/sessions"),
        transcript: TranscriptSupport::Reader(ReaderKind::Codex),
        // Codex 0.149 removed `~/.codex/prompts/` entirely --- the binary
        // contains no such string --- and moved to skills. Engram wrote three
        // files into that directory for a release and nothing ever read them.
        command_surface: CommandSurface::Skill {
            dir: ".codex/skills",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: Some(McpConfigSource::Toml(".codex/config.toml")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Opencode,
        name: "opencode",
        probe: &[".config/opencode/opencode.jsonc", ".config/opencode"],
        sessions_dir: Some(".local/share/opencode/opencode.db"),
        transcript: TranscriptSupport::Reader(ReaderKind::Opencode),
        command_surface: CommandSurface::Markdown {
            dir: ".config/opencode/command",
            file: "engram-{name}.md",
            frontmatter: true,
        },
        export_command: Some("opencode export [session] --sanitize"),
        also_scans: &[],
        mcp_config: Some(McpConfigSource::Jsonc(".config/opencode/opencode.jsonc")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Kimi,
        name: "kimi",
        // `~/.kimi` is the pre-migration root and still holds sessions; the
        // `.migrated-to-kimi-code` marker inside it points at `~/.kimi-code`,
        // which is where configuration lives now. Probing both means a user on
        // either side of that migration is detected.
        probe: &[".kimi-code", ".kimi"],
        sessions_dir: Some(".kimi/sessions"),
        transcript: TranscriptSupport::NotImplemented {
            detail: "kimi writes a line-oriented conversation engram could read — \
                     sessions/wd_<name>_<hash>/<session>/agents/main/wire.jsonl in the \
                     kimi-code layout, or the legacy sessions/<md5>/<session>/context.jsonl. \
                     The working directory is recoverable three ways: session_index.jsonl \
                     maps session to workDir, each session's state.json carries workDir, and \
                     the legacy hash is the MD5 of the absolute path. What is missing is a \
                     per-message timestamp, so ordering is positional",
        },
        command_surface: CommandSurface::Skill {
            dir: ".kimi-code/skills",
        },
        export_command: Some("kimi export [session] -o out.zip"),
        also_scans: &[],
        mcp_config: Some(McpConfigSource::Json(".kimi-code/mcp.json")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::VsCode,
        name: "vscode",
        probe: &[".config/Code"],
        // Chat history lives in workspaceStorage as editor state, not as a
        // transcript engram can read turn by turn.
        sessions_dir: None,
        transcript: TranscriptSupport::Unsupported {
            detail: "vs code keeps chat in workspaceStorage as editor state, not a \
                     per-session transcript file",
        },
        // Reusable prompt files: `<name>.prompt.md` in the profile's `prompts`
        // folder, invoked with `/<name>`. The extension and the `description`
        // frontmatter field are Microsoft-documented; the directory is created
        // by VS Code itself.
        command_surface: CommandSurface::Markdown {
            dir: ".config/Code/User/prompts",
            file: "engram-{name}.prompt.md",
            frontmatter: true,
        },
        export_command: None,
        also_scans: &[],
        mcp_config: Some(McpConfigSource::Json(".config/Code/User/mcp.json")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Cursor,
        name: "cursor",
        probe: &[".cursor"],
        sessions_dir: Some(".cursor/chats"),
        transcript: TranscriptSupport::NotImplemented {
            detail: "cursor stores chats under ~/.cursor/chats in a format engram has not \
                     surveyed",
        },
        // `.cursor/skills` and `SKILL.md` both appear in the cursor-agent
        // binary. `.cursor/skills-cursor` is the vendor's own bundle and is
        // not a user surface --- Grok's compatibility scanner filters those
        // same vendor defaults out.
        command_surface: CommandSurface::Skill {
            dir: ".cursor/skills",
        },
        export_command: None,
        also_scans: &[".cursor/commands"],
        mcp_config: Some(McpConfigSource::Json(".cursor/mcp.json")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Antigravity,
        name: "antigravity",
        probe: &[".gemini/antigravity", ".antigravity"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "antigravity writes a plain JSONL transcript at antigravity-cli/brain/<conversation>/.system_generated/logs/transcript.jsonl (54 on this machine) alongside its protobuf stores; a reader is not written yet. Conversations without a brain transcript stay unreadable — the .pb files and the BLOB columns of conversations/<uuid>.db are protobuf",
        },
        command_surface: CommandSurface::Plugin {
            dir: ".gemini/config/plugins",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: Some(McpConfigSource::Json(".gemini/antigravity/mcp_config.json")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Goose,
        name: "goose",
        probe: &[".config/goose/config.yaml", ".config/goose"],
        sessions_dir: Some(".local/share/goose/sessions/sessions.db"),
        transcript: TranscriptSupport::Reader(ReaderKind::Goose),
        command_surface: CommandSurface::None {
            detail: "goose has no user command directory engram has surveyed",
        },
        export_command: Some("goose session export --format markdown -o FILE"),
        also_scans: &[],
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::CopilotCli,
        name: "copilot-cli",
        probe: &[".copilot/mcp-config.json", ".copilot"],
        sessions_dir: Some(".copilot/session-store.db"),
        transcript: TranscriptSupport::Reader(ReaderKind::CopilotCli),
        command_surface: CommandSurface::None {
            detail: "copilot cli takes plugins, not loose command files, and installs \
                     them from a marketplace, a GitHub repository, or a git URL --- \
                     there is no user-writable directory engram can drop a command into",
        },
        export_command: Some("copilot --share=FILE"),
        also_scans: &[],
        mcp_config: Some(McpConfigSource::Json(".copilot/mcp-config.json")),
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Qwen,
        name: "qwen",
        probe: &[".qwen/settings.json", ".qwen"],
        sessions_dir: Some(".qwen/projects"),
        transcript: TranscriptSupport::Reader(ReaderKind::Qwen),
        // Verified against Qwen Code's own bundled documentation
        // (`docs/features/skills.md`): personal skills live in
        // `~/.qwen/skills/<name>/SKILL.md`.
        command_surface: CommandSurface::Skill {
            dir: ".qwen/skills",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: Some(McpConfigSource::Json(".qwen/settings.json")),
        hooks_config: None,
    },
    // ---------------------------------------------------------------- survey
    //
    // Installed on real machines and recorded so the table stops being silent
    // about them. Each entry states only what was actually probed: engram
    // writes nothing to any of them and reads none of them yet. An absent
    // entry is indistinguishable from an unexamined one, which is how three
    // harnesses came to carry claims that turned out to be false.
    HarnessSpec {
        id: Harness::Grok,
        name: "grok",
        probe: &[".grok/config.toml", ".grok"],
        sessions_dir: Some(".grok/sessions"),
        transcript: TranscriptSupport::NotImplemented {
            detail: "grok keeps a directory per working directory under .grok/sessions, named \
                     by percent-encoding the absolute path (%2Fspacecraft-software%2Fbravais). \
                     Unlike Claude Code's mangling that mapping is reversible, so a reader \
                     would not need the cwd supplied. 83 files on this machine; a reader is \
                     not written yet",
        },
        command_surface: CommandSurface::None {
            detail: "grok reads other vendors' directories on purpose — [compat.claude] in \
                     ~/.grok/config.toml scans ~/.claude/commands and ~/.claude/skills — so \
                     engram's Claude Code install already serves it, and a second copy would \
                     be shadowed by name deduplication rather than useful",
        },
        export_command: None,
        also_scans: &[".claude/commands", ".claude/skills", ".cursor/skills"],
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::ZCode,
        name: "zcode",
        probe: &[".zcode/cli", ".zcode"],
        sessions_dir: Some(".zcode/cli/db/db.sqlite"),
        transcript: TranscriptSupport::Reader(ReaderKind::Opencode),
        command_surface: CommandSurface::None {
            detail: "zcode's skills directory has not been surveyed for writability",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Deepcode,
        name: "deepcode",
        probe: &[".deepcode/projects", ".deepcode"],
        sessions_dir: Some(".deepcode/projects"),
        transcript: TranscriptSupport::NotImplemented {
            detail: "deepcode lays its sessions out like Claude Code — \
                     projects/<mangled-cwd>/<uuid>.jsonl — but the records do not carry \
                     Claude Code's keys (no type, uuid or cwd), so the existing reader would \
                     not serve it. One session on this machine; not surveyed further",
        },
        command_surface: CommandSurface::None {
            detail: "deepcode has no user command directory engram has surveyed",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::PoeCode,
        name: "poe-code",
        probe: &[".poe-code"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "poe-code wraps another agent rather than storing its own conversation — \
                     `npx poe-code wrap goose` appears in captured output — and its .poe-code \
                     holds credentials and logs. Its goose/ subdirectory is empty here",
        },
        command_surface: CommandSurface::None {
            detail: "poe-code has no user command directory engram has surveyed",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Kilo,
        name: "kilo",
        probe: &[".kilo"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "kilo's config root holds only its binary here — no conversation store \
                     was found in it. Its Markdown export is one of the formats \
                     `engram import` already reads",
        },
        command_surface: CommandSurface::None {
            detail: "kilo has no user command directory engram has surveyed",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Mimocode,
        name: "mimocode",
        probe: &[".mimocode"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "mimocode's config root holds only its binary here — no conversation \
                     store was found in it",
        },
        command_surface: CommandSurface::None {
            detail: "mimocode has no user command directory engram has surveyed",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Warp,
        name: "warp",
        probe: &[".warp"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "warp is a terminal with an agent rather than a CLI harness; .warp holds \
                     its TUI assets and no conversation store was found in it",
        },
        command_surface: CommandSurface::None {
            detail: "warp has no user command directory engram has surveyed",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Cline,
        name: "cline",
        probe: &[".cline"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "cline's config root holds a data directory and certificates here; no \
                     conversation store was found in it. Cline is primarily an editor \
                     extension, whose history lives in the host editor's storage",
        },
        command_surface: CommandSurface::None {
            detail: "cline has no user command directory engram has surveyed",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Aichat,
        name: "aichat",
        probe: &[".aichat"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "aichat's config root holds only a skills directory here; no conversation \
                     store was found in it",
        },
        command_surface: CommandSurface::None {
            detail: "aichat has no user command directory engram has surveyed",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: None,
        hooks_config: None,
    },
    HarnessSpec {
        id: Harness::Bailian,
        name: "bailian",
        probe: &[".bailian"],
        sessions_dir: None,
        transcript: TranscriptSupport::NotImplemented {
            detail: "bailian's config root holds configuration and a telemetry log here; no \
                     conversation store was found in it",
        },
        command_surface: CommandSurface::None {
            detail: "bailian has no user command directory engram has surveyed",
        },
        export_command: None,
        also_scans: &[],
        mcp_config: None,
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
    match spec.command_surface {
        CommandSurface::Markdown { dir, .. }
        | CommandSurface::Plugin { dir }
        | CommandSurface::Skill { dir } => in_home(dir),
        CommandSurface::None { .. } => None,
    }
}

/// Why engram cannot install a command here, when it cannot.
pub fn no_command_detail(spec: &HarnessSpec) -> Option<&'static str> {
    match spec.command_surface {
        CommandSurface::None { detail } => Some(detail),
        _ => None,
    }
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
        .or_else(|| value.get("mcp"))
        // VS Code's `~/.config/Code/User/mcp.json` uses `servers`.
        .or_else(|| value.get("servers"))?
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

    /// `--harness <v>` and the `harness` field in every response must be the
    /// same string. They come from different places — clap derives one from the
    /// enum variant, `HarnessSpec::name` hardcodes the other — so nothing but a
    /// test keeps them aligned. `OpenClaude` broke this on arrival: clap
    /// derived `open-claude` while the spec said `openclaude`.
    #[test]
    fn value_enum_names_match_spec_names() {
        use clap::ValueEnum;
        for spec in ALL {
            let variant = spec
                .id
                .to_possible_value()
                .expect("every harness is selectable");
            assert_eq!(
                variant.get_name(),
                spec.name,
                "--harness value and HarnessSpec::name disagree for {:?}",
                spec.id
            );
        }
    }

    /// Serialization must agree too: a response's `harness` field is read back
    /// by scripts and fed to `--harness`.
    #[test]
    fn serialized_harness_names_match_spec_names() {
        for spec in ALL {
            let json = serde_json::to_string(&spec.id).expect("serialize");
            assert_eq!(
                json.trim_matches('"'),
                spec.name,
                "serde name and HarnessSpec::name disagree for {:?}",
                spec.id
            );
        }
    }

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
