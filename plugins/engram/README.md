<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
SPDX-License-Identifier: GPL-3.0-or-later
-->

# The engram plugin

Slash commands that put engram in front of you inside an agent harness:
`/engram-save-chat`, `/engram-ingest`, `/engram-context`.

## This directory is the source of truth

`src/install.rs` embeds these command files with `include_str!` and renders
them into each harness's native command directory. There is deliberately no
second copy: the compiler enforces that the files exist, and a unit test
enforces that the embedded text still parses as frontmatter plus body.

Two placeholders are substituted at render time, and nothing else is:

| Placeholder | Becomes |
|---|---|
| `{{DB}}` | The database path, discovered from the harness's own MCP registration, then `$ENGRAM_DB`, then the resolved `--db` |
| `{{HARNESS}}` | The harness's engram name (`claude-code`, `codex`, …) |

`{{DB}}` matters more than it looks. Every harness on a developer's machine
usually registers engram against one shared database. A generated command that
omitted `--db` would fall back to clap's relative `engram.db` default and
quietly write somewhere else.

## Two ways to install

**`engram install`** copies rendered commands into `~/.claude/commands/`,
`~/.codex/prompts/`, and the equivalents. This is the only option for Codex
and Opencode, which have no plugin system at all.

**As a Claude Code plugin**, point a marketplace at this directory. You get
versioning and updates, and the commands resolve `${CLAUDE_PLUGIN_ROOT}`.

`engram install` **copies rather than symlinks**, on purpose. A symlink would
break the moment the repository moved, and would hand plugin semantics to a
non-plugin context where `${CLAUDE_PLUGIN_ROOT}` is undefined.

## Nix and home-manager

Skill directories on a home-manager system are symlinks into the read-only
Nix store, so nothing can install a *skill* at runtime. Engram therefore never
ships one — it ships commands, and command directories are writable.

If your home directory is declaratively managed, note that anything
`engram install` writes will be replaced by the next `home-manager switch`.
Reference this directory from your configuration instead;
`engram install --list` warns when it sees a Nix-store symlink under a harness
home.

## Hooks

There are none yet. When they land they will be opt-in twice over: the plugin
is installed deliberately, and `engram install` only writes the settings
equivalent under an explicit `--hooks`. A hook will run `ingest` and never
`save-chat` — writing a `.texi` into someone's repository at every session end
without being asked is not a default anyone should get.
