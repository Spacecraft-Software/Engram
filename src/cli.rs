// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
use clap::{Parser, Subcommand};

/// Engram — shared verbatim chat memory for multi-model LLM pipelines.
/// Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
/// https://Engram.SpacecraftSoftware.org/
#[derive(Parser)]
#[command(name = "engram", version, about, long_about = None)]
pub struct Cli {
    /// Path to the shared SQLite database. All agents should point at the
    /// same file (or the same `engram serve` instance) to actually share memory.
    #[arg(long, env = "ENGRAM_DB", default_value = "engram.db")]
    pub db: std::path::PathBuf,

    /// Alias for --format json.
    #[arg(long, global = true)]
    pub json: bool,

    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Store a verbatim message.
    Remember {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        scope: String,
        #[arg(long, default_value = "note")]
        role: String,
        /// Message content. Reads stdin if omitted.
        content: Option<String>,
    },
    /// Read back the last N messages for a scope, chronological order.
    Recall {
        #[arg(long)]
        scope: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Full-text search across stored memories.
    Search {
        query: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Run as an MCP server over stdio.
    Mcp,
    /// Run the HTTP API.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8420")]
        addr: String,
    },
    /// JSON Schema for engram's data types (SFRS introspection).
    Schema,
    /// Machine-readable capability manifest (SFRS introspection).
    Describe,
    /// Save a complete chat history for a scope to a Texinfo file.
    SaveChat {
        /// The scope of the chat to save.
        #[arg(long)]
        scope: String,

        /// Optional custom destination file path. Defaults to chat/<timestamp>.texi.
        #[arg(long)]
        file: Option<std::path::PathBuf>,

        /// Optional signature model/agent name (e.g. gpt-5.6-pro).
        #[arg(long)]
        model: Option<String>,
    },
}

// Rust guideline compliant 2026-05-18

