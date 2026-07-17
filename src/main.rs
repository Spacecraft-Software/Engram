// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Engram — shared verbatim chat memory for multi-model LLM pipelines.
//! Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
//! https://Engram.SpacecraftSoftware.org/

mod cli;
mod error;
mod http;
mod mcp;
mod output;
mod store;
mod time;

use clap::Parser;
use cli::{Cli, Command};
use error::AppError;
use output::envelope::Response;
use output::mode::{resolve_mode, OutputMode};
use std::sync::{Arc, Mutex};
use store::Store;

fn main() {
    let cli = Cli::parse();
    let mode = resolve_mode(cli.json, cli.no_color);

    let store = match Store::open(&cli.db) {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            AppError::from(e).emit_to_stderr();
            std::process::exit(1);
        }
    };

    let exit_code = run(cli.command, store, mode);
    std::process::exit(exit_code);
}

fn run(command: Command, store: Arc<Mutex<Store>>, mode: OutputMode) -> i32 {
    match command {
        Command::Remember { agent, scope, role, content } => {
            let content = match content {
                Some(c) => c,
                None => {
                    use std::io::Read;
                    let mut buf = String::new();
                    if std::io::stdin().read_to_string(&mut buf).is_err() || buf.trim().is_empty() {
                        let err = AppError::new(
                            error::ErrorCode::InvalidArgument,
                            2,
                            "no content provided",
                            "pass content as an argument or pipe it via stdin",
                        );
                        emit_error(&err, mode);
                        return err.exit_code;
                    }
                    buf
                }
            };
            let guard = store.lock().expect("store lock poisoned");
            match guard.remember(&agent, &scope, &role, &content) {
                Ok(mem) => {
                    emit_ok(Response::new("engram memory remember", mem), mode);
                    0
                }
                Err(e) => {
                    let err = AppError::from(e);
                    emit_error(&err, mode);
                    err.exit_code
                }
            }
        }
        Command::Recall { scope, limit } => {
            let guard = store.lock().expect("store lock poisoned");
            match guard.recall(&scope, limit) {
                Ok(mems) => {
                    emit_ok(Response::new("engram memory recall", mems), mode);
                    0
                }
                Err(e) => {
                    let err = AppError::from(e);
                    emit_error(&err, mode);
                    err.exit_code
                }
            }
        }
        Command::Search { query, scope, limit } => {
            let guard = store.lock().expect("store lock poisoned");
            match guard.search(&query, scope.as_deref(), limit) {
                Ok(mems) => {
                    emit_ok(Response::new("engram memory search", mems), mode);
                    0
                }
                Err(e) => {
                    let err = AppError::from(e);
                    emit_error(&err, mode);
                    err.exit_code
                }
            }
        }
        Command::Mcp => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(e) = rt.block_on(mcp::run_stdio(store)) {
                eprintln!("{{\"error\":{{\"message\":\"{e}\"}}}}");
                return 1;
            }
            0
        }
        Command::Serve { addr } => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async {
                let app = http::router(store);
                let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind address");
                eprintln!("engram: serving on http://{addr} (local-only; no auth — do not expose beyond localhost)");
                axum::serve(listener, app).await.expect("serve");
            });
            0
        }
        Command::Schema => {
            let schema = schemars::schema_for!(store::Memory);
            println!("{}", serde_json::to_string_pretty(&schema).unwrap());
            0
        }
        Command::Describe => {
            let manifest = serde_json::json!({
                "tool": "engram",
                "version": env!("CARGO_PKG_VERSION"),
                "maintainer": "Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>",
                "website": "https://Engram.SpacecraftSoftware.org/",
                "commands": ["remember", "recall", "search", "save-chat", "mcp", "serve", "schema", "describe"],
                "transports": ["cli", "mcp-stdio", "http"],
                "storage": "sqlite+fts5, single shared file"
            });
            println!("{}", serde_json::to_string_pretty(&manifest).unwrap());
            0
        }
        Command::SaveChat { scope, file, model } => {
            let guard = store.lock().expect("store lock poisoned");
            match guard.recall(&scope, u32::MAX) {
                Ok(mems) => {
                    if mems.is_empty() {
                        let err = AppError::new(
                            error::ErrorCode::NotFound,
                            3,
                            format!("no memories found for scope '{}'", scope),
                            "verify the scope name is correct",
                        );
                        emit_error(&err, mode);
                        return err.exit_code;
                    }
                    
                    match handle_save_chat(&scope, mems, file, model) {
                        Ok(result) => {
                            emit_ok(Response::new("engram memory save-chat", result), mode);
                            0
                        }
                        Err(e) => {
                            let err = AppError::new(
                                error::ErrorCode::InternalError,
                                1,
                                format!("failed to save chat: {}", e),
                                "check filesystem permissions and directory configuration",
                            );
                            emit_error(&err, mode);
                            return err.exit_code;
                        }
                    }
                }
                Err(e) => {
                    let err = AppError::from(e);
                    emit_error(&err, mode);
                    err.exit_code
                }
            }
        }
    }
}

#[derive(serde::Serialize)]
struct SaveChatResult {
    scope: String,
    file_path: String,
    signed_by: String,
    messages_saved: usize,
}

fn handle_save_chat(
    scope: &str,
    mems: Vec<store::Memory>,
    custom_file: Option<std::path::PathBuf>,
    model: Option<String>,
) -> Result<SaveChatResult, Box<dyn std::error::Error>> {
    let current_dir = std::env::current_dir()?;
    
    // Ensure chat directory exists
    let chat_dir = current_dir.join("chat");
    std::fs::create_dir_all(&chat_dir)?;
    
    // Ensure chat/ is gitignored
    let gitignore_path = current_dir.join(".gitignore");
    let mut gitignore_content = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };
    
    if !gitignore_content.lines().any(|l| l.trim() == "chat" || l.trim() == "chat/") {
        if !gitignore_content.ends_with('\n') && !gitignore_content.is_empty() {
            gitignore_content.push('\n');
        }
        gitignore_content.push_str("chat/\n");
        std::fs::write(&gitignore_path, &gitignore_content)?;
    }

    // Resolve model name
    let model_name = model
        .or_else(|| std::env::var("MODEL").ok())
        .or_else(|| std::env::var("LLM_MODEL").ok())
        .or_else(|| std::env::var("AI_AGENT").ok())
        .or_else(|| std::env::var("AGENT").ok())
        .unwrap_or_else(|| "gemini-3.5-flash-high".to_string());

    // Resolve target file
    let target_file = match custom_file {
        Some(f) => f,
        None => {
            let timestamp = crate::time::now_iso8601().replace(':', "-");
            chat_dir.join(format!("{}.texi", timestamp))
        }
    };

    let exists = target_file.exists();
    let mut file_content = String::new();
    
    if exists {
        // Read existing content and strip trailing @bye to support clean append
        let existing = std::fs::read_to_string(&target_file)?;
        let trimmed = existing.trim();
        if trimmed.ends_with("@bye") {
            file_content = trimmed[..trimmed.len() - 4].to_string();
        } else {
            file_content = existing;
        }
    } else {
        // Write header
        file_content.push_str("\\input texinfo\n");
        file_content.push_str(&format!("@settitle Chat history for scope: {}\n\n", scope));
    }

    // Sign the block
    let sign_timestamp = crate::time::now_iso8601();
    file_content.push_str(&format!("@c Signed by: {} on {}\n", model_name, sign_timestamp));
    file_content.push_str(&format!("@chapter Chat history for scope: {}\n\n", scope));

    let num_saved = mems.len();
    for mem in mems {
        file_content.push_str(&format!("@section Message by {} ({}) at {}\n", mem.agent, mem.role, mem.created_at));
        file_content.push_str(&format!("{}\n\n", escape_texinfo(&mem.content)));
    }

    file_content.push_str("@bye\n");
    std::fs::write(&target_file, file_content)?;
    
    Ok(SaveChatResult {
        scope: scope.to_string(),
        file_path: target_file.to_string_lossy().to_string(),
        signed_by: model_name,
        messages_saved: num_saved,
    })
}

fn escape_texinfo(s: &str) -> String {
    let mut out = String::new();
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

// Rust guideline compliant 2026-05-18


fn emit_ok<T: serde::Serialize>(resp: Response<T>, mode: OutputMode) {
    if mode.is_machine() {
        println!("{}", serde_json::to_string(&resp).unwrap());
    } else {
        println!("{}", serde_json::to_string_pretty(&resp).unwrap());
    }
}

fn emit_error(err: &AppError, mode: OutputMode) {
    if mode.is_machine() {
        err.emit_to_stderr();
    } else {
        err.emit_human();
    }
}
